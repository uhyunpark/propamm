//! Static table of market-data feeds. Pure data: parsing lives in `parse.rs`.

use std::time::Duration;

/// App-level keepalive frame sent on an interval.
#[derive(Debug, Clone, Copy)]
pub struct Ping {
    pub frame: &'static str,
    pub every: Duration,
}

/// Everything needed to connect to and watch one feed.
#[derive(Debug, Clone, Copy)]
pub struct FeedSpec {
    pub name: &'static str,
    pub url: &'static str,
    /// Text frames sent right after connecting.
    pub subscribe: &'static [&'static str],
    pub ping: Option<Ping>,
    /// No message for this long means the connection is considered dead.
    pub silence: Duration,
}

const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}

pub const FEEDS: &[FeedSpec] = &[
    FeedSpec {
        name: "binance_spot_sol_bbo",
        url: "wss://data-stream.binance.vision/ws/solusdt@bookTicker",
        subscribe: &[],
        ping: None,
        silence: secs(10),
    },
    FeedSpec {
        name: "binance_spot_usdcusdt_bbo",
        url: "wss://data-stream.binance.vision/ws/usdcusdt@bookTicker",
        subscribe: &[],
        ping: None,
        silence: secs(60),
    },
    FeedSpec {
        name: "binance_perp_sol_bbo",
        url: "wss://fstream.binance.com/ws/solusdt@bookTicker",
        subscribe: &[],
        ping: None,
        silence: secs(10),
    },
    FeedSpec {
        name: "binance_perp_sol_mark",
        url: "wss://fstream.binance.com/market/ws/solusdt@markPrice@1s",
        subscribe: &[],
        ping: None,
        silence: secs(10),
    },
    FeedSpec {
        name: "binance_perp_sol_trades",
        // Only `/market/ws/` streams aggTrade and markPrice; `/ws/` and `/public/ws/` connect but stay silent.
        url: "wss://fstream.binance.com/market/ws/solusdt@aggTrade",
        subscribe: &[],
        ping: None,
        silence: secs(30),
    },
    FeedSpec {
        name: "coinbase_sol_usd",
        url: "wss://ws-feed.exchange.coinbase.com",
        subscribe: &[
            r#"{"type":"subscribe","product_ids":["SOL-USD"],"channels":["level2_batch","matches","heartbeat"]}"#,
        ],
        ping: None,
        silence: secs(5),
    },
    FeedSpec {
        name: "hyperliquid_sol",
        url: "wss://api.hyperliquid.xyz/ws",
        subscribe: &[
            r#"{"method":"subscribe","subscription":{"type":"bbo","coin":"SOL"}}"#,
            r#"{"method":"subscribe","subscription":{"type":"trades","coin":"SOL"}}"#,
        ],
        ping: Some(Ping {
            frame: r#"{"method":"ping"}"#,
            every: secs(30),
        }),
        silence: secs(30),
    },
    FeedSpec {
        name: "okx_perp_sol_bbo",
        url: "wss://ws.okx.com:8443/ws/v5/public",
        subscribe: &[
            r#"{"op":"subscribe","args":[{"channel":"bbo-tbt","instId":"SOL-USDT-SWAP"}]}"#,
        ],
        // Literal text; the reply is the literal text `pong`.
        ping: Some(Ping {
            frame: "ping",
            every: secs(20),
        }),
        silence: secs(10),
    },
    FeedSpec {
        name: "bybit_perp_sol_bbo",
        url: "wss://stream.bybit.com/v5/public/linear",
        subscribe: &[r#"{"op":"subscribe","args":["orderbook.1.SOLUSDT"]}"#],
        ping: Some(Ping {
            frame: r#"{"op":"ping"}"#,
            every: secs(20),
        }),
        silence: secs(10),
    },
];

/// Select feeds by name. An empty list selects all.
pub fn select(names: &[String]) -> Result<Vec<FeedSpec>, String> {
    if names.is_empty() {
        return Ok(FEEDS.to_vec());
    }
    names
        .iter()
        .map(|n| {
            FEEDS
                .iter()
                .find(|f| f.name == n)
                .copied()
                .ok_or_else(|| format!("unknown feed `{n}`"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn is_json(s: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(s).is_ok()
    }

    #[test]
    fn names_are_unique() {
        let names: HashSet<_> = FEEDS.iter().map(|f| f.name).collect();
        assert_eq!(names.len(), FEEDS.len());
    }

    #[test]
    fn urls_are_wss() {
        for f in FEEDS {
            assert!(f.url.starts_with("wss://") && f.url.len() > 6, "{}", f.name);
        }
    }

    #[test]
    fn subscribe_frames_are_json() {
        for f in FEEDS {
            for frame in f.subscribe {
                assert!(is_json(frame), "{}", f.name);
            }
        }
    }

    #[test]
    fn ping_frames_are_json_except_okx() {
        for f in FEEDS {
            if let Some(p) = f.ping {
                assert!(p.every > Duration::ZERO);
                assert_eq!(is_json(p.frame), f.name != "okx_perp_sol_bbo", "{}", f.name);
            }
        }
    }

    #[test]
    fn thresholds_are_positive() {
        for f in FEEDS {
            assert!(f.silence > Duration::ZERO, "{}", f.name);
        }
    }

    #[test]
    fn select_filters_and_rejects_unknown() {
        assert_eq!(select(&[]).unwrap().len(), FEEDS.len());
        assert_eq!(select(&["coinbase_sol_usd".to_owned()]).unwrap().len(), 1);
        assert!(select(&["nope".to_owned()]).is_err());
    }
}
