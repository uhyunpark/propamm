//! Account layout constants, instruction argument decoding and parameter rules (the same rules
//! as the Anchor program's `state.rs`).

use propamm_pricing::{Ladder, MARKET_STATE_SIZE, MarketState};

use crate::error::Error;

/// Anchor's `Market` discriminator: `sha256("account:Market")[..8]`. Market accounts are
/// byte-identical to the Anchor program's.
pub const MARKET_DISCRIMINATOR: [u8; 8] = [0xdb, 0xbe, 0xd5, 0x37, 0x00, 0xe3, 0xc6, 0x9a];
/// Market account size: discriminator + the shared pricing-crate layout.
pub const MARKET_LEN: usize = 8 + MARKET_STATE_SIZE;

pub const MARKET_SEED: &[u8] = b"market";
pub const VAULT_SEED: &[u8] = b"vault";

/// Largest accepted `max_move_bps` (1%).
pub const MAX_MOVE_BPS: u16 = 100;
/// Largest accepted `max_age_slots`.
pub const MAX_AGE_SLOTS: u64 = 1_000;

/// Little-endian argument reader. Like Borsh in Anchor, trailing bytes are ignored.
pub struct Args<'a>(&'a [u8]);

impl<'a> Args<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self(data)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let (head, rest) = self
            .0
            .split_first_chunk::<N>()
            .ok_or(Error::InstructionDidNotDeserialize)?;
        self.0 = rest;
        Ok(*head)
    }

    pub fn u8(&mut self) -> Result<u8, Error> {
        self.take::<1>().map(|[b]| b)
    }

    pub fn u16(&mut self) -> Result<u16, Error> {
        self.take().map(u16::from_le_bytes)
    }

    pub fn u64(&mut self) -> Result<u64, Error> {
        self.take().map(u64::from_le_bytes)
    }

    pub fn key(&mut self) -> Result<[u8; 32], Error> {
        self.take()
    }

    /// Borsh `bool`: exactly 0 or 1.
    pub fn bool(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::InstructionDidNotDeserialize),
        }
    }
}

/// Parameters the admin can change after initialization (Borsh field order of the Anchor type).
#[derive(Clone, Copy)]
pub struct MarketParams {
    pub level_size_base_atoms: u64,
    pub num_levels: u8,
    pub half_spread_bps: u16,
    pub level_step_bps: u16,
    pub max_age_slots: u64,
    pub max_move_bps: u16,
}

impl MarketParams {
    pub fn decode(args: &mut Args) -> Result<Self, Error> {
        Ok(Self {
            level_size_base_atoms: args.u64()?,
            num_levels: args.u8()?,
            half_spread_bps: args.u16()?,
            level_step_bps: args.u16()?,
            max_age_slots: args.u64()?,
            max_move_bps: args.u16()?,
        })
    }

    /// Ladder shape from the pricing crate, plus the staleness and move-limit caps.
    pub fn validate(&self) -> Result<(), Error> {
        Ladder {
            mid_price: 0,
            half_spread_bps: self.half_spread_bps,
            level_step_bps: self.level_step_bps,
            level_size_base_atoms: self.level_size_base_atoms,
            num_levels: self.num_levels,
        }
        .validate_shape()?;
        if self.max_age_slots == 0 || self.max_age_slots > MAX_AGE_SLOTS {
            return Err(Error::InvalidParams);
        }
        if self.max_move_bps == 0 || self.max_move_bps > MAX_MOVE_BPS {
            return Err(Error::InvalidParams);
        }
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

/// Rejects the default key and the admin as updater.
pub fn validate_updater(updater: &[u8; 32], admin: &[u8; 32]) -> Result<(), Error> {
    if *updater == [0; 32] || updater == admin {
        return Err(Error::InvalidUpdater);
    }
    Ok(())
}

/// Owned copy of the market PDA seeds (so the account borrow ends before a CPI), rebuilt from
/// stored state exactly as `initialize` derived them.
pub struct MarketSeeds {
    admin: [u8; 32],
    base_mint: [u8; 32],
    quote_mint: [u8; 32],
    bump: [u8; 1],
}

impl MarketSeeds {
    pub fn new(state: &MarketState) -> Self {
        Self {
            admin: state.admin,
            base_mint: state.base_mint,
            quote_mint: state.quote_mint,
            bump: [state.bump],
        }
    }

    pub fn as_seeds(&self) -> [pinocchio::cpi::Seed<'_>; 5] {
        [
            MARKET_SEED.into(),
            (&self.admin).into(),
            (&self.base_mint).into(),
            (&self.quote_mint).into(),
            (&self.bump).into(),
        ]
    }
}

/// The vault that holds `mint`, if `mint` is one of the market's mints.
pub fn vault_for(state: &MarketState, mint: &[u8; 32]) -> Option<[u8; 32]> {
    if *mint == state.base_mint {
        Some(state.base_vault)
    } else if *mint == state.quote_mint {
        Some(state.quote_vault)
    } else {
        None
    }
}
