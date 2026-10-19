//! `initialize`: the Anchor `Initialize` accounts struct (three `init`s) and handler.

use pinocchio::cpi::{Seed, Signer};
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use pinocchio_system::instructions::{Allocate, Assign, CreateAccount, Transfer};
use pinocchio_token::instructions::InitializeAccount3;

use crate::checks::{self, SYSTEM_PROGRAM_ID, TOKEN_ACCOUNT_LEN, TOKEN_PROGRAM_ID, next};
use crate::error::Error;
use crate::rent::Rent;
use crate::state::{
    Args, MARKET_DISCRIMINATOR, MARKET_LEN, MARKET_SEED, MarketParams, VAULT_SEED, validate_updater,
};

/// Accounts: `admin` (signer, writable, payer), `base_mint`, `quote_mint`, `market`,
/// `base_vault`, `quote_vault` (writable), `token_program`, `system_program`.
/// Args: `updater: [u8; 32]`, then `MarketParams`.
pub fn process(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let mut args = Args::new(args);
    let updater = args.key()?;
    let params = MarketParams::decode(&mut args)?;

    // try_accounts: field types in declaration order.
    let mut it = accounts.iter_mut();
    let admin = next(&mut it)?;
    checks::signer(admin)?;
    let base_mint = next(&mut it)?;
    checks::mint(base_mint)?;
    let quote_mint = next(&mut it)?;
    checks::mint(quote_mint)?;
    let market = next(&mut it)?;
    let base_vault = next(&mut it)?;
    let quote_vault = next(&mut it)?;
    let token_program = next(&mut it)?;
    checks::program(token_program, &TOKEN_PROGRAM_ID)?;
    let system_program = next(&mut it)?;
    checks::program(system_program, &SYSTEM_PROGRAM_ID)?;

    // Constraints: the `init` accounts in order, then `admin` `mut`.
    let rent = Rent::get()?;
    let admin_key = admin.address().to_bytes();
    let base_key = base_mint.address().to_bytes();
    let quote_key = quote_mint.address().to_bytes();

    let (pda, market_bump) = Address::find_program_address(
        &[MARKET_SEED, &admin_key, &base_key, &quote_key],
        &crate::ID,
    );
    if market.address() != &pda {
        return Err(Error::ConstraintSeeds.into());
    }
    let bump = [market_bump];
    let seeds: [Seed; 5] = [
        MARKET_SEED.into(),
        (&admin_key).into(),
        (&base_key).into(),
        (&quote_key).into(),
        (&bump).into(),
    ];
    init_account(admin, market, MARKET_LEN, &crate::ID, &seeds, &rent)?;
    // AccountLoader::try_from_unchecked (owner), `mut`, rent exemption.
    if !market.owned_by(&crate::ID) {
        return Err(Error::AccountOwnedByWrongProgram.into());
    }
    checks::writable(market)?;
    rent_exempt(market, &rent)?;

    init_vault(admin, market, base_mint, base_vault, &rent)?;
    init_vault(admin, market, quote_mint, quote_vault, &rent)?;
    checks::writable(admin)?;

    // Handler.
    params.validate()?;
    if base_key == quote_key {
        return Err(Error::InvalidMint.into());
    }
    validate_updater(&updater, &admin_key)?;
    let base_vault_key = base_vault.address().to_bytes();
    let quote_vault_key = quote_vault.address().to_bytes();
    let mut data = market.try_borrow_mut()?;
    let (disc, body) = data.split_at_mut(MARKET_DISCRIMINATOR.len());
    disc.copy_from_slice(&MARKET_DISCRIMINATOR);
    let state: &mut propamm_pricing::MarketState =
        bytemuck::try_from_bytes_mut(body).map_err(|_| Error::AccountDidNotDeserialize)?;
    state.admin = admin_key;
    state.updater = updater;
    state.base_mint = base_key;
    state.quote_mint = quote_key;
    state.base_vault = base_vault_key;
    state.quote_vault = quote_vault_key;
    state.bump = market_bump;
    // mid_price, last_update_slot, consumed and paused stay zero: not tradable until the first force_price.
    params.apply(state);
    Ok(())
}

/// One vault `init` (`seeds = [VAULT_SEED, market, mint]`, `token::mint = mint`,
/// `token::authority = market`), then the `Account<TokenAccount>` load, `mut` and rent checks.
fn init_vault(
    admin: &AccountView,
    market: &AccountView,
    mint: &AccountView,
    vault: &mut AccountView,
    rent: &Rent,
) -> ProgramResult {
    let market_key = market.address().to_bytes();
    let mint_key = mint.address().to_bytes();
    let (pda, vault_bump) =
        Address::find_program_address(&[VAULT_SEED, &market_key, &mint_key], &crate::ID);
    if vault.address() != &pda {
        return Err(Error::ConstraintSeeds.into());
    }
    let bump = [vault_bump];
    let seeds: [Seed; 4] = [
        VAULT_SEED.into(),
        (&market_key).into(),
        (&mint_key).into(),
        (&bump).into(),
    ];
    init_account(
        admin,
        vault,
        TOKEN_ACCOUNT_LEN,
        &TOKEN_PROGRAM_ID,
        &seeds,
        rent,
    )?;
    InitializeAccount3::new(vault, mint, market.address()).invoke()?;
    checks::token_account(vault)?;
    checks::writable(vault)?;
    rent_exempt(vault, rent)
}

/// Anchor's `init` for a PDA: `create_account` when the address holds no lamports; otherwise
/// (pre-funded address) top up to rent exemption, `allocate` and `assign`.
fn init_account(
    payer: &AccountView,
    account: &AccountView,
    space: usize,
    owner: &Address,
    seeds: &[Seed],
    rent: &Rent,
) -> ProgramResult {
    let lamports = rent.minimum_balance(space)?;
    let signer = [Signer::from(seeds)];
    let space = u64::try_from(space).map_err(|_| ProgramError::InvalidArgument)?;
    let current = account.lamports();
    if current == 0 {
        return CreateAccount {
            from: payer,
            to: account,
            lamports,
            space,
            owner,
        }
        .invoke_signed(&signer);
    }
    if payer.address() == account.address() {
        return Err(Error::TryingToInitPayerAsProgramAccount.into());
    }
    let required = lamports.max(1).saturating_sub(current);
    if required > 0 {
        Transfer {
            from: payer,
            to: account,
            lamports: required,
        }
        .invoke()?;
    }
    Allocate { account, space }.invoke_signed(&signer)?;
    Assign { account, owner }.invoke_signed(&signer)
}

/// `rent_exempt = enforce`, which Anchor adds to every `init` account.
fn rent_exempt(a: &AccountView, rent: &Rent) -> ProgramResult {
    if a.lamports() < rent.minimum_balance(a.data_len())? {
        return Err(Error::ConstraintRentExempt.into());
    }
    Ok(())
}
