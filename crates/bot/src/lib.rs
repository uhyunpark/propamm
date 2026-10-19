//! Feed collector, raw recorder, feed parsers, fair-value engine and offline evaluation,
//! `update_price` publisher and local setup/admin/trader tools.
#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod chain;
pub mod config;
pub mod connector;
pub mod eval;
pub mod fair;
pub mod feeds;
pub mod live;
pub mod parse;
pub mod publish;
pub mod pyth;
pub mod recorder;
pub mod replay;
pub mod run;
pub mod serve;
pub mod subscribe;
pub mod tools;

pub(crate) const NS_PER_MS: u64 = 1_000_000;
pub(crate) const NS_PER_S: u64 = 1_000_000_000;
pub(crate) const BPS: f64 = 1e4;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::{mpsc, watch};
use tokio::time::interval;
use tracing::info;

use connector::{FeedStats, now_unix_ns, run_feed};
use feeds::FeedSpec;
use recorder::Recorder;

const CHANNEL_CAPACITY: usize = 16_384;
const STATS_EVERY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct RecordOptions {
    pub out: PathBuf,
    /// Feed names; empty selects all.
    pub feeds: Vec<String>,
    /// Stop after this long; `None` runs until a signal.
    pub duration: Option<Duration>,
}

/// Record the selected feeds until Ctrl-C, SIGTERM or the duration elapses.
pub async fn record(opts: RecordOptions) -> anyhow::Result<()> {
    // rustls needs one process-wide crypto provider; Err means one is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let specs = feeds::select(&opts.feeds).map_err(anyhow::Error::msg)?;
    std::fs::create_dir_all(&opts.out)
        .with_context(|| format!("creating {}", opts.out.display()))?;
    info!(out = %opts.out.display(), feeds = specs.len(), "recording");

    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (stop_tx, stop_rx) = watch::channel(false);
    let recorder = tokio::spawn(recorder::run(rx, Recorder::new(&opts.out)));

    let mut all_stats = Vec::new();
    let mut feed_tasks = Vec::new();
    for spec in specs {
        let stats = Arc::new(FeedStats::default());
        all_stats.push((spec, Arc::clone(&stats)));
        feed_tasks.push(tokio::spawn(run_feed(
            spec,
            tx.clone(),
            stats,
            stop_rx.clone(),
        )));
    }
    drop(tx); // the recorder ends once every feed task has dropped its sender
    let reporter = tokio::spawn(report_stats(all_stats));

    tokio::select! {
        () = shutdown_signal() => info!("signal received, shutting down"),
        () = async {
            match opts.duration {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending().await,
            }
        } => info!("duration elapsed, shutting down"),
    }

    stop_tx.send_replace(true);
    reporter.abort();
    // Join everything before returning so no zstd encoder is dropped unfinished.
    let mut failed = false;
    for t in feed_tasks {
        failed |= t.await.is_err();
    }
    failed |= recorder.await.is_err();
    anyhow::ensure!(!failed, "a task panicked");
    info!("all files finished");
    Ok(())
}

async fn report_stats(feeds: Vec<(FeedSpec, Arc<FeedStats>)>) {
    let mut timer = interval(STATS_EVERY);
    timer.tick().await;
    let mut prev = vec![0_u64; feeds.len()];
    loop {
        timer.tick().await;
        let now = now_unix_ns();
        for ((spec, s), prev) in feeds.iter().zip(prev.iter_mut()) {
            let total = s.messages.load(Ordering::Relaxed);
            let window = total - *prev;
            *prev = total;
            let last = s.last_msg_unix_ns.load(Ordering::Relaxed);
            let age_ms = if last == 0 {
                None
            } else {
                Some(now.saturating_sub(last) / 1_000_000)
            };
            // Precision loss is irrelevant for a log rate.
            #[allow(clippy::cast_precision_loss)]
            let rate = window as f64 / STATS_EVERY.as_secs_f64();
            info!(
                feed = spec.name,
                msgs = window,
                rate = format_args!("{rate:.1}/s"),
                last_age_ms = ?age_ms,
                reconnects = s.reconnects.load(Ordering::Relaxed),
                "stats"
            );
        }
    }
}

#[cfg(unix)]
pub(crate) async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(not(unix))]
pub(crate) async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
