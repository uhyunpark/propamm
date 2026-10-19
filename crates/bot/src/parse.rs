//! Per-feed parsers: raw recorded message -> normalized events.
//!
//! One enum-dispatched [`Parser`] per feed file. Parsers are stateful (Coinbase book, Hyperliquid
//! ack tracking) and lenient: acks and heartbeats yield no events, malformed frames yield an error
//! the caller may count and skip.

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;

use crate::NS_PER_MS;

/// Hyperliquid trades older than this at receive time belong to the post-subscribe snapshot.
const HL_SNAPSHOT_AGE_MS: u64 = 5_000;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing or malformed field `{0}`")]
    Field(&'static str),
}

/// Market-data source. Perp mark price and aggTrades belong to [`Feed::BinancePerp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feed {
    BinanceSpot,
    BinanceUsdcUsdt,
    BinancePerp,
    Coinbase,
    Hyperliquid,
    Okx,
    Bybit,
}

impl Feed {
    pub fn label(self) -> &'static str {
        match self {
            Feed::BinanceSpot => "binance_spot",
            Feed::BinanceUsdcUsdt => "binance_usdcusdt",
            Feed::BinancePerp => "binance_perp",
            Feed::Coinbase => "coinbase",
            Feed::Hyperliquid => "hyperliquid",
            Feed::Okx => "okx",
            Feed::Bybit => "bybit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bbo {
    pub feed: Feed,
    pub recv_ns: u64,
    pub exch_ts_ms: Option<u64>,
    pub bid: f64,
    pub bid_size: f64,
    pub ask: f64,
    pub ask_size: f64,
}

impl Bbo {
    pub fn mid(&self) -> f64 {
        f64::midpoint(self.bid, self.ask)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakerSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trade {
    pub feed: Feed,
    pub recv_ns: u64,
    pub exch_ts_ms: Option<u64>,
    pub price: f64,
    pub size: f64,
    pub taker_side: TakerSide,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mark {
    pub recv_ns: u64,
    pub mark: f64,
    pub index: f64,
    pub funding_rate: f64,
    pub next_funding_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Bbo(Bbo),
    Trade(Trade),
    Mark(Mark),
}

#[derive(Debug)]
enum Kind {
    BinanceSpot,
    BinanceUsdcUsdt,
    BinancePerpBbo,
    BinancePerpMark,
    BinancePerpTrades,
    Coinbase(CoinbaseBook),
    Hyperliquid,
    Okx,
    Bybit(BybitTop),
}

#[derive(Debug)]
pub struct Parser {
    kind: Kind,
}

impl Parser {
    /// Parser for a feed name from `feeds::FEEDS`.
    pub fn for_feed(name: &str) -> Option<Self> {
        let kind = match name {
            "binance_spot_sol_bbo" => Kind::BinanceSpot,
            "binance_spot_usdcusdt_bbo" => Kind::BinanceUsdcUsdt,
            "binance_perp_sol_bbo" => Kind::BinancePerpBbo,
            "binance_perp_sol_mark" => Kind::BinancePerpMark,
            "binance_perp_sol_trades" => Kind::BinancePerpTrades,
            "coinbase_sol_usd" => Kind::Coinbase(CoinbaseBook::default()),
            "hyperliquid_sol" => Kind::Hyperliquid,
            "okx_perp_sol_bbo" => Kind::Okx,
            "bybit_perp_sol_bbo" => Kind::Bybit(BybitTop::default()),
            _ => return None,
        };
        Some(Self { kind })
    }

    /// Parse one raw message, appending events to `out`.
    pub fn parse(
        &mut self,
        recv_ns: u64,
        msg: &Value,
        out: &mut Vec<Event>,
    ) -> Result<(), ParseError> {
        match &mut self.kind {
            Kind::BinanceSpot => binance_bbo(Feed::BinanceSpot, recv_ns, msg, out),
            Kind::BinanceUsdcUsdt => binance_bbo(Feed::BinanceUsdcUsdt, recv_ns, msg, out),
            Kind::BinancePerpBbo => binance_bbo(Feed::BinancePerp, recv_ns, msg, out),
            Kind::BinancePerpMark => binance_mark(recv_ns, msg, out),
            Kind::BinancePerpTrades => binance_agg_trade(recv_ns, msg, out),
            Kind::Coinbase(book) => book.parse(recv_ns, msg, out),
            Kind::Hyperliquid => hyperliquid(recv_ns, msg, out),
            Kind::Okx => okx(recv_ns, msg, out),
            Kind::Bybit(top) => top.parse(recv_ns, msg, out),
        }
    }
}

/// Split a recorded line into `(recv_ns, raw message)`.
pub fn split_line(line: &str) -> Result<(u64, Value), ParseError> {
    let mut v: Value = serde_json::from_str(line)?;
    let recv_ns = v
        .get("recv_ns")
        .and_then(Value::as_u64)
        .ok_or(ParseError::Field("recv_ns"))?;
    let msg = v
        .get_mut("msg")
        .map(Value::take)
        .ok_or(ParseError::Field("msg"))?;
    Ok((recv_ns, msg))
}

/// Number encoded as a JSON string or number.
fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        other => other.as_f64(),
    }
}

fn num(v: &Value, key: &'static str) -> Result<f64, ParseError> {
    v.get(key).and_then(as_num).ok_or(ParseError::Field(key))
}

fn millis(v: &Value, key: &'static str) -> Result<u64, ParseError> {
    match v.get(key) {
        Some(Value::String(s)) => s.parse().ok(),
        Some(other) => other.as_u64(),
        None => None,
    }
    .ok_or(ParseError::Field(key))
}

/// Level `[px, size, ...]` of an array-encoded book side.
fn level(v: &Value) -> Option<(f64, f64)> {
    Some((as_num(v.get(0)?)?, as_num(v.get(1)?)?))
}

fn bbo_event(
    feed: Feed,
    recv_ns: u64,
    exch_ts_ms: Option<u64>,
    (bid, bid_size): (f64, f64),
    (ask, ask_size): (f64, f64),
) -> Event {
    Event::Bbo(Bbo {
        feed,
        recv_ns,
        exch_ts_ms,
        bid,
        bid_size,
        ask,
        ask_size,
    })
}

/// Hyperliquid book level `{"px":..,"sz":..}`.
fn px_sz(v: &Value) -> Result<(f64, f64), ParseError> {
    Ok((num(v, "px")?, num(v, "sz")?))
}

fn binance_bbo(
    feed: Feed,
    recv_ns: u64,
    msg: &Value,
    out: &mut Vec<Event>,
) -> Result<(), ParseError> {
    if msg.get("b").is_none() {
        return Ok(()); // subscription ack
    }
    out.push(bbo_event(
        feed,
        recv_ns,
        msg.get("T").and_then(Value::as_u64), // spot bookTicker has none
        (num(msg, "b")?, num(msg, "B")?),
        (num(msg, "a")?, num(msg, "A")?),
    ));
    Ok(())
}

fn binance_mark(recv_ns: u64, msg: &Value, out: &mut Vec<Event>) -> Result<(), ParseError> {
    if msg.get("e").and_then(Value::as_str) != Some("markPriceUpdate") {
        return Ok(());
    }
    out.push(Event::Mark(Mark {
        recv_ns,
        mark: num(msg, "p")?,
        index: num(msg, "i")?,
        funding_rate: num(msg, "r")?,
        next_funding_ms: millis(msg, "T")?,
    }));
    Ok(())
}

fn binance_agg_trade(recv_ns: u64, msg: &Value, out: &mut Vec<Event>) -> Result<(), ParseError> {
    if msg.get("e").and_then(Value::as_str) != Some("aggTrade") {
        return Ok(());
    }
    // `m`: the buyer is the maker, so the taker sold.
    let buyer_is_maker = msg
        .get("m")
        .and_then(Value::as_bool)
        .ok_or(ParseError::Field("m"))?;
    out.push(Event::Trade(Trade {
        feed: Feed::BinancePerp,
        recv_ns,
        exch_ts_ms: Some(millis(msg, "T")?),
        price: num(msg, "p")?,
        size: num(msg, "q")?,
        taker_side: if buyer_is_maker {
            TakerSide::Sell
        } else {
            TakerSide::Buy
        },
    }));
    Ok(())
}

/// Level2 book for Coinbase. Keys are the price's `f64` bit pattern, which orders like the price
/// for positive finite values.
#[derive(Debug, Default)]
struct CoinbaseBook {
    bids: BTreeMap<u64, f64>,
    asks: BTreeMap<u64, f64>,
    seeded: bool,
    last_top: Option<[f64; 4]>,
}

impl CoinbaseBook {
    fn parse(&mut self, recv_ns: u64, msg: &Value, out: &mut Vec<Event>) -> Result<(), ParseError> {
        match msg.get("type").and_then(Value::as_str) {
            Some("snapshot") => {
                self.bids.clear();
                self.asks.clear();
                for (side, key) in [(&mut self.bids, "bids"), (&mut self.asks, "asks")] {
                    let levels = msg
                        .get(key)
                        .and_then(Value::as_array)
                        .ok_or(ParseError::Field("levels"))?;
                    for l in levels {
                        let (px, sz) = level(l).ok_or(ParseError::Field("level"))?;
                        set_level(side, px, sz);
                    }
                }
                self.seeded = true;
                self.emit_if_top_changed(recv_ns, out);
            }
            Some("l2update") if self.seeded => {
                let changes = msg
                    .get("changes")
                    .and_then(Value::as_array)
                    .ok_or(ParseError::Field("changes"))?;
                for c in changes {
                    let side = match c.get(0).and_then(Value::as_str) {
                        Some("buy") => &mut self.bids,
                        Some("sell") => &mut self.asks,
                        _ => return Err(ParseError::Field("side")),
                    };
                    let px = c.get(1).and_then(as_num).ok_or(ParseError::Field("px"))?;
                    let sz = c.get(2).and_then(as_num).ok_or(ParseError::Field("size"))?;
                    set_level(side, px, sz);
                }
                self.emit_if_top_changed(recv_ns, out);
            }
            Some("match") => {
                // `side` is the maker's side: a maker sell means the taker bought.
                let taker_side = match msg.get("side").and_then(Value::as_str) {
                    Some("sell") => TakerSide::Buy,
                    Some("buy") => TakerSide::Sell,
                    _ => return Err(ParseError::Field("side")),
                };
                out.push(Event::Trade(Trade {
                    feed: Feed::Coinbase,
                    recv_ns,
                    exch_ts_ms: None,
                    price: num(msg, "price")?,
                    size: num(msg, "size")?,
                    taker_side,
                }));
            }
            _ => {} // acks, heartbeat, last_match, updates before the snapshot
        }
        Ok(())
    }

