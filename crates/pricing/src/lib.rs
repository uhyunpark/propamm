//! Pricing core for `PropAMM`: quote math and the shared account layout.
//!
//! The same code runs on-chain (program), off-chain (bot, aggregator SDK) and in the
//! browser (WASM), so everything here is deterministic integer math with no `std`.
//!
//! # Units
//!
//! - Price = quote atoms per 1 base atom, multiplied by [`PRICE_SCALE`] (`1e12`).
//!   Example: SOL (9 decimals) / USDC (6 decimals) at $200 is 0.2 quote atoms per base
//!   atom, so the price is `200_000_000_000`.
//! - Supported range: up to `u64::MAX / 1e12`, about 1.8e7 quote atoms per base atom. This
//!   covers 8-decimal BTC-like tokens at $100k (1,000 atoms per atom). Very low-priced
//!   tokens with many decimals lose significant digits to the 1e12 scale.
//! - [`BPS_DENOM`] = 10,000 basis points per 1.0.
//! - Amounts are in atoms. `u128` is used only for intermediates, never in structs.
//!
//! # Model
//!
//! A price ladder per side: `num_levels` levels of `level_size_base_atoms` each, with
//! prices derived from the mid price. See [`Ladder`] for the formulas and rounding.
#![no_std]
#![forbid(unsafe_code)]
#![deny(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

mod error;
mod ladder;
mod math;
mod state;
mod window;

pub use error::PricingError;
pub use ladder::{Fill, Ladder, MAX_LEVELS, Side, is_fresh};
pub use math::{BPS_DENOM, PRICE_SCALE, mul_div_ceil, mul_div_floor};
pub use state::{MARKET_STATE_SIZE, MarketState};
pub use window::{move_band, within_move_limit};
