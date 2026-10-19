use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use super::token::transfer;
use crate::error::PropAmmError;
use crate::state::Market;

/// Accounts shared by `deposit` and `withdraw`.
#[derive(Accounts)]
pub struct Funds<'info> {
    pub admin: Signer<'info>,
    #[account(
        constraint = market.load()?.is_admin(&admin.key()) @ PropAmmError::Unauthorized,
    )]
    pub market: AccountLoader<'info, Market>,
    #[account(
        constraint = market.load()?.vault_for(&mint.key()).is_some() @ PropAmmError::InvalidMint,
    )]
    pub mint: Account<'info, Mint>,
    #[account(
        mut,
        constraint = market.load()?.vault_for(&mint.key()) == Some(vault.key().to_bytes())
            @ PropAmmError::InvalidVault,
    )]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = admin)]
    pub admin_token: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

pub fn deposit(ctx: Context<Funds>, amount: u64) -> Result<()> {
    let a = &ctx.accounts;
    transfer(
        &a.token_program,
        &a.admin_token,
        &a.mint,
        &a.vault,
        a.admin.to_account_info(),
        amount,
        &[],
    )
}

pub fn withdraw(ctx: Context<Funds>, amount: u64) -> Result<()> {
    let a = &ctx.accounts;
    let seeds = a.market.load()?.signer_seeds();
    transfer(
        &a.token_program,
        &a.vault,
        &a.mint,
        &a.admin_token,
        a.market.to_account_info(),
        amount,
        &[&seeds.as_seeds()],
    )
}
