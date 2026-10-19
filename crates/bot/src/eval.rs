//! Offline evaluation of fair-value candidates against held-out Coinbase.
//!
//! One replay pass feeds every candidate engine and records, at each Binance perp or spot BBO,
//! the candidates' fair values plus the Coinbase reference mid series. The metrics are then pure
//! functions over those series. Method: `docs/design-oracle.md`.

// Counts, nanosecond spans and percentile ranks convert between integers and f64 here. All values
// are far below 2^52, so the casts are exact or harmless.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;

use crate::fair::{FairEngine, FairParams, Weights};
use crate::parse::{Bbo, Event, Feed, Parser, TakerSide};
use crate::replay::Replay;
use crate::{BPS, NS_PER_MS, NS_PER_S};

const HORIZONS_MS: [u64; 4] = [0, 100, 1_000, 5_000];
const MARKOUT_HORIZONS_MS: [u64; 3] = [0, 1_000, 5_000];
/// A reference mid older than this at the target time is treated as missing.
const REF_MAX_AGE_NS: u64 = 2 * NS_PER_S;
/// A published fair value older than this is not a live quote.
const QUOTE_MAX_AGE_NS: u64 = NS_PER_S;
const POST_END_SLACK_NS: u64 = 10 * NS_PER_S;
/// Weight of each variant source relative to perp 2 / spot 1.
const VARIANT_WEIGHT: f64 = 0.25;

const LAG_GRID_NS: u64 = 100 * NS_PER_MS;
const LAG_WINDOW_NS: u64 = 500 * NS_PER_MS;
const LAG_RANGE_MS: std::ops::RangeInclusive<i64> = -300..=1_500;
const LAG_STEP_MS: usize = 50;

// Candidate indices. 0..=3 are the core set that defines the common sample mask.
const PERP_RAW: usize = 0;
const SPOT_CONV: usize = 1;
const EQUAL_WT: usize = 2;
const V1: usize = 3;
const NAMES: [&str; 8] = [
    "perp_raw",
    "spot_conv",
    "equal_wt",
    "v1",
    "v1+hl",
    "v1+okx",
    "v1+bybit",
    "v1+hl+okx+bybit",
];

#[derive(Debug, Clone)]
pub struct EvalOptions {
    pub data: PathBuf,
    /// Unix seconds. Events before `from` still warm up engines and the Coinbase book.
    pub from_s: Option<u64>,
    pub to_s: Option<u64>,
    pub latency_ms: u64,
    pub warmup_s: u64,
    pub half_spread_bps: f64,
}

/// Time series of `(recv_ns, value)`, appended in time order.
#[derive(Debug, Default)]
struct Series {
    t: Vec<u64>,
    v: Vec<f64>,
}

impl Series {
    fn push(&mut self, t: u64, v: f64) {
        self.t.push(t);
        self.v.push(v);
    }

    /// Last value at or before `t`, with its timestamp.
    fn as_of(&self, t: u64) -> Option<(u64, f64)> {
        let i = self.t.partition_point(|x| *x <= t).checked_sub(1)?;
        Some((self.t[i], self.v[i]))
    }

    /// Reference value at `t`: needs data up to `t` and a quote younger than `REF_MAX_AGE_NS`.
    fn reference_at(&self, t: u64) -> Option<f64> {
        if t > *self.t.last()? {
            return None;
        }
        self.as_of(t)
            .filter(|(ts, _)| t - ts <= REF_MAX_AGE_NS)
            .map(|(_, v)| v)
    }
}

/// A Binance perp aggTrade with its price converted to each candidate's units.
struct TradePrint {
    t: u64,
    taker_sell: bool,
    px: [f64; NAMES.len()],
}

#[derive(Default)]
struct Collected {
    sample_t: Vec<u64>,
    /// `fair[candidate][sample]`, NaN when the candidate has no value.
    fair: Vec<Vec<f64>>,
    trades: Vec<TradePrint>,
    reference: Series,
    mids: BTreeMap<Feed, Series>,
    transport_ms: BTreeMap<Feed, Vec<f64>>,
    triggers: u64,
    v1_halted: u64,
    parse_errors: u64,
    skipped_lines: u64,
}

