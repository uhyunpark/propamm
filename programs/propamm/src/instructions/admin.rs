use anchor_lang::prelude::*;

use crate::error::PropAmmError;
use crate::state::{Market, MarketParams, validate_updater};

/// Accounts for every instruction that only changes market config.
#[derive(Accounts)]
pub struct AdminOnly<'info> {
    pub admin: Signer<'info>,
    #[account(
        mut,
        constraint = market.load()?.is_admin(&admin.key()) @ PropAmmError::Unauthorized,
    )]
    pub market: AccountLoader<'info, Market>,
}

pub fn set_params(ctx: Context<AdminOnly>, params: MarketParams) -> Result<()> {
    params.validate()?;
    let mut market = ctx.accounts.market.load_mut()?;
    params.apply(&mut market.state);
    // New params change what the old consumed counters mean (level size, spread, step), so the
    // current window is invalidated: u64::MAX makes the pricing walk return InsufficientDepth
    // until the next price update resets both counters.
    market.state.ask_consumed_base_atoms = u64::MAX;
    market.state.bid_consumed_base_atoms = u64::MAX;
    Ok(())
}

pub fn set_updater(ctx: Context<AdminOnly>, new_updater: Pubkey) -> Result<()> {
    let mut market = ctx.accounts.market.load_mut()?;
    validate_updater(&new_updater, &ctx.accounts.admin.key())?;
    market.state.updater = new_updater.to_bytes();
    Ok(())
}

pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
    ctx.accounts.market.load_mut()?.state.paused = u8::from(paused);
    Ok(())
}

/// First-price and unfreeze path: admin only, allowed while paused, no move limit.
/// Starts a fresh window at `mid`.
pub fn force_price(ctx: Context<AdminOnly>, mid: u64) -> Result<()> {
    let slot = Clock::get()?.slot;
    let mut market = ctx.accounts.market.load_mut()?;
    market.state.validate_mid(mid).map_err(PropAmmError::from)?;
    market.state.reset_window(mid, slot);
    Ok(())
}
