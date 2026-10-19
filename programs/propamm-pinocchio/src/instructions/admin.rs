//! Instructions on the Anchor `AdminOnly` accounts: `set_params`, `set_updater`, `set_paused`,
//! `force_price`.

use pinocchio::account::RefMut;
use pinocchio::error::ProgramError;
use pinocchio::sysvars::Sysvar;
use pinocchio::sysvars::clock::Clock;
use pinocchio::{AccountView, ProgramResult};
use propamm_pricing::MarketState;

use crate::checks;
use crate::error::Error;
use crate::state::{Args, MarketParams, validate_updater};

/// `AdminOnly`: `admin` (signer), `market` (`AccountLoader<Market>`, `mut`,
/// `market.admin == admin` else `Unauthorized`). Returns the loaded state and the admin key.
fn admin_only(
    accounts: &mut [AccountView],
) -> Result<(RefMut<'_, MarketState>, [u8; 32]), ProgramError> {
    let (admin_key, state) = checks::signer_and_market_mut(accounts)?;
    if state.admin != admin_key {
        return Err(Error::Unauthorized.into());
    }
    Ok((state, admin_key))
}

/// Args: `MarketParams`. Invalidates the current window (`consumed = u64::MAX`).
pub fn set_params(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let params = MarketParams::decode(&mut Args::new(args))?;
    let (mut state, _) = admin_only(accounts)?;
    params.validate()?;
    params.apply(&mut state);
    state.ask_consumed_base_atoms = u64::MAX;
    state.bid_consumed_base_atoms = u64::MAX;
    Ok(())
}

/// Args: `new_updater: [u8; 32]`.
pub fn set_updater(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let new_updater = Args::new(args).key()?;
    let (mut state, admin_key) = admin_only(accounts)?;
    validate_updater(&new_updater, &admin_key)?;
    state.updater = new_updater;
    Ok(())
}

/// Args: `paused: bool`.
pub fn set_paused(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let paused = Args::new(args).bool()?;
    let (mut state, _) = admin_only(accounts)?;
    state.paused = u8::from(paused);
    Ok(())
}

/// Args: `mid: u64`. First-price and unfreeze path: allowed while paused, no move limit.
pub fn force_price(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let mid = Args::new(args).u64()?;
    let (mut state, _) = admin_only(accounts)?;
    let slot = Clock::get()?.slot;
    state.validate_mid(mid).map_err(Error::from)?;
    state.reset_window(mid, slot);
    Ok(())
}
