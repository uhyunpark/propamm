//! Raw `update_price` through the Anchor `fallback` (feature `fast-update`, CU ladder).
//!
//! Wire format: `[FAST_UPDATE_PRICE_TAG, mid as u64 LE]`, accounts `[updater (signer), market
//! (writable)]`, the same order as the Anchor `UpdatePrice` accounts. It skips Anchor's
//! per-instruction work (log line, Borsh, `try_accounts`, `exit`) but still pays the Anchor
//! entrypoint's account deserialization. Every check of `UpdatePrice` is done by hand below, in
//! the order the normal handler runs them, so any faulty input returns the same error code as
//! `update_price`; the state transition is shared (`apply_update_price`).
//!
//! Test-only build: it is compiled to `target/deploy/fast/propamm.so` for the CU ladder and is
//! never the deployed program (see `programs/propamm/CLAUDE.md`).

use anchor_lang::Discriminator;
use anchor_lang::error::ErrorCode;
use anchor_lang::prelude::*;
use propamm_pricing::{MARKET_STATE_SIZE, MarketState};

use crate::error::PropAmmError;
use crate::instructions::price::apply_update_price;
use crate::state::Market;

/// First byte of a fast-path instruction. Anchor dispatch matches whole 8-byte discriminators,
/// so data starting with a byte that begins no discriminator can never reach a normal handler.
pub const FAST_UPDATE_PRICE_TAG: u8 = 0x01;

/// Instruction data length: tag + `mid`.
const DATA_LEN: usize = 9;

const fn first_byte_differs(disc: &[u8]) -> bool {
    !matches!(disc.first(), Some(b) if *b == FAST_UPDATE_PRICE_TAG)
}

// Compile-time collision check against every instruction discriminator and the event-CPI tag
// that Anchor's dispatch tests before calling the fallback.
const _: () = {
    use crate::instruction as ix;
    assert!(first_byte_differs(ix::Initialize::DISCRIMINATOR));
    assert!(first_byte_differs(ix::Deposit::DISCRIMINATOR));
    assert!(first_byte_differs(ix::Withdraw::DISCRIMINATOR));
    assert!(first_byte_differs(ix::SetParams::DISCRIMINATOR));
    assert!(first_byte_differs(ix::SetUpdater::DISCRIMINATOR));
    assert!(first_byte_differs(ix::SetPaused::DISCRIMINATOR));
    assert!(first_byte_differs(ix::UpdatePrice::DISCRIMINATOR));
    assert!(first_byte_differs(ix::Swap::DISCRIMINATOR));
    assert!(first_byte_differs(ix::ForcePrice::DISCRIMINATOR));
    assert!(first_byte_differs(anchor_lang::event::EVENT_IX_TAG_LE));
};

/// The `UpdatePrice` checks in the normal handler's order: updater `Signer`, market owner,
/// discriminator (missing, then wrong), `mut`, size (`load`), stored updater. Then the same
/// state transition as `update_price`.
pub fn update_price(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> Result<()> {
    let mid = data
        .get(1..DATA_LEN)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_le_bytes)
        .ok_or(ErrorCode::InstructionDidNotDeserialize)?;
    let [updater, market, ..] = accounts else {
        return Err(ErrorCode::AccountNotEnoughKeys.into());
    };
    if !updater.is_signer {
        return Err(ErrorCode::AccountNotSigner.into());
    }
    if market.owner != program_id {
        return Err(ErrorCode::AccountOwnedByWrongProgram.into());
    }
    let mut data = market.try_borrow_mut_data()?;
    let Some((disc, body)) = data.split_at_mut_checked(Market::DISCRIMINATOR.len()) else {
        return Err(ErrorCode::AccountDiscriminatorNotFound.into());
    };
    if disc != Market::DISCRIMINATOR {
        return Err(ErrorCode::AccountDiscriminatorMismatch.into());
    }
    if !market.is_writable {
        return Err(ErrorCode::ConstraintMut.into());
    }
    // Like `AccountLoader::load`: at least `MARKET_SPACE`, extra trailing bytes are ignored.
    let body = body
        .get_mut(..MARKET_STATE_SIZE)
        .ok_or(ErrorCode::AccountDidNotDeserialize)?;
    let state: &mut MarketState =
        bytemuck::try_from_bytes_mut(body).map_err(|_| ErrorCode::AccountDidNotDeserialize)?;
    if state.updater != updater.key.to_bytes() {
        return Err(PropAmmError::Unauthorized.into());
    }
    apply_update_price(state, mid, Clock::get()?.slot)
}