    fn emit_if_top_changed(&mut self, recv_ns: u64, out: &mut Vec<Event>) {
        let (Some((bid, bid_size)), Some((ask, ask_size))) = (
            self.bids
                .last_key_value()
                .map(|(k, s)| (f64::from_bits(*k), *s)),
            self.asks
                .first_key_value()
                .map(|(k, s)| (f64::from_bits(*k), *s)),
        ) else {
            return;
        };
        let top = [bid, bid_size, ask, ask_size];
        if self.last_top == Some(top) {
            return;
        }
        self.last_top = Some(top);
        out.push(bbo_event(
            Feed::Coinbase,
            recv_ns,
            None,
            (bid, bid_size),
            (ask, ask_size),
        ));
    }
}

fn set_level(side: &mut BTreeMap<u64, f64>, px: f64, sz: f64) {
    if sz > 0.0 {
        side.insert(px.to_bits(), sz);
    } else {
        side.remove(&px.to_bits());
    }
}

fn hyperliquid(recv_ns: u64, msg: &Value, out: &mut Vec<Event>) -> Result<(), ParseError> {
    let data = msg.get("data").unwrap_or(&Value::Null);
    match msg.get("channel").and_then(Value::as_str) {
        Some("bbo") => {
            // `bbo` is [best bid, best ask]; either side may be null.
            let (Some(bid), Some(ask)) = (
                data.pointer("/bbo/0").filter(|v| !v.is_null()),
                data.pointer("/bbo/1").filter(|v| !v.is_null()),
            ) else {
                return Ok(());
            };
            out.push(bbo_event(
                Feed::Hyperliquid,
                recv_ns,
                Some(millis(data, "time")?),
                px_sz(bid)?,
                px_sz(ask)?,
            ));
        }
        Some("trades") => {
            let now_ms = recv_ns / NS_PER_MS;
            for t in data.as_array().ok_or(ParseError::Field("data"))? {
                let time = millis(t, "time")?;
                // The first message after (re)subscribing replays old trades; drop them by age.
                if now_ms.saturating_sub(time) > HL_SNAPSHOT_AGE_MS {
                    continue;
                }
                out.push(Event::Trade(Trade {
                    feed: Feed::Hyperliquid,
                    recv_ns,
                    exch_ts_ms: Some(time),
                    price: num(t, "px")?,
                    size: num(t, "sz")?,
                    taker_side: match t.get("side").and_then(Value::as_str) {
                        Some("B") => TakerSide::Buy,
                        Some("A") => TakerSide::Sell,
                        _ => return Err(ParseError::Field("side")),
                    },
                }));
            }
        }
        _ => {}
    }
    Ok(())
}

fn okx(recv_ns: u64, msg: &Value, out: &mut Vec<Event>) -> Result<(), ParseError> {
    let Some(d) = msg.pointer("/data/0") else {
        return Ok(()); // ack
    };
    let bid = d
        .pointer("/bids/0")
        .and_then(level)
        .ok_or(ParseError::Field("bids"))?;
    let ask = d
        .pointer("/asks/0")
        .and_then(level)
        .ok_or(ParseError::Field("asks"))?;
    out.push(bbo_event(
        Feed::Okx,
        recv_ns,
        Some(millis(d, "ts")?),
        bid,
        ask,
    ));
    Ok(())
}

/// Bybit `orderbook.1`: `snapshot` carries both sides; a `delta` may carry only the changed one.
#[derive(Debug, Default)]
struct BybitTop {
    bid: Option<(f64, f64)>,
    ask: Option<(f64, f64)>,
}

impl BybitTop {
    fn parse(&mut self, recv_ns: u64, msg: &Value, out: &mut Vec<Event>) -> Result<(), ParseError> {
        let Some(d) = msg.get("data").filter(|_| msg.get("topic").is_some()) else {
            return Ok(()); // ack
        };
        let is_snapshot = msg.get("type").and_then(Value::as_str) == Some("snapshot");
        for (side, key) in [(&mut self.bid, "b"), (&mut self.ask, "a")] {
            match d.get(key).and_then(|a| a.get(0)) {
                Some(l) => *side = Some(level(l).ok_or(ParseError::Field("level"))?),
                None if is_snapshot => *side = None,
                None => {}
            }
        }
        if let (Some(bid), Some(ask)) = (self.bid, self.ask) {
            out.push(bbo_event(
                Feed::Bybit,
                recv_ns,
                Some(millis(msg, "ts")?),
                bid,
                ask,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Events of the fixture named after the feed in `tests/fixtures/`.
    macro_rules! run {
        ($feed:literal) => {
            run_lines(
                $feed,
                include_str!(concat!("../tests/fixtures/", $feed, ".jsonl")),
            )
        };
    }

    fn run_lines(feed: &str, fixture: &str) -> Vec<Event> {
        let mut p = Parser::for_feed(feed).unwrap();
        let mut out = Vec::new();
        for line in fixture.lines() {
            let (recv_ns, msg) = split_line(line).unwrap();
            p.parse(recv_ns, &msg, &mut out).unwrap();
        }
        out
    }

    /// Parse one inline raw message at `recv_ns` = 1.
    fn feed_json(p: &mut Parser, s: &str, out: &mut Vec<Event>) {
        let v: Value = serde_json::from_str(s).unwrap();
        p.parse(1, &v, out).unwrap();
    }

    fn bbo(e: &Event) -> Bbo {
        match e {
            Event::Bbo(b) => *b,
            other => panic!("not a bbo: {other:?}"),
        }
    }

    fn trade(e: &Event) -> Trade {
        match e {
            Event::Trade(t) => *t,
            other => panic!("not a trade: {other:?}"),
        }
    }

    #[test]
    fn every_recorded_feed_has_a_parser() {
        for f in crate::feeds::FEEDS {
            assert!(Parser::for_feed(f.name).is_some(), "{}", f.name);
        }
    }

    #[test]
    fn binance_spot_has_no_exchange_ts() {
        let ev = run!("binance_spot_sol_bbo");
        let b = bbo(&ev[0]);
        assert_eq!(b.feed, Feed::BinanceSpot);
        assert_eq!(b.exch_ts_ms, None);
        assert!((b.bid - 121.41).abs() < 1e-9 && (b.ask_size - 345.958).abs() < 1e-9);
    }

    #[test]
    fn binance_usdcusdt() {
        let ev = run!("binance_spot_usdcusdt_bbo");
        assert_eq!(bbo(&ev[0]).feed, Feed::BinanceUsdcUsdt);
        assert!((bbo(&ev[0]).mid() - 1.000_105).abs() < 1e-9);
    }

    #[test]
    fn binance_perp_bbo_and_mark() {
        let ev = run!("binance_perp_sol_bbo");
        let b = bbo(&ev[0]);
        assert_eq!(
            (b.feed, b.exch_ts_ms),
            (Feed::BinancePerp, Some(1_791_125_026_815))
        );
        assert!((b.bid - 121.37).abs() < 1e-9 && (b.bid_size - 412.84).abs() < 1e-9);

        let ev = run!("binance_perp_sol_mark");
        let Event::Mark(m) = ev[0] else { panic!() };
        assert!((m.mark - 121.380_770_77).abs() < 1e-9 && (m.index - 121.422_405_36).abs() < 1e-9);
        assert!((m.funding_rate - 0.0001).abs() < 1e-12);
        assert_eq!(m.next_funding_ms, 1_791_129_600_000);
    }

    #[test]
    fn binance_agg_trade_taker_side() {
        let ev = run!("binance_perp_sol_trades");
        // `m: true` -> buyer was maker -> taker sold.
        assert_eq!(trade(&ev[0]).taker_side, TakerSide::Sell);
        assert_eq!(trade(&ev[1]).taker_side, TakerSide::Buy);
        assert!((trade(&ev[0]).price - 121.37).abs() < 1e-9);
        assert_eq!(trade(&ev[0]).exch_ts_ms, Some(1_791_125_027_186));
    }

    #[test]
    fn coinbase_book_emits_bbo_on_top_change_only() {
        let ev = run!("coinbase_sol_usd");
        // snapshot -> bbo; l2update deep in the book -> nothing; l2update at top -> bbo; match -> trade.
        let bbos: Vec<Bbo> = ev
            .iter()
            .filter(|e| matches!(e, Event::Bbo(_)))
            .map(bbo)
            .collect();
        assert_eq!(bbos.len(), 2, "{ev:?}");
        assert!((bbos[0].bid - 121.40).abs() < 1e-9 && (bbos[0].ask - 121.41).abs() < 1e-9);
        assert!((bbos[1].ask - 121.41).abs() < 1e-9 && (bbos[1].ask_size - 5.0).abs() < 1e-9);
        let trades: Vec<Trade> = ev
            .iter()
            .filter(|e| matches!(e, Event::Trade(_)))
            .map(trade)
            .collect();
        // maker `side: sell` -> taker bought; `last_match` is ignored.
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].taker_side, TakerSide::Buy);
        assert!((trades[0].price - 121.41).abs() < 1e-9);
    }

    #[test]
    fn coinbase_zero_size_removes_level_and_snapshot_resets() {
        let mut p = Parser::for_feed("coinbase_sol_usd").unwrap();
        let mut out = Vec::new();
        // l2update before any snapshot is ignored.
        for s in [
            r#"{"type":"l2update","changes":[["buy","1","1"]]}"#,
            r#"{"type":"snapshot","bids":[["10","1"],["9","1"]],"asks":[["11","1"]]}"#,
            r#"{"type":"l2update","changes":[["buy","10","0"]]}"#,
            r#"{"type":"snapshot","bids":[["20","2"]],"asks":[["21","2"]]}"#,
        ] {
            feed_json(&mut p, s, &mut out);
        }
        let bids: Vec<f64> = out.iter().map(|e| bbo(e).bid).collect();
        assert_eq!(bids, vec![10.0, 9.0, 20.0]);
    }

    #[test]
    fn hyperliquid_bbo_and_trade_snapshot_skipped() {
        let ev = run!("hyperliquid_sol");
        // ack x2 and the old-trade snapshot yield nothing; then a bbo and a live trade.
        assert_eq!(ev.len(), 2, "{ev:?}");
        let b = bbo(&ev[0]);
        assert_eq!(
            (b.feed, b.exch_ts_ms),
            (Feed::Hyperliquid, Some(1_791_125_026_834))
        );
        assert!((b.bid - 121.38).abs() < 1e-9 && (b.ask_size - 1758.15).abs() < 1e-9);
        assert_eq!(trade(&ev[1]).taker_side, TakerSide::Buy);
    }

    #[test]
    fn hyperliquid_null_side_is_skipped() {
        let mut p = Parser::for_feed("hyperliquid_sol").unwrap();
        let mut out = Vec::new();
        feed_json(
            &mut p,
            r#"{"channel":"bbo","data":{"coin":"SOL","time":1,"bbo":[null,{"px":"1","sz":"1","n":1}]}}"#,
            &mut out,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn okx_bbo() {
        let ev = run!("okx_perp_sol_bbo");
        assert_eq!(ev.len(), 2); // ack skipped
        let b = bbo(&ev[0]);
        assert_eq!((b.feed, b.exch_ts_ms), (Feed::Okx, Some(1_791_125_026_805)));
        assert!((b.bid - 121.38).abs() < 1e-9 && (b.ask_size - 1550.76).abs() < 1e-9);
    }

    #[test]
    fn bybit_snapshot_and_one_sided_delta() {
        let ev = run!("bybit_perp_sol_bbo");
        assert_eq!(ev.len(), 2); // ack skipped
        let b = bbo(&ev[0]);
        assert_eq!(
            (b.feed, b.exch_ts_ms),
            (Feed::Bybit, Some(1_791_125_026_589))
        );
        assert!((b.bid - 121.41).abs() < 1e-9 && (b.ask - 121.42).abs() < 1e-9);

        let mut p = Parser::for_feed("bybit_perp_sol_bbo").unwrap();
        let mut out = Vec::new();
        for s in [
            r#"{"topic":"orderbook.1.SOLUSDT","ts":1,"type":"snapshot","data":{"b":[["10","1"]],"a":[["11","2"]]}}"#,
            r#"{"topic":"orderbook.1.SOLUSDT","ts":2,"type":"delta","data":{"b":[["10.5","3"]],"a":[]}}"#,
        ] {
            feed_json(&mut p, s, &mut out);
        }
        let last = bbo(&out[1]);
        assert!((last.bid - 10.5).abs() < 1e-9 && (last.ask - 11.0).abs() < 1e-9);
    }

    #[test]
    fn malformed_message_is_an_error() {
        let mut p = Parser::for_feed("binance_perp_sol_bbo").unwrap();
        let v: Value = serde_json::from_str(r#"{"b":"x","B":"1","a":"1","A":"1"}"#).unwrap();
        assert!(p.parse(1, &v, &mut Vec::new()).is_err());
        assert!(split_line("not json").is_err());
    }
}
