//! One resilient WebSocket task per feed: connect, subscribe, forward raw text, reconnect.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, interval, sleep, timeout};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::feeds::FeedSpec;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(30);
/// A session that stayed up this long resets the backoff.
const HEALTHY_AFTER: Duration = Duration::from_mins(1);

/// One raw text frame with its local receive time.
#[derive(Debug, Clone)]
pub struct RawMessage {
    pub feed: &'static str,
    pub recv_unix_ns: u64,
    pub text: String,
}

/// Lock-free per-feed counters, read by the stats reporter.
#[derive(Debug, Default)]
pub struct FeedStats {
    pub messages: AtomicU64,
    pub reconnects: AtomicU64,
    /// 0 until the first message arrives.
    pub last_msg_unix_ns: AtomicU64,
}

pub fn now_unix_ns() -> u64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Exponential backoff: `base`, doubling up to `cap`, with jitter in `[50%, 100%]` of the step.
#[derive(Debug, Clone)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    step: Duration,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self {
            base,
            cap,
            step: base,
        }
    }

    /// Next un-jittered delay; advances the schedule.
    pub fn next_step(&mut self) -> Duration {
        let current = self.step;
        self.step = (self.step * 2).min(self.cap);
        current
    }

    /// Next jittered delay, `jitter` in `[0, 1]` picks the point in `[step/2, step]`.
    pub fn next_delay_with(&mut self, jitter: f64) -> Duration {
        self.next_step().mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0))
    }

    pub fn next_delay(&mut self) -> Duration {
        self.next_delay_with(fastrand::f64())
    }

    pub fn reset(&mut self) {
        self.step = self.base;
    }
}

#[derive(Debug, thiserror::Error)]
enum SessionEnd {
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("websocket error: {0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("server closed the connection")]
    Closed,
    #[error("no message for {0:?}")]
    Silence(Duration),
    #[error("shutdown requested")]
    Shutdown,
    #[error("recorder channel closed")]
    ChannelClosed,
}

/// Run one feed until shutdown. Never returns an error: failures lead to a reconnect.
pub async fn run_feed(
    spec: FeedSpec,
    tx: mpsc::Sender<RawMessage>,
    stats: Arc<FeedStats>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = Backoff::new(BACKOFF_BASE, BACKOFF_CAP);
    loop {
        let started = Instant::now();
        let end = session(&spec, &tx, &stats, &mut shutdown).await;
        if matches!(end, SessionEnd::Shutdown | SessionEnd::ChannelClosed) {
            info!(feed = spec.name, reason = %end, "feed stopped");
            return;
        }
        if started.elapsed() >= HEALTHY_AFTER {
            backoff.reset();
        }
        let delay = backoff.next_delay();
        stats.reconnects.fetch_add(1, Ordering::Relaxed);
        warn!(feed = spec.name, reason = %end, retry_in = ?delay, "feed disconnected");
        tokio::select! {
            () = sleep(delay) => {}
            _ = shutdown.changed() => return,
        }
    }
}

async fn session(
    spec: &FeedSpec,
    tx: &mpsc::Sender<RawMessage>,
    stats: &FeedStats,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionEnd {
    match session_inner(spec, tx, stats, shutdown).await {
        Ok(end) | Err(end) => end,
    }
}

async fn session_inner(
    spec: &FeedSpec,
    tx: &mpsc::Sender<RawMessage>,
    stats: &FeedStats,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<SessionEnd, SessionEnd> {
    let connecting = timeout(CONNECT_TIMEOUT, connect_async(spec.url));
    let (mut ws, _) = tokio::select! {
        r = connecting => match r {
            Ok(Ok(ok)) => ok,
            Ok(Err(e)) => return Err(SessionEnd::Connect(e.to_string())),
            Err(_) => return Err(SessionEnd::Connect("timeout".to_owned())),
        },
        _ = shutdown.changed() => return Ok(SessionEnd::Shutdown),
    };
    for frame in spec.subscribe {
        ws.send(Message::text(*frame)).await?;
    }
    info!(feed = spec.name, "connected");

    let mut ping_timer = interval(spec.ping.map_or(Duration::from_hours(1), |p| p.every));
    ping_timer.tick().await; // the first tick fires immediately
    let silence_timer = sleep(spec.silence);
    tokio::pin!(silence_timer);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                // Best effort; the connection is dropped right after.
                let _ = ws.close(None).await;
                return Ok(SessionEnd::Shutdown);
            }
            () = &mut silence_timer => return Ok(SessionEnd::Silence(spec.silence)),
            _ = ping_timer.tick(), if spec.ping.is_some() => {
                if let Some(p) = spec.ping {
                    ws.send(Message::text(p.frame)).await?;
                }
            }
            frame = ws.next() => {
                let recv_unix_ns = now_unix_ns();
                let text = match frame {
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(Message::Text(t))) => t.as_str().to_owned(),
                    Some(Ok(Message::Binary(b))) => match String::from_utf8(b.into()) {
                        Ok(t) => t,
                        Err(_) => continue,
                    },
                    None | Some(Ok(Message::Close(_))) => return Ok(SessionEnd::Closed),
                    Some(Ok(_)) => continue,
                };
                silence_timer.as_mut().reset(Instant::now() + spec.silence);
                stats.messages.fetch_add(1, Ordering::Relaxed);
                stats.last_msg_unix_ns.store(recv_unix_ns, Ordering::Relaxed);
                let msg = RawMessage { feed: spec.name, recv_unix_ns, text };
                if tx.send(msg).await.is_err() {
                    return Ok(SessionEnd::ChannelClosed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn schedule_doubles_and_caps() {
        let mut b = Backoff::new(BACKOFF_BASE, BACKOFF_CAP);
        let steps: Vec<_> = (0..8).map(|_| b.next_step()).collect();
        let expect = [1, 2, 4, 8, 16, 30, 30, 30].map(secs);
        assert_eq!(steps, expect);
    }

    #[test]
    fn reset_restarts_schedule() {
        let mut b = Backoff::new(BACKOFF_BASE, BACKOFF_CAP);
        for _ in 0..4 {
            b.next_step();
        }
        b.reset();
        assert_eq!(b.next_step(), secs(1));
    }

    #[test]
    fn jitter_stays_within_bounds() {
        for j in [0.0, 0.3, 1.0, 5.0, -1.0] {
            let mut b = Backoff::new(BACKOFF_BASE, BACKOFF_CAP);
            for _ in 0..10 {
                let step = b.step;
                let d = b.next_delay_with(j);
                assert!(d >= step / 2 && d <= step, "{d:?} vs {step:?}");
                assert!(d <= BACKOFF_CAP);
            }
        }
        let mut b = Backoff::new(BACKOFF_BASE, BACKOFF_CAP);
        for _ in 0..50 {
            assert!(b.next_delay() <= BACKOFF_CAP);
        }
    }
}
