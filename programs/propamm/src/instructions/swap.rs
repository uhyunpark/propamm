use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};
use propamm_pricing::{Side, is_fresh};

use super::token::transfer;
use crate::error::PropAmmError;
use crate::state::Market;

/// Trade direction from the trader's perspective.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum SwapSide {
    /// Pay quote, receive base.
    BuyBase,
    /// Pay base, receive quote.
    SellBase,
}

impl From<SwapSide> for Side {
    fn from(s: SwapSide) -> Self {
        match s {
            SwapSide::BuyBase => Self::BuyBase,
            SwapSide::SellBase => Self::SellBase,
        }
    }
}

#[event]
pub struct SwapEvent {
    pub market: Pubkey,
    pub trader: Pubkey,
    pub side: SwapSide,
    pub amount_in: u64,
    pub amount_out: u64,
    pub consumed_after: u64,
    pub mid_price: u64,
    pub slot: u64,
}

/// Fixed account roles: the direction comes only from `side`, never from the mints passed in.
#[derive(Accounts)]
pub struct Swap<'info> {
    pub trader: Signer<'info>,
    #[account(mut)]
    pub market: AccountLoader<'info, Market>,
    #[account(address = Pubkey::new_from_array(market.load()?.state.base_mint) @ PropAmmError::InvalidMint)]
    pub base_mint: Account<'info, Mint>,
    #[account(address = Pubkey::new_from_array(market.load()?.state.quote_mint) @ PropAmmError::InvalidMint)]
    pub quote_mint: Account<'info, Mint>,
    #[account(mut, address = Pubkey::new_from_array(market.load()?.state.base_vault) @ PropAmmError::InvalidVault)]
    pub base_vault: Account<'info, TokenAccount>,
    #[account(mut, address = Pubkey::new_from_array(market.load()?.state.quote_vault) @ PropAmmError::InvalidVault)]
    pub quote_vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint = base_mint, token::authority = trader)]
    pub trader_base: Account<'info, TokenAccount>,
    #[account(mut, token::mint = quote_mint, token::authority = trader)]
    pub trader_quote: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

/// Exact-in swap against the ladder. Never touches the window: only `consumed` for `side`.
/// See `docs/design-onchain.md` (Swap requirements).
pub fn swap(ctx: Context<Swap>, side: SwapSide, amount_in: u64, min_out: u64) -> Result<()> {
    let a = &ctx.accounts;
    let slot = Clock::get()?.slot;
    let pside = Side::from(side);
    let (fill, mid_price, seeds) = {
        let mut market = a.market.load_mut()?;
        let state = &mut market.state;
        require!(!state.is_paused(), PropAmmError::Paused);
        require!(state.mid_price != 0, PropAmmError::NotInitializedPrice);
        require!(
            is_fresh(slot, state.last_update_slot, state.max_age_slots),
            PropAmmError::StalePrice
        );
        let fill = state
            .ladder()
            .quote_exact_in(pside, state.consumed(pside), amount_in)
            .map_err(PropAmmError::from)?;
        require!(fill.amount_out >= min_out, PropAmmError::SlippageExceeded);
        let out_balance = match side {
            SwapSide::BuyBase => a.base_vault.amount,
            SwapSide::SellBase => a.quote_vault.amount,
        };
        require!(
            out_balance >= fill.amount_out,
            PropAmmError::InsufficientVaultBalance
        );
        state.set_consumed(pside, fill.consumed_after);
        (fill, state.mid_price, market.signer_seeds())
    };

    let (in_from, in_mint, in_to, out_from, out_mint, out_to) = match side {
        SwapSide::BuyBase => (
            &a.trader_quote,
            &a.quote_mint,
            &a.quote_vault,
            &a.base_vault,
            &a.base_mint,
            &a.trader_base,
        ),
        SwapSide::SellBase => (
            &a.trader_base,
            &a.base_mint,
            &a.base_vault,
            &a.quote_vault,
            &a.quote_mint,
            &a.trader_quote,
        ),
    };
    transfer(
        &a.token_program,
        in_from,
        in_mint,
        in_to,
        a.trader.to_account_info(),
        amount_in,
        &[],
    )?;
    transfer(
        &a.token_program,
        out_from,
        out_mint,
        out_to,
        a.market.to_account_info(),
        fill.amount_out,
        &[&seeds.as_seeds()],
    )?;

    emit!(SwapEvent {
        market: a.market.key(),
        trader: a.trader.key(),
        side,
        amount_in,
        amount_out: fill.amount_out,
        consumed_after: fill.consumed_after,
        mid_price,
        slot,
    });
    Ok(())
}
