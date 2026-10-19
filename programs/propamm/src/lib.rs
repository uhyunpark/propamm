//! `PropAMM` program: market, roles, funds and price updates and swap.
#![deny(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used
)]
// Anchor's `#[program]` and `#[derive(Accounts)]` expect glob imports of the instruction modules.
#![allow(clippy::wildcard_imports)]
// Anchor handlers take `Context<T>` and params by value by design.
#![allow(clippy::needless_pass_by_value)]
// Handlers return anchor `Result`; the error enum documents the failures.
#![allow(clippy::missing_errors_doc)]

use anchor_lang::prelude::*;

pub mod error;
#[cfg(feature = "fast-update")]
pub mod fast;
pub mod instructions;
pub mod state;

use instructions::*;
use state::MarketParams;

declare_id!("2PuFKsYDoYaVWFnSxVGgbD2Xvsz3nymLu2WuctvF6V3F");

#[program]
pub mod propamm {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>, params: InitParams) -> Result<()> {
        initialize::handler(ctx, params)
    }

    pub fn deposit(ctx: Context<Funds>, amount: u64) -> Result<()> {
        funds::deposit(ctx, amount)
    }

    pub fn withdraw(ctx: Context<Funds>, amount: u64) -> Result<()> {
        funds::withdraw(ctx, amount)
    }

    pub fn set_params(ctx: Context<AdminOnly>, params: MarketParams) -> Result<()> {
        admin::set_params(ctx, params)
    }

    pub fn set_updater(ctx: Context<AdminOnly>, new_updater: Pubkey) -> Result<()> {
        admin::set_updater(ctx, new_updater)
    }

    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        admin::set_paused(ctx, paused)
    }

    pub fn update_price(ctx: Context<UpdatePrice>, mid: u64) -> Result<()> {
        price::update_price(ctx, mid)
    }

    pub fn swap(ctx: Context<Swap>, side: SwapSide, amount_in: u64, min_out: u64) -> Result<()> {
        instructions::swap::swap(ctx, side, amount_in, min_out)
    }

    pub fn force_price(ctx: Context<AdminOnly>, mid: u64) -> Result<()> {
        admin::force_price(ctx, mid)
    }

    /// Data that matches no discriminator. With `fast-update`, a 1-byte-tag raw `update_price`
    /// (see `fast.rs`); otherwise the same `InstructionFallbackNotFound` Anchor returns when a
    /// program defines no fallback, so the default build behaves exactly as before.
    pub fn fallback<'info>(
        program_id: &Pubkey,
        accounts: &'info [AccountInfo<'info>],
        data: &[u8],
    ) -> Result<()> {
        #[cfg(feature = "fast-update")]
        if data.first() == Some(&fast::FAST_UPDATE_PRICE_TAG) {
            return fast::update_price(program_id, accounts, data);
        }
        #[cfg(not(feature = "fast-update"))]
        let _ = (program_id, accounts, data);
        Err(anchor_lang::error::ErrorCode::InstructionFallbackNotFound.into())
    }
}
