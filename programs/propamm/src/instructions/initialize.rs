use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::error::PropAmmError;
use crate::state::{MARKET_SEED, MARKET_SPACE, Market, MarketParams, VAULT_SEED, validate_updater};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct InitParams {
    pub updater: Pubkey,
    pub level_size_base_atoms: u64,
    pub num_levels: u8,
    pub half_spread_bps: u16,
    pub level_step_bps: u16,
    pub max_age_slots: u64,
    pub max_move_bps: u16,
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    /// Pays for the accounts and becomes the market admin.
    #[account(mut)]
    pub admin: Signer<'info>,
    pub base_mint: Account<'info, Mint>,
    /// Must differ from `base_mint` (see the guard in `handler`).
    pub quote_mint: Account<'info, Mint>,
    #[account(
        init,
        payer = admin,
        space = MARKET_SPACE,
        seeds = [
            MARKET_SEED,
            admin.key().as_ref(),
            base_mint.key().as_ref(),
            quote_mint.key().as_ref(),
        ],
        bump,
    )]
    pub market: AccountLoader<'info, Market>,
    #[account(
        init,
        payer = admin,
        seeds = [VAULT_SEED, market.key().as_ref(), base_mint.key().as_ref()],
        bump,
        token::mint = base_mint,
        token::authority = market,
    )]
    pub base_vault: Account<'info, TokenAccount>,
    #[account(
        init,
        payer = admin,
        seeds = [VAULT_SEED, market.key().as_ref(), quote_mint.key().as_ref()],
        bump,
        token::mint = quote_mint,
        token::authority = market,
    )]
    pub quote_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<Initialize>, params: InitParams) -> Result<()> {
    let market_params = MarketParams {
        level_size_base_atoms: params.level_size_base_atoms,
        num_levels: params.num_levels,
        half_spread_bps: params.half_spread_bps,
        level_step_bps: params.level_step_bps,
        max_age_slots: params.max_age_slots,
        max_move_bps: params.max_move_bps,
    };
    market_params.validate()?;

    let a = &ctx.accounts;
    // With the current vault seeds, equal mints already fail at vault init (the PDAs collide).
    // This guard keeps the invariant if the seeds ever change.
    require_keys_neq!(
        a.base_mint.key(),
        a.quote_mint.key(),
        PropAmmError::InvalidMint
    );
    validate_updater(&params.updater, &a.admin.key())?;
    let mut market = a.market.load_init()?;
    let state = &mut market.state;
    state.admin = a.admin.key().to_bytes();
    state.updater = params.updater.to_bytes();
    state.base_mint = a.base_mint.key().to_bytes();
    state.quote_mint = a.quote_mint.key().to_bytes();
    state.base_vault = a.base_vault.key().to_bytes();
    state.quote_vault = a.quote_vault.key().to_bytes();
    state.bump = ctx.bumps.market;
    // mid_price, last_update_slot, consumed and paused stay zero: not tradable until the first force_price.
    market_params.apply(state);
    Ok(())
}
