//! WASM bindings for the demo web app. Interface: `docs/demo-contract.md`.
//!
//! Every export is a thin wrapper over a plain Rust function in this file, so the logic is
//! tested natively. u64 values cross the boundary as JS `bigint`, errors as `Error` whose
//! message is the error name.
#![deny(clippy::unwrap_used, clippy::expect_used)]

use propamm_client::{DecodeError, decode_market, swap_data};
use propamm_pricing::{MarketState, PRICE_SCALE, PricingError, Side, is_fresh};
use serde::Serialize;
use solana_pubkey::Pubkey;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
export type Side = "buy" | "sell";
export interface MarketView {
  admin: string; updater: string; baseMint: string; quoteMint: string; baseVault: string; quoteVault: string;
  midPrice: bigint; lastUpdateSlot: bigint; windowStartMid: bigint;
  askConsumed: bigint; bidConsumed: bigint; levelSize: bigint; maxAgeSlots: bigint;
  halfSpreadBps: number; levelStepBps: number; maxMoveBps: number; numLevels: number; paused: boolean;
}
export interface Level { price: bigint; remaining: bigint }
export interface Quote { amountOut: bigint; consumedAfter: bigint }
"#;

/// Error name as thrown to JS.
type Name = &'static str;

fn pricing_name(e: PricingError) -> Name {
    match e {
        PricingError::ZeroInput => "ZeroInput",
        PricingError::ZeroOutput => "ZeroOutput",
        PricingError::InsufficientDepth => "InsufficientDepth",
        PricingError::InvalidParams => "InvalidParams",
        PricingError::Overflow => "Overflow",
        PricingError::SlotRegression => "SlotRegression",
        PricingError::AlreadyUpdatedThisSlot => "AlreadyUpdatedThisSlot",
    }
}

fn decode_name(e: DecodeError) -> Name {
    match e {
        DecodeError::WrongSize { .. } => "WrongSize",
        DecodeError::WrongDiscriminator => "WrongDiscriminator",
    }
}

