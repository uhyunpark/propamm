//! `swap`: the Anchor `Swap` accounts struct and handler.
//!
//! Direction comes only from `side`; vaults and mints are taken by role and checked against the
//! stored addresses. Unlike the Anchor program this emits no `SwapEvent` log.

use pinocchio::cpi::Signer;
use pinocchio::error::ProgramError;
use pinocchio::sysvars::Sysvar;
use pinocchio::sysvars::clock::Clock;
use pinocchio::{AccountView, ProgramResult};
use pinocchio_token::instructions::TransferChecked;
use propamm_pricing::{Side, is_fresh};

use crate::checks::{self, TOKEN_PROGRAM_ID, next};
use crate::error::Error;
use crate::state::{Args, MarketSeeds};

/// One side of the pair: mint, vault, the trader's token account.
struct Leg<'a> {
    mint: &'a AccountView,
    decimals: u8,
    vault: &'a AccountView,
    /// Vault balance read during validation (Anchor reads its deserialized copy too).
    vault_amount: u64,
    trader_token: &'a AccountView,
}

/// Validated `Swap` accounts.
struct SwapAccounts<'a> {
    trader: &'a AccountView,
    market: &'a mut AccountView,
    base: Leg<'a>,
    quote: Leg<'a>,
}

/// Accounts: `trader` (signer), `market` (`mut`), `base_mint` / `quote_mint` (`Account<Mint>`,
/// `address =` stored mints else `InvalidMint`), `base_vault` / `quote_vault` (`mut`,
/// `address =` stored vaults else `InvalidVault`), `trader_base` / `trader_quote` (`mut`,
/// `token::mint` = the fixed mint, `token::authority = trader`), `token_program` (SPL Token).
fn swap_accounts(accounts: &mut [AccountView]) -> Result<SwapAccounts<'_>, ProgramError> {
    // try_accounts: field types in declaration order.
    let mut it = accounts.iter_mut();
    let trader = next(&mut it)?;
    checks::signer(trader)?;
    let market = next(&mut it)?;
    checks::market(market)?;
    let base_mint = next(&mut it)?;
    let base_decimals = checks::mint(base_mint)?;
    let quote_mint = next(&mut it)?;
    let quote_decimals = checks::mint(quote_mint)?;
    let base_vault = next(&mut it)?;
    let base_vault_data = checks::token_account(base_vault)?;
    let quote_vault = next(&mut it)?;
    let quote_vault_data = checks::token_account(quote_vault)?;
    let trader_base = next(&mut it)?;
    let trader_base_data = checks::token_account(trader_base)?;
    let trader_quote = next(&mut it)?;
    let trader_quote_data = checks::token_account(trader_quote)?;
    let token_program = next(&mut it)?;
    checks::program(token_program, &TOKEN_PROGRAM_ID)?;
    checks::no_duplicates(&[base_vault, quote_vault, trader_base, trader_quote])?;

    // Constraints in field order.
    checks::writable(market)?;
    {
        let state = checks::load(market)?;
        if base_mint.address().to_bytes() != state.base_mint
            || quote_mint.address().to_bytes() != state.quote_mint
        {
            return Err(Error::InvalidMint.into());
        }
        checks::writable(base_vault)?;
        if base_vault.address().to_bytes() != state.base_vault {
            return Err(Error::InvalidVault.into());
        }
        checks::writable(quote_vault)?;
        if quote_vault.address().to_bytes() != state.quote_vault {
            return Err(Error::InvalidVault.into());
        }
    }
    let trader_key = trader.address().to_bytes();
    for (token, data, mint) in [
        (&*trader_base, &trader_base_data, &*base_mint),
        (&*trader_quote, &trader_quote_data, &*quote_mint),
    ] {
        checks::writable(token)?;
        if data.owner != trader_key {
            return Err(Error::ConstraintTokenOwner.into());
        }
        if data.mint != mint.address().to_bytes() {
            return Err(Error::ConstraintTokenMint.into());
        }
    }
    Ok(SwapAccounts {
        trader,
        market,
        base: Leg {
            mint: base_mint,
            decimals: base_decimals,
            vault: base_vault,
            vault_amount: base_vault_data.amount,
            trader_token: trader_base,
        },
        quote: Leg {
            mint: quote_mint,
            decimals: quote_decimals,
            vault: quote_vault,
            vault_amount: quote_vault_data.amount,
            trader_token: trader_quote,
        },
    })
}

/// Args: `side: u8` (0 `BuyBase`, 1 `SellBase`), `amount_in: u64`, `min_out: u64`.
pub fn swap(accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let mut args = Args::new(args);
    let side = match args.u8()? {
        0 => Side::BuyBase,
        1 => Side::SellBase,
        _ => return Err(Error::InstructionDidNotDeserialize.into()),
    };
    let amount_in = args.u64()?;
    let min_out = args.u64()?;
    let a = swap_accounts(accounts)?;
    // Trader pays into the `pay` leg's vault and receives from the `get` leg's vault.
    let (pay, get) = match side {
        Side::BuyBase => (&a.quote, &a.base),
        Side::SellBase => (&a.base, &a.quote),
    };

    let slot = Clock::get()?.slot;
    let (amount_out, seeds) = {
        let mut state = checks::load_mut(a.market)?;
        if state.is_paused() {
            return Err(Error::Paused.into());
        }
        if state.mid_price == 0 {
            return Err(Error::NotInitializedPrice.into());
        }
        if !is_fresh(slot, state.last_update_slot, state.max_age_slots) {
            return Err(Error::StalePrice.into());
        }
        let fill = state
            .ladder()
            .quote_exact_in(side, state.consumed(side), amount_in)
            .map_err(Error::from)?;
        if fill.amount_out < min_out {
            return Err(Error::SlippageExceeded.into());
        }
        if get.vault_amount < fill.amount_out {
            return Err(Error::InsufficientVaultBalance.into());
        }
        state.set_consumed(side, fill.consumed_after);
        (fill.amount_out, MarketSeeds::new(&state))
    };

    TransferChecked::new(
        pay.trader_token,
        pay.mint,
        pay.vault,
        a.trader,
        amount_in,
        pay.decimals,
    )
    .invoke()?;
    let seeds = seeds.as_seeds();
    TransferChecked::new(
        get.vault,
        get.mint,
        get.trader_token,
        a.market,
        amount_out,
        get.decimals,
    )
    .invoke_signed(&[Signer::from(&seeds)])
}
