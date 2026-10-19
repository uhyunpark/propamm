//! `update_price`: the Anchor `UpdatePrice` accounts struct and handler.

use pinocchio::sysvars::Sysvar;
use pinocchio::sysvars::clock::Clock;
use pinocchio::{AccountView, ProgramResult};
use propamm_pricing::within_move_limit;

use crate::checks;
use crate::error::Error;
use crate::state::Args;

/// Accounts: `updater` (signer), `market` (`AccountLoader<Market>`, `mut`,
/// `market.updater == updater` else `Unauthorized`; the admin is not accepted). Args: `mid: u64`.
pub fn update_price(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let mid = Args::new(args).u64()?;
    let (updater_key, mut state) = checks::signer_and_market_mut(accounts)?;
    if state.updater != updater_key {
        return Err(Error::Unauthorized.into());
    }

    let slot = Clock::get()?.slot;
    if state.is_paused() {
        return Err(Error::Paused.into());
    }
    if state.mid_price == 0 {
        return Err(Error::NotInitializedPrice.into());
    }
    state.roll_window(slot).map_err(Error::from)?;
    // One update per slot: the reference is the mid left by the previous update.
    if !within_move_limit(state.window_start_mid, mid, state.max_move_bps).map_err(Error::from)? {
        return Err(Error::PriceMoveTooLarge.into());
    }
    state.validate_mid(mid).map_err(Error::from)?;
    state.mid_price = mid;
    state.last_update_slot = slot;
    Ok(())
}
