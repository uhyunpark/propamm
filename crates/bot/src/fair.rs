//! Fair-value engine v1. Design: `docs/design-oracle.md`.
//!
//! Every price source is turned into one unit, SOLUSDC spot: size-weighted mid, basis-adjusted for
//! perps, divided by an EMA of USDCUSDT. Stale quotes are dropped, outliers rejected (with 3+
//! candidates), and the survivors averaged. Coinbase is the held-out reference and never an input.

use std::time::Duration;

use crate::parse::{Bbo, Event, Feed};
use crate::{BPS, NS_PER_MS};

/// Per-source weights. A weight of 0 disables the source (its basis is still tracked).
#[derive(Debug, Clone, Copy)]
pub struct Weights {
    pub perp: f64,
    pub spot: f64,
    pub hyperliquid: f64,
    pub okx: f64,
    pub bybit: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct FairParams {
    pub weights: Weights,
    pub basis_half_life_s: f64,
    pub usdc_half_life_s: f64,
    /// Both books must be younger than this for a basis update.
    pub basis_fresh_ms: u64,
    /// Max quote age for fast feeds (perp, OKX, Bybit).
    pub max_age_fast_ms: u64,
    /// Max quote age for slow feeds (spot bookTicker only publishes on change; Hyperliquid is block-paced).
    pub max_age_slow_ms: u64,
    pub imbalance_clip: (f64, f64),
    pub outlier_k: f64,
    pub outlier_c: f64,
    pub dispersion_half_life_s: f64,
    pub usdc_halt_bps: f64,
    pub min_sources: usize,
}

impl Default for FairParams {
    fn default() -> Self {
        Self {
            weights: Weights {
                perp: 2.0,
                spot: 1.0,
                hyperliquid: 0.0,
                okx: 0.0,
                bybit: 0.0,
            },
            basis_half_life_s: 30.0,
            usdc_half_life_s: 10.0,
            basis_fresh_ms: 500,
            max_age_fast_ms: 500,
            max_age_slow_ms: 2_000,
            imbalance_clip: (0.1, 0.9),
            outlier_k: 3.0,
            outlier_c: 1.0,
            dispersion_half_life_s: 10.0,
            usdc_halt_bps: 30.0,
            min_sources: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FairValue {
    pub ts_ns: u64,
    /// SOLUSDC.
    pub fair: f64,
    /// Weighted standard deviation of the surviving sources, in bps of `fair`.
    pub conf_bps: f64,
    pub live_sources: usize,
}

/// Exponential average over irregular time steps.
#[derive(Debug, Clone, Copy, Default)]
struct Ema {
    value: Option<f64>,
    last_ns: u64,
}

impl Ema {
    fn update(&mut self, x: f64, now_ns: u64, half_life_s: f64) {
        self.value = Some(match self.value {
            None => x,
            Some(v) => {
                let dt_s = Duration::from_nanos(now_ns.saturating_sub(self.last_ns)).as_secs_f64();
                let alpha = 1.0 - (-std::f64::consts::LN_2 * dt_s / half_life_s).exp();
                v + alpha * (x - v)
            }
        });
        self.last_ns = now_ns;
    }
}

#[derive(Debug, Clone, Copy)]
struct Quote {
    recv_ns: u64,
    wmid: f64,
    spread: f64,
}

#[derive(Debug, Clone, Copy, Default)]
struct Source {
    quote: Option<Quote>,
    /// Perp / spot - 1. Unused for the spot source.
    basis: Ema,
}

const SOURCES: [Feed; 5] = [
    Feed::BinanceSpot,
    Feed::BinancePerp,
    Feed::Hyperliquid,
    Feed::Okx,
    Feed::Bybit,
];

/// Index of the spot source in `SOURCES`; every other source is a perp priced against it.
const SPOT: usize = 0;

fn slot(feed: Feed) -> Option<usize> {
    SOURCES.iter().position(|f| *f == feed)
}

#[derive(Debug)]
pub struct FairEngine {
    params: FairParams,
    sources: [Source; SOURCES.len()],
    usdc: Ema,
    usdc_last: Option<f64>,
    dispersion: Ema,
}

impl FairEngine {
    pub fn new(params: FairParams) -> Self {
        Self {
            params,
            sources: [Source::default(); SOURCES.len()],
            usdc: Ema::default(),
            usdc_last: None,
            dispersion: Ema::default(),
        }
    }

    /// Feed one event. Only BBOs of input feeds change state.
    pub fn update(&mut self, ev: &Event) {
        let Event::Bbo(b) = ev else { return };
        if b.feed == Feed::BinanceUsdcUsdt {
            self.usdc
                .update(b.mid(), b.recv_ns, self.params.usdc_half_life_s);
            self.usdc_last = Some(b.mid());
            return;
        }
        let Some(i) = slot(b.feed) else { return };
        self.sources[i].quote = Some(Quote {
            recv_ns: b.recv_ns,
            wmid: self.microprice(b),
            spread: b.ask - b.bid,
        });
        self.update_basis(b.recv_ns);
    }

    fn microprice(&self, b: &Bbo) -> f64 {
        let total = b.bid_size + b.ask_size;
        if total <= 0.0 {
            return b.mid();
        }
        let (lo, hi) = self.params.imbalance_clip;
        let imbalance = (b.bid_size / total).clamp(lo, hi);
        b.bid + imbalance * (b.ask - b.bid)
    }

    /// Refresh every perp basis whose book and the spot book are both fresh.
    fn update_basis(&mut self, now_ns: u64) {
        let fresh_ns = self.params.basis_fresh_ms * NS_PER_MS;
        let Some(spot) = self.sources[SPOT]
            .quote
            .filter(|q| now_ns.saturating_sub(q.recv_ns) < fresh_ns)
        else {
            return;
        };
        let half_life = self.params.basis_half_life_s;
        for s in &mut self.sources[SPOT + 1..] {
            if let Some(q) = s
                .quote
                .filter(|q| now_ns.saturating_sub(q.recv_ns) < fresh_ns)
            {
                s.basis.update(q.wmid / spot.wmid - 1.0, now_ns, half_life);
            }
        }
    }

    fn weight(&self, i: usize) -> f64 {
        let w = &self.params.weights;
        [w.spot, w.perp, w.hyperliquid, w.okx, w.bybit][i]
    }

    /// Basis of source `i` against spot (0 for spot itself), once known.
    fn basis(&self, i: usize) -> Option<f64> {
        if i == SPOT {
            Some(0.0)
        } else {
            self.sources[i].basis.value
        }
    }

    pub fn max_age_ns(&self, feed: Feed) -> u64 {
        let ms = match feed {
            Feed::BinanceSpot | Feed::Hyperliquid => self.params.max_age_slow_ms,
            _ => self.params.max_age_fast_ms,
        };
        ms * NS_PER_MS
    }

    /// Smoothed USDCUSDT (USDT per USDC), if seen.
    pub fn usdc(&self) -> Option<f64> {
        self.usdc.value
    }

    /// Convert a price quoted on `feed` (USDT perp print or spot) to SOLUSDC fair units.
    pub fn to_fair_units(&self, feed: Feed, price: f64) -> Option<f64> {
        Some(price / (1.0 + self.basis(slot(feed)?)?) / self.usdc()?)
    }

    /// Fair value as of `now_ns`, or `None` when halted.
    pub fn fair(&mut self, now_ns: u64) -> Option<FairValue> {
        let usdc = self.usdc()?;
        if (self.usdc_last? - 1.0).abs() * BPS > self.params.usdc_halt_bps {
            return None;
        }
        // (price, spread, weight) per live source, in SOLUSDC.
        let mut cands: Vec<(f64, f64, f64)> = Vec::with_capacity(SOURCES.len());
        for (i, feed) in SOURCES.iter().enumerate() {
            let w = self.weight(i);
            let Some(q) = self.sources[i]
                .quote
                .filter(|q| w > 0.0 && now_ns.saturating_sub(q.recv_ns) <= self.max_age_ns(*feed))
            else {
                continue;
            };
            if let Some(b) = self.basis(i) {
                let scale = 1.0 / (1.0 + b) / usdc;
                cands.push((q.wmid * scale, q.spread * scale, w));
            }
        }
        if cands.len() >= 3 {
            self.reject_outliers(&mut cands);
        }
        if cands.len() < self.params.min_sources {
            return None;
        }
        let wsum: f64 = cands.iter().map(|c| c.2).sum();
        let fair = cands.iter().map(|c| c.0 * c.2).sum::<f64>() / wsum;
        let var = cands
            .iter()
            .map(|c| c.2 * (c.0 - fair).powi(2))
            .sum::<f64>()
            / wsum;
        self.dispersion
            .update(var.sqrt(), now_ns, self.params.dispersion_half_life_s);
        Some(FairValue {
            ts_ns: now_ns,
            fair,
            conf_bps: var.sqrt() / fair * BPS,
            live_sources: cands.len(),
        })
    }

    /// Drop candidates with |p - median| > k * max(spread, c * sigma). Sigma is the smoothed
    /// dispersion of past survivors. Needs 3+ candidates: with two, both sit equally far from the median.
    fn reject_outliers(&self, cands: &mut Vec<(f64, f64, f64)>) {
        let mut prices: Vec<f64> = cands.iter().map(|c| c.0).collect();
        prices.sort_by(f64::total_cmp);
        let n = prices.len();
        let median = if n % 2 == 1 {
            prices[n / 2]
        } else {
            f64::midpoint(prices[n / 2 - 1], prices[n / 2])
        };
        let sigma = self.dispersion.value.unwrap_or(0.0);
        let (k, c) = (self.params.outlier_k, self.params.outlier_c);
        cands.retain(|&(p, spread, _)| (p - median).abs() <= k * spread.max(c * sigma));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = NS_PER_MS;

    fn quote(feed: Feed, t_ms: u64, bid: f64, ask: f64) -> Event {
        Event::Bbo(Bbo {
            feed,
            recv_ns: t_ms * MS,
            exch_ts_ms: None,
            bid,
            bid_size: 10.0,
            ask,
            ask_size: 10.0,
        })
    }

    /// Engine with USDC = 1 and spot/perp seeded at `t_ms`.
    fn seeded(params: FairParams, spot: f64, perp: f64, t_ms: u64) -> FairEngine {
        let mut e = FairEngine::new(params);
        e.update(&quote(Feed::BinanceUsdcUsdt, t_ms, 1.0, 1.0));
        e.update(&quote(Feed::BinanceSpot, t_ms, spot - 0.005, spot + 0.005));
        e.update(&quote(Feed::BinancePerp, t_ms, perp - 0.005, perp + 0.005));
        e
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn microprice_clips_imbalance() {
        let e = FairEngine::new(FairParams::default());
        let mut b = Bbo {
            feed: Feed::BinancePerp,
            recv_ns: 0,
            exch_ts_ms: None,
            bid: 100.0,
            bid_size: 1.0,
            ask: 101.0,
            ask_size: 1.0,
        };
        assert!(close(e.microprice(&b), 100.5));
        b.bid_size = 1000.0; // raw imbalance 0.999 clipped to 0.9
        assert!(close(e.microprice(&b), 100.9));
        b.bid_size = 0.0; // raw 0 clipped to 0.1
        assert!(close(e.microprice(&b), 100.1));
    }

    #[test]
    fn converts_usdt_perp_to_usdc_spot() {
        let mut e = FairEngine::new(FairParams::default());
        e.update(&quote(Feed::BinanceUsdcUsdt, 0, 1.0004, 1.0004));
        e.update(&quote(Feed::BinanceSpot, 0, 100.035, 100.045));
        e.update(&quote(Feed::BinancePerp, 0, 100.135, 100.145)); // +10 bps basis
        let f = e.fair(0).unwrap();
        // spot 100.04 USDT / 1.0004 = 100.0 USDC; the perp adjusted for its basis agrees.
        assert!(close(f.fair, 100.04 / 1.0004), "{}", f.fair);
        assert_eq!(f.live_sources, 2);
        assert!(f.conf_bps < 1e-6);
        assert!(close(
            e.to_fair_units(Feed::BinancePerp, 100.14).unwrap(),
            100.04 / 1.0004
        ));
    }

    #[test]
    fn basis_updates_only_when_both_books_fresh() {
        let mut e = seeded(FairParams::default(), 100.0, 100.1, 0);
        let b0 = e.sources[1].basis.value.unwrap();
        assert!(close(b0, 100.1 / 100.0 - 1.0));
        // Perp moves 1 s later; spot (last seen at 0) is no longer fresh: basis stays put.
        e.update(&quote(Feed::BinancePerp, 1_000, 100.4, 100.41));
        assert!(close(e.sources[1].basis.value.unwrap(), b0));
        // Spot refreshes, perp still fresh from <500 ms ago: basis moves toward the new gap.
        e.update(&quote(Feed::BinanceSpot, 1_100, 100.0, 100.01));
        assert!(e.sources[1].basis.value.unwrap() > b0);
    }

    #[test]
    fn stale_quotes_are_dropped() {
        let mut e = seeded(FairParams::default(), 100.0, 100.0, 0);
        assert!(e.fair(400 * MS).is_some());
        // Perp (fast, 500 ms) is stale at 600 ms; spot (slow, 2 s) is alone -> halt.
        assert!(e.fair(600 * MS).is_none());
        // Refreshed perp with a 1.5 s old spot: both live.
        e.update(&quote(Feed::BinancePerp, 1_500, 100.0, 100.01));
        assert_eq!(e.fair(1_500 * MS).unwrap().live_sources, 2);
        assert!(e.fair(2_500 * MS).is_none()); // spot older than 2 s, perp older than 500 ms
    }

    #[test]
    fn halts_on_usdc_depeg_or_missing_usdc() {
        let mut e = FairEngine::new(FairParams::default());
        e.update(&quote(Feed::BinanceSpot, 0, 100.0, 100.01));
        e.update(&quote(Feed::BinancePerp, 0, 100.0, 100.01));
        assert!(e.fair(0).is_none()); // no USDCUSDT yet
        e.update(&quote(Feed::BinanceUsdcUsdt, 0, 1.0019, 1.0021)); // 20 bps
        assert!(e.fair(0).is_some());
        e.update(&quote(Feed::BinanceUsdcUsdt, 1, 1.0034, 1.0036)); // 35 bps
        assert!(e.fair(MS).is_none());
        e.update(&quote(Feed::BinanceUsdcUsdt, 2, 1.0, 1.0));
        assert!(e.fair(2 * MS).is_some());
    }

    #[test]
    fn rejects_outlier_with_three_or_more_sources() {
        let mut p = FairParams::default();
        p.weights.okx = 1.0;
        p.weights.bybit = 1.0;
        let mut e = seeded(p, 100.0, 100.0, 0);
        e.update(&quote(Feed::Okx, 0, 99.995, 100.005));
        e.update(&quote(Feed::Bybit, 0, 99.995, 100.005));
        assert_eq!(e.fair(0).unwrap().live_sources, 4);
        // Bybit jumps 1% while the others hold.
        e.update(&quote(Feed::BinanceSpot, 100, 99.995, 100.005));
        e.update(&quote(Feed::BinancePerp, 100, 99.995, 100.005));
        e.update(&quote(Feed::Okx, 100, 99.995, 100.005));
        e.update(&quote(Feed::Bybit, 100, 100.995, 101.005));
        let f = e.fair(100 * MS).unwrap();
        assert_eq!(f.live_sources, 3);
        assert!((f.fair - 100.0).abs() < 0.02, "{}", f.fair);
    }

    #[test]
    fn two_sources_keep_both_even_when_far_apart() {
        let mut e = seeded(FairParams::default(), 100.0, 100.0, 0);
        e.update(&quote(Feed::BinanceSpot, 100, 101.0, 101.01));
        e.update(&quote(Feed::BinancePerp, 100, 99.0, 99.01));
        let f = e.fair(100 * MS).unwrap();
        assert_eq!(f.live_sources, 2);
        assert!(f.conf_bps > 50.0); // dispersion is visible in conf
    }

    #[test]
    fn weights_zero_excludes_source() {
        // Hyperliquid has weight 0 in v1: it must not count as a live source.
        let mut e = seeded(FairParams::default(), 100.0, 100.0, 0);
        e.update(&quote(Feed::Hyperliquid, 0, 50.0, 50.01));
        let f = e.fair(0).unwrap();
        assert_eq!(f.live_sources, 2);
        assert!(close(f.fair, 100.0));
    }

    #[test]
    fn coinbase_is_never_an_input() {
        let mut e = seeded(FairParams::default(), 100.0, 100.0, 0);
        e.update(&quote(Feed::Coinbase, 0, 10.0, 10.01));
        assert!(close(e.fair(0).unwrap().fair, 100.0));
    }

    #[test]
    fn ema_half_life() {
        let mut ema = Ema::default();
        ema.update(0.0, 0, 10.0);
        ema.update(1.0, 10_000_000_000, 10.0); // one half-life
        assert!(close(ema.value.unwrap(), 0.5));
    }
}
