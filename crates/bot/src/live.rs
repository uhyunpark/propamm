//! Live feeds into the fair-value engine: the same connectors and parsers as `record` and
//! `eval`, driven in real time.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::connector::{FeedStats, RawMessage, run_feed};
use crate::fair::{FairEngine, FairParams, FairValue};
use crate::feeds;
use crate::parse::{Bbo, Event, Feed, Parser};

/// Feeds the v1 engine uses (weights above 0, plus the USDCUSDT conversion).
pub const FAIR_FEEDS: [&str; 3] = [
    "binance_spot_sol_bbo",
    "binance_spot_usdcusdt_bbo",
    "binance_perp_sol_bbo",
];
/// Extra feeds shown by the demo server. They never reach the fair-value engine.
pub const VENUE_FEEDS: [&str; 4] = [
    "okx_perp_sol_bbo",
    "bybit_perp_sol_bbo",
    "hyperliquid_sol",
    "coinbase_sol_usd",
];
/// A venue quote older than this is left out of the demo stream.
const VENUE_MAX_AGE_NS: u64 = 5 * crate::NS_PER_S;
const CHANNEL_CAPACITY: usize = 4_096;

/// Running connector tasks. Dropping the handles stops them on the next `stop`.
pub struct Feeds {
    pub rx: mpsc::Receiver<RawMessage>,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Feeds {
    pub fn spawn() -> anyhow::Result<Self> {
        Self::spawn_with(false)
    }

    /// `venues` adds the display-only feeds (`VENUE_FEEDS`).
    pub fn spawn_with(venues: bool) -> anyhow::Result<Self> {
        let mut names = FAIR_FEEDS.map(str::to_owned).to_vec();
        if venues {
            names.extend(VENUE_FEEDS.map(str::to_owned));
        }
        let specs = feeds::select(&names).map_err(anyhow::Error::msg)?;
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (stop, stop_rx) = watch::channel(false);
        let tasks = specs
            .into_iter()
            .map(|spec| {
                tokio::spawn(run_feed(
                    spec,
                    tx.clone(),
                    Arc::new(FeedStats::default()),
                    stop_rx.clone(),
                ))
            })
            .collect();
        Ok(Self { rx, stop, tasks })
    }

    pub async fn shutdown(self) {
        self.stop.send_replace(true);
        for t in self.tasks {
            let _ = tokio::time::timeout(Duration::from_secs(2), t).await;
        }
    }
}

/// Parsers plus the fair engine.
pub struct LiveFair {
    parsers: HashMap<&'static str, Parser>,
    engine: FairEngine,
    events: Vec<Event>,
    /// Latest BBO per source, for the demo stream.
    last_bbo: BTreeMap<Feed, Bbo>,
}

impl LiveFair {
    pub fn new() -> Self {
        Self::with_feeds(&FAIR_FEEDS)
    }

    /// Also parses the display-only venue feeds.
    pub fn with_venues() -> Self {
        let all: Vec<&'static str> = FAIR_FEEDS.into_iter().chain(VENUE_FEEDS).collect();
        Self::with_feeds(&all)
    }

    fn with_feeds(names: &[&'static str]) -> Self {
        let parsers = names
            .iter()
            .filter_map(|name| Parser::for_feed(name).map(|p| (*name, p)))
            .collect();
        Self {
            parsers,
            engine: FairEngine::new(FairParams::default()),
            events: Vec::new(),
            last_bbo: BTreeMap::new(),
        }
    }

    /// Parses one raw frame and feeds the resulting events to the engine.
    pub fn ingest(&mut self, msg: &RawMessage) {
        let Some(parser) = self.parsers.get_mut(msg.feed) else {
            return;
        };
        let value = match serde_json::from_str(&msg.text) {
            Ok(v) => v,
            Err(e) => {
                debug!(feed = msg.feed, error = %e, "unparseable frame");
                return;
            }
        };
        self.events.clear();
        if let Err(e) = parser.parse(msg.recv_unix_ns, &value, &mut self.events) {
            debug!(feed = msg.feed, error = %e, "parse error");
            return;
        }
        let for_engine = FAIR_FEEDS.contains(&msg.feed);
        for ev in &self.events {
            if let Event::Bbo(b) = ev {
                self.last_bbo.insert(b.feed, *b);
            }
            if for_engine {
                self.engine.update(ev);
            }
        }
    }

    /// Fresh venue quotes as `(key, bbo)`, keys per `docs/demo-contract.md`.
    pub fn venues(&self, now_ns: u64) -> Vec<(&'static str, Bbo)> {
        self.last_bbo
            .values()
            .filter(|b| now_ns.saturating_sub(b.recv_ns) <= VENUE_MAX_AGE_NS)
            .filter_map(|b| Some((venue_key(b.feed)?, *b)))
            .collect()
    }

    pub fn fair(&mut self, now_ns: u64) -> Option<FairValue> {
        self.engine.fair(now_ns)
    }
}

/// Key of a source in the demo stream; `None` for sources that are not shown.
fn venue_key(feed: Feed) -> Option<&'static str> {
    match feed {
        Feed::BinancePerp => Some("binance_perp"),
        Feed::BinanceSpot => Some("binance_spot"),
        Feed::Okx => Some("okx_perp"),
        Feed::Bybit => Some("bybit_perp"),
        Feed::Hyperliquid => Some("hyperliquid"),
        Feed::Coinbase => Some("coinbase"),
        Feed::BinanceUsdcUsdt => None,
    }
}

impl Default for LiveFair {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(feed: &'static str, recv_unix_ns: u64, text: &str) -> RawMessage {
        RawMessage {
            feed,
            recv_unix_ns,
            text: text.to_owned(),
        }
    }

    #[test]
    fn venue_quotes_are_tracked_and_age_out() {
        let mut live = LiveFair::with_venues();
        let t0 = 1_000 * crate::NS_PER_S;
        live.ingest(&raw(
            "binance_spot_sol_bbo",
            t0,
            r#"{"b":"121.41","B":"2","a":"121.43","A":"3"}"#,
        ));
        live.ingest(&raw(
            "okx_perp_sol_bbo",
            t0,
            r#"{"data":[{"asks":[["121.40","1","0","1"]],"bids":[["121.38","1","0","1"]],"ts":"1"}]}"#,
        ));
        let venues = live.venues(t0 + crate::NS_PER_S);
        let keys: Vec<_> = venues.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, ["binance_spot", "okx_perp"]);
        assert!((venues[0].1.mid() - 121.42).abs() < 1e-9);
        // Venue-only feeds never reach the fair engine, and old quotes drop out.
        assert!(live.fair(t0).is_none());
        assert!(live.venues(t0 + 10 * crate::NS_PER_S).is_empty());
        // The plain constructor ignores venue feeds.
        let mut plain = LiveFair::new();
        plain.ingest(&raw(
            "okx_perp_sol_bbo",
            t0,
            r#"{"data":[{"asks":[["1","1","0","1"]],"bids":[["1","1","0","1"]],"ts":"1"}]}"#,
        ));
        assert!(plain.venues(t0).is_empty());
    }
}