fn engines() -> Vec<FairEngine> {
    let base = FairParams::default().weights;
    let with = |w: Weights| {
        FairEngine::new(FairParams {
            weights: w,
            ..FairParams::default()
        })
    };
    let hl = Weights {
        hyperliquid: VARIANT_WEIGHT,
        ..base
    };
    let okx = Weights {
        okx: VARIANT_WEIGHT,
        ..base
    };
    let bybit = Weights {
        bybit: VARIANT_WEIGHT,
        ..base
    };
    let all = Weights {
        hyperliquid: VARIANT_WEIGHT,
        okx: VARIANT_WEIGHT,
        bybit: VARIANT_WEIGHT,
        ..base
    };
    let equal = Weights { perp: 1.0, ..base };
    // Order matches NAMES from EQUAL_WT on; PERP_RAW and SPOT_CONV are not engines.
    [equal, base, hl, okx, bybit, all]
        .into_iter()
        .map(with)
        .collect()
}

/// Engine index in `engines()` for a candidate index >= `EQUAL_WT`.
const fn eng(cand: usize) -> usize {
    cand - EQUAL_WT
}

fn collect(opts: &EvalOptions) -> anyhow::Result<Collected> {
    let mut replay = Replay::open(&opts.data)?;
    let mut parsers: HashMap<&'static str, Parser> = HashMap::new();
    let mut engines = engines();
    let mut c = Collected {
        fair: vec![Vec::new(); NAMES.len()],
        ..Collected::default()
    };
    let to_ns = opts.to_s.map(|s| s * NS_PER_S);
    let mut start_ns = None;
    let (mut perp, mut spot): (Option<Bbo>, Option<Bbo>) = (None, None);
    let mut events = Vec::new();

    for rec in replay.by_ref() {
        if to_ns.is_some_and(|to| rec.recv_ns > to + POST_END_SLACK_NS) {
            break;
        }
        let start = *start_ns.get_or_insert_with(|| {
            (rec.recv_ns + opts.warmup_s * NS_PER_S).max(opts.from_s.map_or(0, |s| s * NS_PER_S))
        });
        let in_window = rec.recv_ns >= start && to_ns.is_none_or(|to| rec.recv_ns <= to);

        let parser = match parsers.entry(rec.feed) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => match Parser::for_feed(rec.feed) {
                Some(p) => v.insert(p),
                None => continue,
            },
        };
        events.clear();
        if parser.parse(rec.recv_ns, &rec.msg, &mut events).is_err() {
            c.parse_errors += 1;
            continue;
        }
        for ev in &events {
            for e in &mut engines {
                e.update(ev);
            }
            match ev {
                Event::Bbo(b) => {
                    c.mids.entry(b.feed).or_default().push(b.recv_ns, b.mid());
                    if let Some(ms) = b.exch_ts_ms {
                        c.transport_ms
                            .entry(b.feed)
                            .or_default()
                            .push((b.recv_ns / NS_PER_MS) as f64 - ms as f64);
                    }
                    match b.feed {
                        Feed::Coinbase => c.reference.push(b.recv_ns, b.mid()),
                        Feed::BinancePerp => perp = Some(*b),
                        Feed::BinanceSpot => spot = Some(*b),
                        _ => {}
                    }
                    if in_window && matches!(b.feed, Feed::BinancePerp | Feed::BinanceSpot) {
                        sample(
                            &mut c,
                            &mut engines,
                            b.recv_ns,
                            perp.as_ref(),
                            spot.as_ref(),
                        );
                    }
                }
                Event::Trade(t) if in_window && t.feed == Feed::BinancePerp => {
                    let mut px = [f64::NAN; NAMES.len()];
                    px[PERP_RAW] = t.price;
                    for cand in EQUAL_WT..NAMES.len() {
                        px[cand] = engines[eng(cand)]
                            .to_fair_units(Feed::BinancePerp, t.price)
                            .unwrap_or(f64::NAN);
                    }
                    px[SPOT_CONV] = px[V1]; // spot_conv shares v1's basis and USDC
                    c.trades.push(TradePrint {
                        t: t.recv_ns,
                        taker_sell: t.taker_side == TakerSide::Sell,
                        px,
                    });
                }
                _ => {}
            }
        }
    }
    c.skipped_lines = replay.skipped_lines();
    anyhow::ensure!(!c.sample_t.is_empty(), "no samples in the selected range");
    Ok(c)
}