fn parse_side(side: &str) -> Result<Side, Name> {
    match side {
        "buy" => Ok(Side::BuyBase),
        "sell" => Ok(Side::SellBase),
        _ => Err("InvalidSide"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketView {
    pub admin: String,
    pub updater: String,
    pub base_mint: String,
    pub quote_mint: String,
    pub base_vault: String,
    pub quote_vault: String,
    pub mid_price: u64,
    pub last_update_slot: u64,
    pub window_start_mid: u64,
    pub ask_consumed: u64,
    pub bid_consumed: u64,
    pub level_size: u64,
    pub max_age_slots: u64,
    pub half_spread_bps: u16,
    pub level_step_bps: u16,
    pub max_move_bps: u16,
    pub num_levels: u8,
    pub paused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Quote {
    pub amount_out: u64,
    pub consumed_after: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Level {
    pub price: u64,
    pub remaining: u64,
}

fn key(bytes: [u8; 32]) -> String {
    Pubkey::new_from_array(bytes).to_string()
}

fn state_of(data: &[u8]) -> Result<MarketState, Name> {
    decode_market(data).map_err(decode_name)
}

pub fn market_view(data: &[u8]) -> Result<MarketView, Name> {
    let s = state_of(data)?;
    Ok(MarketView {
        admin: key(s.admin),
        updater: key(s.updater),
        base_mint: key(s.base_mint),
        quote_mint: key(s.quote_mint),
        base_vault: key(s.base_vault),
        quote_vault: key(s.quote_vault),
        mid_price: s.mid_price,
        last_update_slot: s.last_update_slot,
        window_start_mid: s.window_start_mid,
        ask_consumed: s.ask_consumed_base_atoms,
        bid_consumed: s.bid_consumed_base_atoms,
        level_size: s.level_size_base_atoms,
        max_age_slots: s.max_age_slots,
        half_spread_bps: s.half_spread_bps,
        level_step_bps: s.level_step_bps,
        max_move_bps: s.max_move_bps,
        num_levels: s.num_levels,
        paused: s.is_paused(),
    })
}

pub fn quote(data: &[u8], side: &str, amount_in: u64) -> Result<Quote, Name> {
    let state = state_of(data)?;
    let side = parse_side(side)?;
    let fill = state
        .ladder()
        .quote_exact_in(side, state.consumed(side), amount_in)
        .map_err(pricing_name)?;
    Ok(Quote {
        amount_out: fill.amount_out,
        consumed_after: fill.consumed_after,
    })
}

/// Levels of the side a trader walks: asks for `buy`, bids for `sell`. `remaining` is what is
/// left of each level after the side's consumed amount (levels below the current one are 0).
pub fn ladder_levels(data: &[u8], side: &str) -> Result<Vec<Level>, Name> {
    let state = state_of(data)?;
    let side = parse_side(side)?;
    let ladder = state.ladder();
    let level_size = ladder.level_size_base_atoms;
    let consumed = state.consumed(side);
    (0..ladder.num_levels)
        .map(|i| {
            let price = match side {
                Side::BuyBase => ladder.ask_price(i),
                Side::SellBase => ladder.bid_price(i),
            }
            .map_err(pricing_name)?;
            let start = u64::from(i).checked_mul(level_size).ok_or("Overflow")?;
            let used = consumed.saturating_sub(start).min(level_size);
            Ok(Level {
                price,
                remaining: level_size.saturating_sub(used),
            })
        })
        .collect()
}

pub fn swap_ix_data(side: &str, amount_in: u64, min_out: u64) -> Result<Vec<u8>, Name> {
    Ok(swap_data(parse_side(side)?, amount_in, min_out))
}

/// USD-style UI price: quote tokens per base token.
// Precision loss is irrelevant for a display value.
#[allow(clippy::cast_precision_loss)]
pub fn price_ui(price_scaled: u64, base_decimals: u8, quote_decimals: u8) -> f64 {
    let exp = i32::from(base_decimals) - i32::from(quote_decimals);
    price_scaled as f64 / PRICE_SCALE as f64 * 10_f64.powi(exp)
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsError> {
    value
        .serialize(
            &serde_wasm_bindgen::Serializer::new().serialize_large_number_types_as_bigints(true),
        )
        .map_err(|e| JsError::new(&e.to_string()))
}

fn js_err(name: Name) -> JsError {
    JsError::new(name)
}

/// Decodes a full Market account (8-byte discriminator included).
#[wasm_bindgen(js_name = decodeMarket, unchecked_return_type = "MarketView")]
pub fn decode_market_js(data: &[u8]) -> Result<JsValue, JsError> {
    to_js(&market_view(data).map_err(js_err)?)
}

#[wasm_bindgen(js_name = quoteExactIn, unchecked_return_type = "Quote")]
pub fn quote_exact_in_js(
    data: &[u8],
    #[wasm_bindgen(unchecked_param_type = "Side")] side: &str,
    amount_in: u64,
) -> Result<JsValue, JsError> {
    to_js(&quote(data, side, amount_in).map_err(js_err)?)
}

#[wasm_bindgen(js_name = isFresh)]
pub fn is_fresh_js(current_slot: u64, last_update_slot: u64, max_age_slots: u64) -> bool {
    is_fresh(current_slot, last_update_slot, max_age_slots)
}

#[wasm_bindgen(js_name = swapIxData)]
pub fn swap_ix_data_js(
    #[wasm_bindgen(unchecked_param_type = "Side")] side: &str,
    amount_in: u64,
    min_out: u64,
) -> Result<Vec<u8>, JsError> {
    swap_ix_data(side, amount_in, min_out).map_err(js_err)
}

#[wasm_bindgen(js_name = ladder, unchecked_return_type = "Level[]")]
pub fn ladder_js(
    data: &[u8],
    #[wasm_bindgen(unchecked_param_type = "Side")] side: &str,
) -> Result<JsValue, JsError> {
    to_js(&ladder_levels(data, side).map_err(js_err)?)
}

#[wasm_bindgen(js_name = priceToUi)]
pub fn price_to_ui_js(price_scaled: u64, base_decimals: u8, quote_decimals: u8) -> f64 {
    price_ui(price_scaled, base_decimals, quote_decimals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use propamm_client::{MARKET_ACCOUNT_SIZE, MARKET_DISCRIMINATOR, SWAP_DISCRIMINATOR};

    const FIXTURE_HEX: &str = include_str!("../tests/fixtures/market.hex");
    const GOLDEN: &str = include_str!("../tests/fixtures/golden.json");

    /// SOL/USDC (9/6 decimals) at $200, 10 SOL per level, 8 levels, some consumed on both sides.
    fn fixture_state() -> MarketState {
        MarketState {
            admin: [1; 32],
            updater: [2; 32],
            base_mint: [3; 32],
            quote_mint: [4; 32],
            base_vault: [5; 32],
            quote_vault: [6; 32],
            mid_price: 200_000_000_000,
            last_update_slot: 1_234,
            ask_consumed_base_atoms: 12_000_000_000,
            bid_consumed_base_atoms: 3_000_000_000,
            level_size_base_atoms: 10_000_000_000,
            max_age_slots: 20,
            half_spread_bps: 5,
            level_step_bps: 1,
            max_move_bps: 100,
            num_levels: 8,
            paused: 0,
            bump: 255,
            padding: [0; 7],
            window_start_mid: 199_900_000_000,
        }
    }

    fn account(state: &MarketState) -> Vec<u8> {
        let mut data = MARKET_DISCRIMINATOR.to_vec();
        data.extend_from_slice(bytemuck::bytes_of(state));
        data
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        bytes.iter().fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    fn fixture() -> Vec<u8> {
        let text = FIXTURE_HEX.trim();
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap_or(0))
            .collect()
    }

    /// `UPDATE_FIXTURES=1 cargo test -p propamm-wasm regenerate -- --ignored` rewrites the fixture.
    #[test]
    #[ignore = "writes tests/fixtures; run on purpose"]
    fn regenerate_fixtures() {
        let data = account(&fixture_state());
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        std::fs::write(format!("{dir}/market.hex"), hex(&data) + "\n").unwrap();
        let buy = quote(&data, "buy", 1_000_000_000).unwrap();
        let sell = quote(&data, "sell", 25_000_000_000).unwrap();
        let golden = format!(
            "{{\n  \"buy\": {{ \"amountIn\": \"1000000000\", \"amountOut\": \"{}\", \"consumedAfter\": \"{}\" }},\n  \"sell\": {{ \"amountIn\": \"25000000000\", \"amountOut\": \"{}\", \"consumedAfter\": \"{}\" }}\n}}\n",
            buy.amount_out, buy.consumed_after, sell.amount_out, sell.consumed_after
        );
        std::fs::write(format!("{dir}/golden.json"), golden).unwrap();
    }

    #[test]
    fn fixture_file_matches_the_state() {
        assert_eq!(fixture(), account(&fixture_state()));
        assert_eq!(fixture().len(), MARKET_ACCOUNT_SIZE);
    }

    #[test]
    fn decodes_the_market() {
        let v = market_view(&fixture()).unwrap();
        assert_eq!(v.admin, key([1; 32]));
        assert_eq!(v.quote_vault, key([6; 32]));
        assert_eq!(v.mid_price, 200_000_000_000);
        assert_eq!(v.last_update_slot, 1_234);
        assert_eq!(v.window_start_mid, 199_900_000_000);
        assert_eq!(
            (v.ask_consumed, v.bid_consumed),
            (12_000_000_000, 3_000_000_000)
        );
        assert_eq!((v.level_size, v.max_age_slots), (10_000_000_000, 20));
        assert_eq!(
            (v.half_spread_bps, v.level_step_bps, v.max_move_bps),
            (5, 1, 100)
        );
        assert_eq!(v.num_levels, 8);
        assert!(!v.paused);
    }

    #[test]
    fn decode_errors_are_named() {
        assert_eq!(market_view(&[0; 10]), Err("WrongSize"));
        let mut bad = fixture();
        bad[0] ^= 0xff;
        assert_eq!(market_view(&bad), Err("WrongDiscriminator"));
    }

    fn golden(side: &str) -> (u64, u64, u64) {
        let v: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
        let n = |k: &str| v[side][k].as_str().unwrap().parse().unwrap();
        (n("amountIn"), n("amountOut"), n("consumedAfter"))
    }

    #[test]
    fn quotes_match_the_golden_file() {
        for side in ["buy", "sell"] {
            let (amount_in, amount_out, consumed_after) = golden(side);
            assert!(amount_out > 0, "golden file is empty");
            let q = quote(&fixture(), side, amount_in).unwrap();
            assert_eq!(
                (q.amount_out, q.consumed_after),
                (amount_out, consumed_after)
            );
        }
    }

    #[test]
    fn quote_matches_the_pricing_crate_directly() {
        // 12 SOL already consumed on the ask side: the quote starts inside level 1.
        let state = fixture_state();
        let direct = state
            .ladder()
            .quote_exact_in(Side::BuyBase, state.ask_consumed_base_atoms, 1_000_000_000)
            .unwrap();
        let q = quote(&fixture(), "buy", 1_000_000_000).unwrap();
        assert_eq!(
            (q.amount_out, q.consumed_after),
            (direct.amount_out, direct.consumed_after)
        );
    }

    #[test]
    fn quote_errors_are_named() {
        let data = fixture();
        assert_eq!(quote(&data, "buy", 0), Err("ZeroInput"));
        assert_eq!(quote(&data, "sell", 1), Err("ZeroOutput"));
        assert_eq!(quote(&data, "sell", u64::MAX / 4), Err("InsufficientDepth"));
        assert_eq!(quote(&data, "hold", 1), Err("InvalidSide"));
        let mut unpriced = fixture_state();
        unpriced.mid_price = 0;
        assert_eq!(
            quote(&account(&unpriced), "buy", 1_000),
            Err("InvalidParams")
        );
    }

    #[test]
    fn ladder_marks_consumed_levels() {
        let asks = ladder_levels(&fixture(), "buy").unwrap();
        assert_eq!(asks.len(), 8);
        // 12 SOL consumed with 10 SOL levels: level 0 empty, level 1 has 8 left, the rest full.
        assert_eq!(asks[0].remaining, 0);
        assert_eq!(asks[1].remaining, 8_000_000_000);
        assert_eq!(asks[2].remaining, 10_000_000_000);
        // 200 * (1 + 5 bps) = 200.10 -> scaled; level 1 adds one step.
        assert_eq!(asks[0].price, 200_100_000_000);
        assert_eq!(asks[1].price, 200_120_000_000);
        let bids = ladder_levels(&fixture(), "sell").unwrap();
        assert_eq!(bids[0].remaining, 7_000_000_000);
        assert_eq!(bids[1].remaining, 10_000_000_000);
        assert_eq!(bids[0].price, 199_900_000_000);
        assert!(ladder_levels(&fixture(), "x").is_err());
    }

    #[test]
    fn ladder_past_depth_is_all_zero() {
        let mut s = fixture_state();
        s.ask_consumed_base_atoms = 80_000_000_000;
        let asks = ladder_levels(&account(&s), "buy").unwrap();
        assert!(asks.iter().all(|l| l.remaining == 0));
    }

    #[test]
    fn swap_data_layout() {
        let d = swap_ix_data("sell", 5, 7).unwrap();
        assert_eq!(d.len(), 25);
        assert_eq!(&d[..8], &SWAP_DISCRIMINATOR);
        assert_eq!(d[8], 1);
        assert_eq!(&d[9..17], &5_u64.to_le_bytes());
        assert_eq!(&d[17..], &7_u64.to_le_bytes());
        assert_eq!(swap_ix_data("buy", 1, 0).unwrap()[8], 0);
        assert_eq!(swap_ix_data("nope", 1, 0), Err("InvalidSide"));
    }

    #[test]
    fn price_to_ui_scales_by_decimals() {
        assert!((price_ui(200_000_000_000, 9, 6) - 200.0).abs() < 1e-9);
        assert!((price_ui(1_000_000_000_000, 6, 6) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn freshness_boundary() {
        assert!(is_fresh(120, 100, 20));
        assert!(!is_fresh(121, 100, 20));
    }
}
