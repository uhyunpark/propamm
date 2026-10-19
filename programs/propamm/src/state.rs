use anchor_lang::prelude::*;
use propamm_pricing::{Ladder, MARKET_STATE_SIZE, MarketState};

use crate::error::PropAmmError;

/// Account size: 8-byte Anchor discriminator + the shared pricing-crate layout.
pub const MARKET_SPACE: usize = 8 + MARKET_STATE_SIZE;

pub const MARKET_SEED: &[u8] = b"market";
pub const VAULT_SEED: &[u8] = b"vault";

/// Largest accepted `max_move_bps` (1%): bounds what a stolen updater key can move per slot.
pub const MAX_MOVE_BPS: u16 = 100;
/// Largest accepted `max_age_slots` (about 4 minutes at 250 ms): prevents disabling staleness.
pub const MAX_AGE_SLOTS: u64 = 1_000;

/// Rejects the default key and the admin as updater: the hot key must differ from the cold key.
pub fn validate_updater(updater: &Pubkey, admin: &Pubkey) -> Result<()> {
    require!(
        *updater != Pubkey::default() && updater != admin,
        PropAmmError::InvalidUpdater
    );
    Ok(())
}

/// Zero-copy market account. The layout lives in `propamm-pricing` so other programs can share it.
#[account(zero_copy)]
#[repr(C)]
pub struct Market {
    pub state: MarketState,
}

/// Parameters the admin can change after initialization.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub struct MarketParams {
    pub level_size_base_atoms: u64,
    pub num_levels: u8,
    pub half_spread_bps: u16,
    pub level_step_bps: u16,
    pub max_age_slots: u64,
    pub max_move_bps: u16,
}

impl MarketParams {
    /// Ladder checks come from the pricing crate; the mid is checked at the first price update.
    pub fn validate(&self) -> Result<()> {
        Ladder {
            mid_price: 0,
            half_spread_bps: self.half_spread_bps,
            level_step_bps: self.level_step_bps,
            level_size_base_atoms: self.level_size_base_atoms,
            num_levels: self.num_levels,
        }
        .validate_shape()
        .map_err(PropAmmError::from)?;
        require!(
            self.max_age_slots > 0 && self.max_age_slots <= MAX_AGE_SLOTS,
            PropAmmError::InvalidParams
        );
        require!(
            self.max_move_bps > 0 && self.max_move_bps <= MAX_MOVE_BPS,
            PropAmmError::InvalidParams
        );
        Ok(())
    }

    pub fn apply(&self, state: &mut MarketState) {
        state.level_size_base_atoms = self.level_size_base_atoms;
        state.num_levels = self.num_levels;
        state.half_spread_bps = self.half_spread_bps;
        state.level_step_bps = self.level_step_bps;
        state.max_age_slots = self.max_age_slots;
        state.max_move_bps = self.max_move_bps;
    }
}

/// Owned copy of the market PDA seeds, so the account borrow can be dropped before a CPI.
pub struct MarketSeeds {
    admin: [u8; 32],
    base_mint: [u8; 32],
    quote_mint: [u8; 32],
    bump: [u8; 1],
}

impl MarketSeeds {
    pub fn as_seeds(&self) -> [&[u8]; 5] {
        [
            MARKET_SEED,
            &self.admin,
            &self.base_mint,
            &self.quote_mint,
            &self.bump,
        ]
    }
}

impl Market {
    /// Signer seeds rebuilt from stored state, matching `initialize` exactly.
    pub fn signer_seeds(&self) -> MarketSeeds {
        MarketSeeds {
            admin: self.state.admin,
            base_mint: self.state.base_mint,
            quote_mint: self.state.quote_mint,
            bump: [self.state.bump],
        }
    }

    pub fn is_admin(&self, key: &Pubkey) -> bool {
        self.state.admin == key.to_bytes()
    }

    /// The vault that holds `mint`, if `mint` is one of the market's mints.
    pub fn vault_for(&self, mint: &Pubkey) -> Option<[u8; 32]> {
        let mint = mint.to_bytes();
        if mint == self.state.base_mint {
            Some(self.state.base_vault)
        } else if mint == self.state.quote_mint {
            Some(self.state.quote_vault)
        } else {
            None
        }
    }
}
