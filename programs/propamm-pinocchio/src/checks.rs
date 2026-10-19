//! Hand-written equivalents of the Anchor account types and constraints used by the Anchor
//! program. Each helper names the Anchor check it replaces and returns the same error.

use core::slice::IterMut;

use pinocchio::account::{Ref, RefMut};
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address};
use propamm_pricing::MarketState;

use crate::error::Error;
use crate::state::{MARKET_DISCRIMINATOR, MARKET_LEN};

pub const TOKEN_PROGRAM_ID: Address = pinocchio_token::ID;
pub const SYSTEM_PROGRAM_ID: Address = pinocchio_system::ID;

/// SPL Token account and mint sizes (`spl_token::state::{Account, Mint}::LEN`).
pub const TOKEN_ACCOUNT_LEN: usize = 165;
const MINT_LEN: usize = 82;

/// Next account in declaration order (`try_accounts` consumes them one by one), or
/// `AccountNotEnoughKeys`.
pub fn next<'a>(it: &mut IterMut<'a, AccountView>) -> Result<&'a mut AccountView, Error> {
    it.next().ok_or(Error::AccountNotEnoughKeys)
}

/// `Signer<'info>`.
pub fn signer(a: &AccountView) -> Result<(), Error> {
    if !a.is_signer() {
        return Err(Error::AccountNotSigner);
    }
    Ok(())
}

/// `#[account(mut)]`.
pub fn writable(a: &AccountView) -> Result<(), Error> {
    if !a.is_writable() {
        return Err(Error::ConstraintMut);
    }
    Ok(())
}

/// `Program<'info, T>`: the expected address, then executable.
pub fn program(a: &AccountView, id: &Address) -> Result<(), Error> {
    if a.address() != id {
        return Err(Error::InvalidProgramId);
    }
    if !a.executable() {
        return Err(Error::InvalidProgramExecutable);
    }
    Ok(())
}

/// `AccountLoader<'info, Market>::try_accounts`: owned by this program, discriminator present
/// and equal to `Market`'s.
pub fn market(a: &AccountView) -> Result<(), ProgramError> {
    if !a.owned_by(&crate::ID) {
        return Err(Error::AccountOwnedByWrongProgram.into());
    }
    let data = a.try_borrow()?;
    let disc = data
        .first_chunk::<8>()
        .ok_or(Error::AccountDiscriminatorNotFound)?;
    if *disc != MARKET_DISCRIMINATOR {
        return Err(Error::AccountDiscriminatorMismatch.into());
    }
    Ok(())
}

/// `AccountLoader::load`, after [`market`]: the body as `MarketState`. Anchor accepts any size
/// `>= MARKET_LEN`; this requires exactly `MARKET_LEN`, the only size `initialize` creates.
pub fn load(a: &AccountView) -> Result<Ref<'_, MarketState>, ProgramError> {
    Ref::try_map(a.try_borrow()?, |d| {
        d.get(8..)
            .filter(|_| d.len() == MARKET_LEN)
            .and_then(|body| bytemuck::try_from_bytes(body).ok())
            .ok_or(Error::AccountDidNotDeserialize)
    })
    .map_err(|(_, e)| e.into())
}

/// The first two accounts of `UpdatePrice` and `AdminOnly`: a `Signer`, then a writable
/// `AccountLoader<Market>`. Returns the signer's key and the market state.
pub fn signer_and_market_mut(
    accounts: &mut [AccountView],
) -> Result<([u8; 32], RefMut<'_, MarketState>), ProgramError> {
    let mut it = accounts.iter_mut();
    let signer_account = next(&mut it)?;
    signer(signer_account)?;
    let market_account = next(&mut it)?;
    market(market_account)?;
    writable(market_account)?;
    let key = signer_account.address().to_bytes();
    Ok((key, load_mut(market_account)?))
}

/// `AccountLoader::load_mut`, after [`market`] and [`writable`].
pub fn load_mut(a: &mut AccountView) -> Result<RefMut<'_, MarketState>, ProgramError> {
    RefMut::try_map(a.try_borrow_mut()?, |d| {
        let len = d.len();
        d.get_mut(8..)
            .filter(|_| len == MARKET_LEN)
            .and_then(|body| bytemuck::try_from_bytes_mut(body).ok())
            .ok_or(Error::AccountDidNotDeserialize)
    })
    .map_err(|(_, e)| e.into())
}

/// `Account<'info, T>` for an SPL Token type: initialized, then owned by SPL Token.
fn token_owned(a: &AccountView) -> Result<(), Error> {
    if a.owned_by(&SYSTEM_PROGRAM_ID) && a.lamports() == 0 {
        return Err(Error::AccountNotInitialized);
    }
    if !a.owned_by(&TOKEN_PROGRAM_ID) {
        return Err(Error::AccountOwnedByWrongProgram);
    }
    Ok(())
}

/// The fields this program reads from a token account (copied, like Anchor's deserialized
/// `Account<TokenAccount>`, so no borrow is held across CPIs).
pub struct TokenAccount {
    pub mint: [u8; 32],
    pub owner: [u8; 32],
    pub amount: u64,
}

/// `Account<'info, TokenAccount>`: owner checks, then `spl_token` `unpack` (exact length,
/// initialized state).
pub fn token_account(a: &AccountView) -> Result<TokenAccount, ProgramError> {
    token_owned(a)?;
    let d = a.try_borrow()?;
    let d: &[u8; TOKEN_ACCOUNT_LEN] = d
        .as_ref()
        .try_into()
        .map_err(|_| ProgramError::InvalidAccountData)?;
    match d[108] {
        0 => return Err(ProgramError::UninitializedAccount),
        1 | 2 => {}
        _ => return Err(ProgramError::InvalidAccountData),
    }
    let field = |at: usize| -> [u8; 32] {
        let mut out = [0; 32];
        out.copy_from_slice(&d[at..][..32]);
        out
    };
    let mut amount = [0; 8];
    amount.copy_from_slice(&d[64..72]);
    Ok(TokenAccount {
        mint: field(0),
        owner: field(32),
        amount: u64::from_le_bytes(amount),
    })
}

/// `Account<'info, Mint>`: owner checks, then `spl_token` `unpack`. Returns the decimals.
pub fn mint(a: &AccountView) -> Result<u8, ProgramError> {
    token_owned(a)?;
    let d = a.try_borrow()?;
    let d: &[u8; MINT_LEN] = d
        .as_ref()
        .try_into()
        .map_err(|_| ProgramError::InvalidAccountData)?;
    match d[45] {
        0 => Err(ProgramError::UninitializedAccount),
        1 => Ok(d[44]),
        _ => Err(ProgramError::InvalidAccountData),
    }
}

/// Anchor's duplicate-mutable-account check: no two of `accounts` share an address.
pub fn no_duplicates(accounts: &[&AccountView]) -> Result<(), Error> {
    for (i, a) in accounts.iter().enumerate() {
        if accounts[..i].iter().any(|b| b.address() == a.address()) {
            return Err(Error::ConstraintDuplicateMutableAccount);
        }
    }
    Ok(())
}
