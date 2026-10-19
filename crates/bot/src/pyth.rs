//! Pyth circuit breaker. Pyth is not the price source: it only vetoes the bot's own fair value
//! when the two disagree by more than `max_dev_bps`. Design: `docs/design-oracle.md`.
//!
//! Choice: when Hermes is unreachable, rejects the request, or its price is older than
//! `max_stale_s`, the breaker reports `Unavailable` and the bot keeps running with a warning.
//! The breaker guards against the bot's own feeds going wrong. Halting on a third-party outage
//! would let a Hermes problem take the market down, and the on-chain staleness check and
//! move limit still bound the damage.

use std::time::Duration;

use serde_json::Value;
use tokio::sync::watch;
use tracing::warn;

use crate::BPS;

/// Pyth `Crypto.SOL/USD` feed id (verified against `/v2/price_feeds?query=SOL&asset_type=crypto`).
pub const SOL_USD_FEED_ID: &str =
    "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d";
pub const DEFAULT_HERMES_URL: &str = "https://hermes.pyth.network";
const POLL_EVERY: Duration = Duration::from_secs(2);
const WARN_EVERY: Duration = Duration::from_mins(1);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PythPrice {
    /// USD per SOL.
    pub price: f64,
    pub publish_time_s: u64,
}

/// Latest poll result: a price, or why there is none.
pub type PythSnapshot = Result<PythPrice, String>;

/// Parses a Hermes `/v2/updates/price/latest` response (`parsed[0].price`).
pub fn parse_hermes(body: &Value) -> Result<PythPrice, String> {
    let p = body
        .pointer("/parsed/0/price")
        .ok_or("no parsed price in response")?;
    let mantissa: f64 = p
        .get("price")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or("bad price field")?;
    let expo = p
        .get("expo")
        .and_then(Value::as_i64)
        .and_then(|e| i32::try_from(e).ok())
        .ok_or("bad expo field")?;
    let publish_time_s = p
        .get("publish_time")
        .and_then(Value::as_u64)
        .ok_or("bad publish_time field")?;
    let price = mantissa * 10_f64.powi(expo);
    if !price.is_finite() || price <= 0.0 {
        return Err(format!("non-positive price {price}"));
    }
    Ok(PythPrice {
        price,
        publish_time_s,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum Breaker {
    /// Within the threshold.
    Ok { dev_bps: f64 },
    /// |fair - pyth| above the threshold: halt.
    Tripped { dev_bps: f64 },
    /// No usable Pyth price: keep running, warn.
    Unavailable(String),
}

impl Breaker {
    pub fn trips(&self) -> bool {
        matches!(self, Self::Tripped { .. })
    }

    pub fn dev_bps(&self) -> Option<f64> {
        match self {
            Self::Ok { dev_bps } | Self::Tripped { dev_bps } => Some(*dev_bps),
            Self::Unavailable(_) => None,
        }
    }
}

/// Compares `fair` with the latest Pyth snapshot at `now_unix_s`.
pub fn check(
    fair: f64,
    snapshot: &PythSnapshot,
    now_unix_s: u64,
    max_dev_bps: f64,
    max_stale_s: u64,
) -> Breaker {
    let pyth = match snapshot {
        Ok(p) => p,
        Err(e) => return Breaker::Unavailable(e.clone()),
    };
    let age = now_unix_s.saturating_sub(pyth.publish_time_s);
    if age > max_stale_s {
        return Breaker::Unavailable(format!("pyth price is {age}s old"));
    }
    let dev_bps = (fair - pyth.price).abs() / pyth.price * BPS;
    if dev_bps > max_dev_bps {
        Breaker::Tripped { dev_bps }
    } else {
        Breaker::Ok { dev_bps }
    }
}

/// Polls Hermes every ~2 s and publishes the latest snapshot. Runs until the receiver is dropped.
pub async fn poll(base_url: String, tx: watch::Sender<PythSnapshot>) {
    let client = match http_client(Duration::from_secs(3)) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let url = latest_url(&base_url);
    let mut timer = tokio::time::interval(POLL_EVERY);
    let mut last_warn: Option<tokio::time::Instant> = None;
    loop {
        timer.tick().await;
        let snapshot = fetch(&client, &url).await;
        if let Err(e) = &snapshot
            && last_warn.is_none_or(|t| t.elapsed() >= WARN_EVERY)
        {
            warn!(error = %e, "pyth unavailable; circuit breaker is inactive, still running");
            last_warn = Some(tokio::time::Instant::now());
        }
        if tx.send(snapshot).is_err() {
            return;
        }
    }
}

/// One Hermes request against `base_url` (used by `setup-local` for the first price).
pub async fn fetch_once(base_url: &str) -> PythSnapshot {
    let client = http_client(Duration::from_secs(5))?;
    fetch(&client, &latest_url(base_url)).await
}

fn http_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("http client: {e}"))
}

fn latest_url(base_url: &str) -> String {
    format!(
        "{}/v2/updates/price/latest?ids[]={SOL_USD_FEED_ID}&parsed=true",
        base_url.trim_end_matches('/')
    )
}

async fn fetch(client: &reqwest::Client, url: &str) -> PythSnapshot {
    let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("hermes returned HTTP {status}"));
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    parse_hermes(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[allow(clippy::unnecessary_wraps)]
    fn snap(price: f64, t: u64) -> PythSnapshot {
        Ok(PythPrice {
            price,
            publish_time_s: t,
        })
    }

    #[test]
    fn parses_a_hermes_response() {
        let body = json!({"parsed": [{"id": "ef0d", "price": {
            "price": "20012345678", "conf": "1", "expo": -8, "publish_time": 1_790_000_000
        }}]});
        let p = parse_hermes(&body).unwrap();
        assert!((p.price - 200.123_456_78).abs() < 1e-9);
        assert_eq!(p.publish_time_s, 1_790_000_000);
        assert!(parse_hermes(&json!({"parsed": []})).is_err());
        let zero = json!({"parsed": [{"price": {"price": "0", "expo": -8, "publish_time": 1}}]});
        assert!(parse_hermes(&zero).is_err());
    }

    #[test]
    fn trips_only_above_the_threshold() {
        let s = snap(200.0, 1_000);
        // 50 bps of 200 is 1.0.
        assert!(matches!(
            check(200.9, &s, 1_001, 50.0, 30),
            Breaker::Ok { .. }
        ));
        assert!(matches!(
            check(201.1, &s, 1_001, 50.0, 30),
            Breaker::Tripped { .. }
        ));
        // Symmetric below.
        assert!(matches!(
            check(198.9, &s, 1_001, 50.0, 30),
            Breaker::Tripped { .. }
        ));
        let dev = check(200.5, &s, 1_001, 50.0, 30).dev_bps().unwrap();
        assert!((dev - 25.0).abs() < 1e-9);
    }

    #[test]
    fn stale_or_missing_pyth_never_trips() {
        let s = snap(200.0, 1_000);
        // Exactly 30 s old is still usable, 31 s is not.
        assert!(matches!(
            check(250.0, &s, 1_030, 50.0, 30),
            Breaker::Tripped { .. }
        ));
        let stale = check(250.0, &s, 1_031, 50.0, 30);
        assert!(matches!(stale, Breaker::Unavailable(_)) && !stale.trips());
        let down = check(250.0, &Err("HTTP 401".into()), 1_000, 50.0, 30);
        assert!(matches!(down, Breaker::Unavailable(_)) && !down.trips());
    }
}
