use anchor_lang::prelude::*;
use propamm_pricing::{MarketState, within_move_limit};

use crate::error::PropAmmError;
use crate::state::Market;

#[derive(Accounts)]
pub struct UpdatePrice<'info> {
    pub updater: Signer<'info>,
    /// Only the stored updater may post prices. The admin is deliberately not accepted.
    #[account(
        mut,
        constraint = market.load()?.state.updater == updater.key().to_bytes() @ PropAmmError::Unauthorized,
    )]
    pub market: AccountLoader<'info, Market>,
}

/// Hot-key price update. Pause blocks it. See `docs/design-onchain.md` (Guards).
pub fn update_price(ctx: Context<UpdatePrice>, mid: u64) -> Result<()> {
    let slot = Clock::get()?.slot;
    let mut market = ctx.accounts.market.load_mut()?;
    apply_update_price(&mut market.state, mid, slot)
}

/// The `update_price` state transition after the account checks. Shared with the
/// `fast-update` raw path so both run exactly the same logic.
pub(crate) fn apply_update_price(state: &mut MarketState, mid: u64, slot: u64) -> Result<()> {
    require!(!state.is_paused(), PropAmmError::Paused);
    require!(state.mid_price != 0, PropAmmError::NotInitializedPrice);
    state.roll_window(slot).map_err(PropAmmError::from)?;
    // One update per slot: the reference is the mid left by the previous update.
    require!(
        within_move_limit(state.window_start_mid, mid, state.max_move_bps)
            .map_err(PropAmmError::from)?,
        PropAmmError::PriceMoveTooLarge
    );
    state.validate_mid(mid).map_err(PropAmmError::from)?;
    state.mid_price = mid;
    state.last_update_slot = slot;
    Ok(())
}