fn fresh(b: Option<&Bbo>, now_ns: u64, max_ns: u64) -> Option<&Bbo> {
    b.filter(|b| now_ns - b.recv_ns <= max_ns)
}

/// Record every candidate's value at `now_ns`.
fn sample(
    c: &mut Collected,
    engines: &mut [FairEngine],
    now_ns: u64,
    perp: Option<&Bbo>,
    spot: Option<&Bbo>,
) {
    c.triggers += 1;
    c.sample_t.push(now_ns);
    let v1 = &engines[eng(V1)];
    let perp = fresh(perp, now_ns, v1.max_age_ns(Feed::BinancePerp));
    let spot = fresh(spot, now_ns, v1.max_age_ns(Feed::BinanceSpot));
    c.fair[PERP_RAW].push(perp.map_or(f64::NAN, Bbo::mid));
    c.fair[SPOT_CONV].push(match (spot, v1.usdc()) {
        (Some(s), Some(u)) => s.mid() / u,
        _ => f64::NAN,
    });
    for (i, e) in engines.iter_mut().enumerate() {
        let f = e.fair(now_ns).map_or(f64::NAN, |f| f.fair);
        if eng(V1) == i && f.is_nan() {
            c.v1_halted += 1;
        }
        c.fair[EQUAL_WT + i].push(f);
    }
}

/// Samples where every core candidate has a value.
fn common_mask(c: &Collected) -> Vec<bool> {
    (0..c.sample_t.len())
        .map(|i| (PERP_RAW..=V1).all(|k| c.fair[k][i].is_finite()))
        .collect()
}

