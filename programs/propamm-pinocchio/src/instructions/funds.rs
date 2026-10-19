//! `deposit` / `withdraw`: the Anchor `Funds` accounts struct and handlers.

use pinocchio::cpi::Signer;
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, ProgramResult};
use pinocchio_token::instructions::TransferChecked;

use crate::checks::{self, TOKEN_PROGRAM_ID, next};
use crate::error::Error;
use crate::state::{Args, MarketSeeds, vault_for};

/// Validated `Funds` accounts.
struct Funds<'a> {
    admin: &'a AccountView,
    market: &'a AccountView,
    mint: &'a AccountView,
    vault: &'a AccountView,
    admin_token: &'a AccountView,
    decimals: u8,
    seeds: MarketSeeds,
}

/// Accounts: `admin` (signer), `market` (`AccountLoader<Market>`, read-only), `mint`
/// (`Account<Mint>`), `vault` (`Account<TokenAccount>`, `mut`), `admin_token`
/// (`Account<TokenAccount>`, `mut`, `token::mint = mint`, `token::authority = admin`),
/// `token_program` (SPL Token).
fn funds(accounts: &mut [AccountView]) -> Result<Funds<'_>, ProgramError> {
    let mut it = accounts.iter_mut();
    let admin = next(&mut it)?;
    checks::signer(admin)?;
    let market = next(&mut it)?;
    checks::market(market)?;
    let mint = next(&mut it)?;
    let decimals = checks::mint(mint)?;
    let vault = next(&mut it)?;
    checks::token_account(vault)?;
    let admin_token = next(&mut it)?;
    let admin_token_data = checks::token_account(admin_token)?;
    let token_program = next(&mut it)?;
    checks::program(token_program, &TOKEN_PROGRAM_ID)?;
    checks::no_duplicates(&[vault, admin_token])?;

    let admin_key = admin.address().to_bytes();
    let mint_key = mint.address().to_bytes();
    let seeds = {
        let state = checks::load(market)?;
        if state.admin != admin_key {
            return Err(Error::Unauthorized.into());
        }
        let expected_vault = vault_for(&state, &mint_key).ok_or(Error::InvalidMint)?;
        checks::writable(vault)?;
        if expected_vault != vault.address().to_bytes() {
            return Err(Error::InvalidVault.into());
        }
        MarketSeeds::new(&state)
    };
    checks::writable(admin_token)?;
    if admin_token_data.owner != admin_key {
        return Err(Error::ConstraintTokenOwner.into());
    }
    if admin_token_data.mint != mint_key {
        return Err(Error::ConstraintTokenMint.into());
    }
    Ok(Funds {
        admin,
        market,
        mint,
        vault,
        admin_token,
        decimals,
        seeds,
    })
}

/// Args: `amount: u64`. Admin token account -> vault, signed by the admin.
pub fn deposit(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let amount = Args::new(args).u64()?;
    let f = funds(accounts)?;
    TransferChecked::new(f.admin_token, f.mint, f.vault, f.admin, amount, f.decimals).invoke()
}

/// Args: `amount: u64`. Vault -> admin token account, signed by the market PDA.
pub fn withdraw(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let amount = Args::new(args).u64()?;
    let f = funds(accounts)?;
    let seeds = f.seeds.as_seeds();
    TransferChecked::new(f.vault, f.mint, f.admin_token, f.market, amount, f.decimals)
        .invoke_signed(&[Signer::from(&seeds)])
}