/// `(sample time, error in bps)` of `f` against the reference `shift_ns` after each sample.
fn errors_at(
    t: &[u64],
    f: &[f64],
    mask: &[bool],
    reference: &Series,
    shift_ns: u64,
) -> Vec<(u64, f64)> {
    let mut out = Vec::new();
    for i in 0..t.len() {
        if !mask[i] || !f[i].is_finite() {
            continue;
        }
        if let Some(m) = reference.reference_at(t[i] + shift_ns) {
            out.push((t[i], (m - f[i]) / f[i] * BPS));
        }
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct Metrics {
    n: usize,
    bias: f64,
    rmse: f64,
    mae: f64,
    p50: f64,
    p90: f64,
    p99: f64,
}

/// Nearest-rank percentile of a non-empty ascending slice.
fn nearest_rank(sorted: &[f64], p: f64) -> f64 {
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn metrics(errors: &[f64]) -> Metrics {
    let n = errors.len();
    if n == 0 {
        let nan = f64::NAN;
        return Metrics {
            n,
            bias: nan,
            rmse: nan,
            mae: nan,
            p50: nan,
            p90: nan,
            p99: nan,
        };
    }
    let nf = n as f64;
    let mut abs: Vec<f64> = errors.iter().map(|e| e.abs()).collect();
    abs.sort_by(f64::total_cmp);
    Metrics {
        n,
        bias: errors.iter().sum::<f64>() / nf,
        rmse: (errors.iter().map(|e| e * e).sum::<f64>() / nf).sqrt(),
        mae: abs.iter().sum::<f64>() / nf,
        p50: nearest_rank(&abs, 0.5),
        p90: nearest_rank(&abs, 0.9),
        p99: nearest_rank(&abs, 0.99),
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Markout {
    fills: usize,
    /// Mean markout in bps at `MARKOUT_HORIZONS_MS`.
    mean: [f64; MARKOUT_HORIZONS_MS.len()],
}

/// Quote `F +- half_spread` published `latency` before each trade; fill when the print trades
/// through it. `markout_h = side * (M_{t+h} - q) / q`, side +1 for a bought bid, -1 for a sold ask.
fn markout(c: &Collected, cand: usize, mask: &[bool], opts: &EvalOptions) -> Markout {
    let latency_ns = opts.latency_ms * NS_PER_MS;
    let hs = opts.half_spread_bps / BPS;
    let n_h = MARKOUT_HORIZONS_MS.len();
    let (mut sum, mut cnt, mut fills) = (vec![0.0; n_h], vec![0_usize; n_h], 0);
    for tr in &c.trades {
        let Some(qt) = tr.t.checked_sub(latency_ns) else {
            continue;
        };
        let Some(i) = c.sample_t.partition_point(|x| *x <= qt).checked_sub(1) else {
            continue;
        };
        let f = c.fair[cand][i];
        if !mask[i] || !f.is_finite() || qt - c.sample_t[i] > QUOTE_MAX_AGE_NS {
            continue;
        }
        let (q, side) = if tr.taker_sell && tr.px[cand] <= f * (1.0 - hs) {
            (f * (1.0 - hs), 1.0)
        } else if !tr.taker_sell && tr.px[cand] >= f * (1.0 + hs) {
            (f * (1.0 + hs), -1.0)
        } else {
            continue;
        };
        fills += 1;
        for (k, h) in MARKOUT_HORIZONS_MS.iter().enumerate() {
            if let Some(m) = c.reference.reference_at(tr.t + h * NS_PER_MS) {
                sum[k] += side * (m - q) / q * BPS;
                cnt[k] += 1;
            }
        }
    }
    let mut mean = [f64::NAN; MARKOUT_HORIZONS_MS.len()];
    for ((m, s), n) in mean.iter_mut().zip(&sum).zip(&cnt) {
        if *n > 0 {
            *m = s / *n as f64;
        }
    }
    Markout { fills, mean }
}

fn pearson(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len() as f64;
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (x, y) in xs.iter().zip(ys) {
        sxy += (x - mx) * (y - my);
        sxx += (x - mx).powi(2);
        syy += (y - my).powi(2);
    }
    sxy / (sxx * syy).sqrt()
}

/// Log return of `s` over `LAG_WINDOW_NS` ending at `t`.
fn window_return(s: &Series, t: u64) -> Option<f64> {
    let (_, now) = s.as_of(t)?;
    let (_, before) = s.as_of(t.checked_sub(LAG_WINDOW_NS)?)?;
    Some((now / before).ln())
}

/// Lead-lag against the Binance perp mid: the lag (ms, positive = `src` trails perp) that
/// maximizes the correlation of 500 ms returns on a 100 ms grid.
fn lag_scan(src: &Series, perp: &Series, from_ns: u64, to_ns: u64) -> (i64, f64, f64) {
    let grid: Vec<u64> = (from_ns..=to_ns).step_by(LAG_GRID_NS as usize).collect();
    let rs: Vec<Option<f64>> = grid.iter().map(|t| window_return(src, *t)).collect();
    let (mut best, mut at_zero) = ((0, f64::NEG_INFINITY), f64::NAN);
    for lag in LAG_RANGE_MS.step_by(LAG_STEP_MS) {
        let shift = lag.unsigned_abs() * NS_PER_MS;
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        for (t, r) in grid.iter().zip(&rs) {
            let tp = if lag >= 0 {
                t.checked_sub(shift)
            } else {
                Some(t + shift)
            };
            if let (Some(r), Some(rp)) = (r, tp.and_then(|tp| window_return(perp, tp))) {
                xs.push(*r);
                ys.push(rp);
            }
        }
        if xs.len() < 100 {
            continue;
        }
        let corr = pearson(&xs, &ys);
        if lag == 0 {
            at_zero = corr;
        }
        if corr > best.1 {
            best = (lag, corr);
        }
    }
    (best.0, best.1, at_zero)
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DD HH:MM:SS` UTC.
fn iso(ns: u64) -> String {
    let secs = i64::try_from(ns / NS_PER_S).unwrap_or(i64::MAX);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

fn f2(x: f64) -> String {
    if x.is_finite() {
        format!("{x:.2}")
    } else {
        "-".to_owned()
    }
}

fn horizon_label(h: u64) -> String {
    if h >= 1_000 {
        format!("{}s", h / 1_000)
    } else {
        format!("{h}ms")
    }
}

/// Replay `opts.data` and render the Markdown report.
pub fn run(opts: &EvalOptions) -> anyhow::Result<String> {
    let c = collect(opts)?;
    let mask = common_mask(&c);
    let shift = |h: u64| (opts.latency_ms + h) * NS_PER_MS;

    let valid_t: Vec<u64> = c
        .sample_t
        .iter()
        .zip(&mask)
        .filter(|(_, m)| **m)
        .map(|(t, _)| *t)
        .collect();
    let (first, last) = (
        valid_t.first().copied().unwrap_or(0),
        valid_t.last().copied().unwrap_or(0),
    );
    let mid_t = first + (last - first) / 2;
    let n_common = valid_t.len();

    // errs[cand][horizon] -> (sample time, e)
    let errs: Vec<Vec<Vec<(u64, f64)>>> = (0..NAMES.len())
        .map(|k| {
            HORIZONS_MS
                .iter()
                .map(|h| errors_at(&c.sample_t, &c.fair[k], &mask, &c.reference, shift(*h)))
                .collect()
        })
        .collect();

    let r = Report {
        c: &c,
        mask: &mask,
        opts,
        errs: &errs,
        first,
        last,
        mid_t,
        n_common,
    };
    let mut out = String::new();
    r.header(&mut out);
    r.errors(&mut out);
    r.halves(&mut out);
    r.markout(&mut out);
    r.lead_lag(&mut out);
    r.transport(&mut out);
    Ok(out)
}

/// Everything the report sections read.
struct Report<'a> {
    c: &'a Collected,
    mask: &'a [bool],
    opts: &'a EvalOptions,
    /// `errs[candidate][horizon]` -> `(sample time, e)`.
    errs: &'a [Vec<Vec<(u64, f64)>>],
    first: u64,
    last: u64,
    mid_t: u64,
    n_common: usize,
}

impl Report<'_> {
    fn header(&self, out: &mut String) {
        let (c, opts, first, last) = (self.c, self.opts, self.first, self.last);
        let _ = writeln!(out, "# Fair-value evaluation\n");
        let _ = writeln!(
            out,
            "- Data: `{}`, {} to {} UTC ({:.0} min scored, {} min warm-up skipped).",
            opts.data.display(),
            iso(first),
            iso(last),
            (last - first) as f64 / 60e9,
            opts.warmup_s / 60
        );
        let _ = writeln!(
            out,
            "- Reference M: Coinbase SOL-USD mid (held out). Publish latency {} ms is added to t before the reference lookup.",
            opts.latency_ms
        );
        let _ = writeln!(
            out,
            "- Samples: one per Binance perp/spot BBO. {} triggers, {:.2}% with v1 halted; {} on the common set. {} unparsable messages, {} malformed lines skipped.",
            c.triggers,
            c.v1_halted as f64 / c.triggers as f64 * 100.0,
            self.n_common,
            c.parse_errors,
            c.skipped_lines
        );
        let _ = writeln!(
            out,
            "- Errors e = (M - F) / F in bps. All candidates are scored on the same sample set (every core candidate has a value). Samples overlap in time, so n overstates independent observations.\n"
        );
    }

    fn errors(&self, out: &mut String) {
        let errs = self.errs;
        let _ = writeln!(out, "## Error vs reference, full range\n");
        let _ = writeln!(
            out,
            "| candidate | h | n | bias | RMSE | MAE | p50 | p90 | p99 |"
        );
        let _ = writeln!(out, "|---|---|---:|---:|---:|---:|---:|---:|---:|");
        for (k, name) in NAMES.iter().enumerate() {
            for (j, h) in HORIZONS_MS.iter().enumerate() {
                let m = metrics(&errs[k][j].iter().map(|x| x.1).collect::<Vec<_>>());
                let _ = writeln!(
                    out,
                    "| {name} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    horizon_label(*h),
                    m.n,
                    f2(m.bias),
                    f2(m.rmse),
                    f2(m.mae),
                    f2(m.p50),
                    f2(m.p90),
                    f2(m.p99)
                );
            }
        }
    }

    fn halves(&self, out: &mut String) {
        let (errs, mid_t) = (self.errs, self.mid_t);
        let _ = writeln!(out, "\n## Walk-forward halves, RMSE (bias) in bps\n");
        let _ = write!(out, "| candidate | half |");
        for h in HORIZONS_MS {
            let _ = write!(out, " {} |", horizon_label(h));
        }
        let _ = writeln!(out, "\n|---|---|---:|---:|---:|---:|");
        for (k, name) in NAMES.iter().enumerate() {
            for (half, label) in [(false, "first"), (true, "second")] {
                let _ = write!(out, "| {name} | {label} |");
                for horizon in &errs[k] {
                    let part: Vec<f64> = horizon
                        .iter()
                        .filter(|(t, _)| (*t >= mid_t) == half)
                        .map(|x| x.1)
                        .collect();
                    let m = metrics(&part);
                    let _ = write!(out, " {} ({}) |", f2(m.rmse), f2(m.bias));
                }
                let _ = writeln!(out);
            }
        }
    }

    fn markout(&self, out: &mut String) {
        let (c, opts) = (self.c, self.opts);
        let _ = writeln!(
            out,
            "\n## Markout of simulated quotes (F +- {} bps)\n",
            f2(opts.half_spread_bps)
        );
        let _ = writeln!(
            out,
            "Quote published {} ms before the print; filled when a Binance perp aggTrade (converted to the candidate's units) trades through it. Mean markout in bps; adverse selection = markout at 0 minus markout at h.\n",
            opts.latency_ms
        );
        let _ = writeln!(
            out,
            "| candidate | fills | markout 0 | markout 1s | markout 5s |"
        );
        let _ = writeln!(out, "|---|---:|---:|---:|---:|");
        for (k, name) in NAMES.iter().enumerate() {
            let m = markout(c, k, self.mask, opts);
            let _ = writeln!(
                out,
                "| {name} | {} | {} | {} | {} |",
                m.fills,
                f2(m.mean[0]),
                f2(m.mean[1]),
                f2(m.mean[2])
            );
        }
    }

    fn lead_lag(&self, out: &mut String) {
        let (c, first, last) = (self.c, self.first, self.last);
        let _ = writeln!(out, "\n## Lead-lag vs Binance perp mid\n");
        let _ = writeln!(
            out,
            "Lag in ms (positive = the source trails Binance perp) maximizing the correlation of 500 ms mid returns, by local receive time.\n"
        );
        let _ = writeln!(out, "| source | best lag ms | corr at best | corr at 0 |");
        let _ = writeln!(out, "|---|---:|---:|---:|");
        if let Some(perp) = c.mids.get(&Feed::BinancePerp) {
            for feed in [
                Feed::BinanceSpot,
                Feed::Coinbase,
                Feed::Hyperliquid,
                Feed::Okx,
                Feed::Bybit,
            ] {
                if let Some(src) = c.mids.get(&feed) {
                    let (lag, best, zero) = lag_scan(src, perp, first, last);
                    let _ = writeln!(out, "| {} | {lag} | {best:.3} | {zero:.3} |", feed.label());
                }
            }
        }
    }

    fn transport(&self, out: &mut String) {
        let c = self.c;
        let _ = writeln!(
            out,
            "\n## Transport lag (receive time minus exchange timestamp, ms)\n"
        );
        let _ = writeln!(out, "| feed | n | p50 | p90 | p99 |");
        let _ = writeln!(out, "|---|---:|---:|---:|---:|");
        for (feed, lags) in &c.transport_ms {
            let mut s = lags.clone();
            s.sort_by(f64::total_cmp);
            let _ = writeln!(
                out,
                "| {} | {} | {:.0} | {:.0} | {:.0} |",
                feed.label(),
                s.len(),
                nearest_rank(&s, 0.5),
                nearest_rank(&s, 0.9),
                nearest_rank(&s, 0.99)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn metrics_of_known_errors() {
        // +10 and -30 bps alternating: bias -10, rmse sqrt((100+900)/2), mae 20.
        let m = metrics(&[10.0, -30.0, 10.0, -30.0]);
        assert_eq!(m.n, 4);
        assert!(close(m.bias, -10.0));
        assert!(close(m.rmse, 500_f64.sqrt()));
        assert!(close(m.mae, 20.0));
        assert!(close(m.p50, 10.0) && close(m.p99, 30.0));
        assert!(metrics(&[]).rmse.is_nan());
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let errs: Vec<f64> = (1..=100).map(f64::from).collect();
        let m = metrics(&errs);
        assert!(close(m.p50, 50.0) && close(m.p90, 90.0) && close(m.p99, 99.0));
    }

    #[test]
    fn errors_use_latency_shifted_reference() {
        // Reference ramps 100.00 -> 100.10 over 1 s in 10 ms steps (1 bp per step).
        let mut reference = Series::default();
        for k in 0..=100_u64 {
            reference.push(k * 10 * NS_PER_MS, 100.0 + k as f64 * 0.001);
        }
        let t = [0, 100 * NS_PER_MS];
        let f = [100.0, 100.0];
        let mask = [true, true];
        // Shift 0: M(0) = 100 -> 0 bps; M(100 ms) = 100.01 -> 1 bp.
        let e = errors_at(&t, &f, &mask, &reference, 0);
        assert!(close(e[0].1, 0.0) && (e[1].1 - 1.0).abs() < 1e-6);
        // Shift 150 ms: M(150) = 100.015 -> 1.5 bp, M(250) = 100.025 -> 2.5 bp.
        let e = errors_at(&t, &f, &mask, &reference, 150 * NS_PER_MS);
        assert!((e[0].1 - 1.5).abs() < 1e-6 && (e[1].1 - 2.5).abs() < 1e-6);
        // Past the end of the reference: no sample. Masked samples are skipped.
        assert!(errors_at(&t, &f, &mask, &reference, 2 * NS_PER_S).is_empty());
        assert_eq!(errors_at(&t, &f, &[false, true], &reference, 0).len(), 1);
    }

    #[test]
    fn stale_reference_is_missing() {
        let mut reference = Series::default();
        reference.push(0, 100.0);
        reference.push(10 * NS_PER_S, 100.0);
        assert!(reference.reference_at(NS_PER_S).is_some());
        assert!(reference.reference_at(5 * NS_PER_S).is_none()); // 5 s since last update
    }

    #[test]
    fn markout_of_a_filled_bid() {
        // F = 100 flat. Half spread 2 bps: bid 99.98. A taker sell at 99.97 fills it; M later 100.
        let mut reference = Series::default();
        for s in 0..=10_u64 {
            reference.push(s * NS_PER_S, 100.0);
        }
        let mut px = [f64::NAN; NAMES.len()];
        px[V1] = 99.97;
        let mut miss = px;
        miss[V1] = 99.99; // does not trade through the bid
        let mut fair = vec![vec![f64::NAN; 2]; NAMES.len()];
        fair[V1] = vec![100.0, 100.0];
        let c = Collected {
            sample_t: vec![NS_PER_S, 2 * NS_PER_S],
            fair,
            trades: vec![
                TradePrint {
                    t: 3 * NS_PER_S,
                    taker_sell: true,
                    px,
                },
                TradePrint {
                    t: 3 * NS_PER_S,
                    taker_sell: true,
                    px: miss,
                },
            ],
            reference,
            ..Collected::default()
        };
        let opts = EvalOptions {
            data: PathBuf::new(),
            from_s: None,
            to_s: None,
            latency_ms: 150,
            warmup_s: 0,
            half_spread_bps: 2.0,
        };
        let m = markout(&c, V1, &[true, true], &opts);
        assert_eq!(m.fills, 1);
        // Bought at 99.98, mid 100: +2 bps at every horizon.
        assert!(
            m.mean.iter().all(|x| (x - 2.0).abs() < 1e-3),
            "{:?}",
            m.mean
        );
    }

    #[test]
    fn lag_scan_finds_a_known_shift() {
        // Source copies perp 300 ms later; perp is a random walk updated every 50 ms.
        let (mut perp, mut src) = (Series::default(), Series::default());
        let mut x = 100.0_f64;
        let mut seed = 12_345_u64;
        for k in 0..20_000_u64 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            x *= 1.0 + ((seed >> 33) as f64 / f64::from(1_u32 << 31) - 0.5) * 1e-3;
            let t = k * 50 * NS_PER_MS;
            perp.push(t, x);
            src.push(t + 300 * NS_PER_MS, x);
        }
        let (lag, corr, zero) = lag_scan(&src, &perp, 5 * NS_PER_S, 900 * NS_PER_S);
        assert_eq!(lag, 300);
        assert!(corr > 0.99 && zero < corr);
    }

    #[test]
    fn iso_formats_utc() {
        assert_eq!(iso(1_791_125_026 * NS_PER_S), "2026-10-04 14:43:46");
        assert_eq!(iso(0), "1970-01-01 00:00:00");
    }
}
