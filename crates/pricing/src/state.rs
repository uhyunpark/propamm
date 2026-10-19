//! Shared on-chain account layout.
//!
//! The on-chain account is an 8-byte discriminator header (added by Anchor or Pinocchio,
//! NOT part of this struct) followed by [`MarketState`]. Offsets are pinned by
//! `tests/layout.rs`.

use crate::error::PricingError;
use crate::ladder::{Ladder, Side};

/// Size in bytes of [`MarketState`] (excluding the 8-byte discriminator).
pub const MARKET_STATE_SIZE: usize = 264;

/// Market account body. Pubkeys are raw `[u8; 32]` to keep this crate dependency-free.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct MarketState {
    /// Cold key allowed to move funds and change parameters.
    pub admin: [u8; 32],
    /// Hot key allowed to call `update_price` only.
    pub updater: [u8; 32],
    /// Base token mint.
    pub base_mint: [u8; 32],
    /// Quote token mint.
    pub quote_mint: [u8; 32],
    /// Base token vault.
    pub base_vault: [u8; 32],
    /// Quote token vault.
    pub quote_vault: [u8; 32],
    /// Mid price, scaled by `PRICE_SCALE`. Written by `update_price` and `force_price`.
    pub mid_price: u64,
    /// Slot of the last price update. Written by `update_price` and `force_price`.
    pub last_update_slot: u64,
    /// Base atoms consumed on the ask side (traders buying) in the current slot window.
    pub ask_consumed_base_atoms: u64,
    /// Base atoms consumed on the bid side (traders selling) in the current slot window.
    pub bid_consumed_base_atoms: u64,
    /// Base atoms per ladder level.
    pub level_size_base_atoms: u64,
    /// Maximum price age in slots before swaps are rejected.
    pub max_age_slots: u64,
    /// Half spread in basis points.
    pub half_spread_bps: u16,
    /// Extra spread per ladder level in basis points.
    pub level_step_bps: u16,
    /// Maximum mid move per slot window in basis points (of `window_start_mid`).
    pub max_move_bps: u16,
    /// Number of ladder levels per side.
    pub num_levels: u8,
    /// Non-zero when the market is paused.
    pub paused: u8,
    /// PDA bump.
    pub bump: u8,
    /// Explicit padding to a multiple of 8 bytes.
    pub padding: [u8; 7],
    /// Mid price at the start of the current slot window: the reference for the move limit.
    /// Appended after the padding so every earlier offset stays pinned.
    pub window_start_mid: u64,
}

const _: () = assert!(core::mem::size_of::<MarketState>() == MARKET_STATE_SIZE);

impl MarketState {
    /// The pricing ladder described by this state.
    pub fn ladder(&self) -> Ladder {
        Ladder {
            mid_price: self.mid_price,
            half_spread_bps: self.half_spread_bps,
            level_step_bps: self.level_step_bps,
            level_size_base_atoms: self.level_size_base_atoms,
            num_levels: self.num_levels,
        }
    }

    /// Base atoms consumed on `side` in the current slot window.
    pub fn consumed(&self, side: Side) -> u64 {
        match side {
            Side::BuyBase => self.ask_consumed_base_atoms,
            Side::SellBase => self.bid_consumed_base_atoms,
        }
    }

    /// Sets the consumed base atoms for `side`.
    pub fn set_consumed(&mut self, side: Side, value: u64) {
        match side {
            Side::BuyBase => self.ask_consumed_base_atoms = value,
            Side::SellBase => self.bid_consumed_base_atoms = value,
        }
    }

    /// Checks that the ladder is valid with `mid` as its mid price.
    pub fn validate_mid(&self, mid: u64) -> Result<(), PricingError> {
        let mut ladder = self.ladder();
        ladder.mid_price = mid;
        ladder.validate()
    }

    /// Opens a new window for a price update at `slot`, before the new mid is written.
    ///
    /// One update per slot: every successful update starts a window. Both consumed counters
    /// reset and `window_start_mid` becomes the current `mid_price`. Errors with
    /// `SlotRegression` if `slot < last_update_slot` and `AlreadyUpdatedThisSlot` if
    /// `slot == last_update_slot` (this includes the slot of a `force_price`).
    pub fn roll_window(&mut self, slot: u64) -> Result<(), PricingError> {
        if slot < self.last_update_slot {
            return Err(PricingError::SlotRegression);
        }
        if slot == self.last_update_slot {
            return Err(PricingError::AlreadyUpdatedThisSlot);
        }
        self.ask_consumed_base_atoms = 0;
        self.bid_consumed_base_atoms = 0;
        self.window_start_mid = self.mid_price;
        Ok(())
    }

    /// Sets `mid` as a fresh window at `slot`: window start, consumed counters and slot.
    pub fn reset_window(&mut self, mid: u64, slot: u64) {
        self.mid_price = mid;
        self.window_start_mid = mid;
        self.last_update_slot = slot;
        self.ask_consumed_base_atoms = 0;
        self.bid_consumed_base_atoms = 0;
    }

    /// Whether the market is paused.
    pub fn is_paused(&self) -> bool {
        self.paused != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> MarketState {
        let mut s: MarketState = bytemuck::Zeroable::zeroed();
        s.mid_price = 500;
        s.window_start_mid = 400;
        s.last_update_slot = 10;
        s.ask_consumed_base_atoms = 7;
        s.bid_consumed_base_atoms = 9;
        s
    }

    #[test]
    fn roll_window_new_slot_resets_and_snapshots_mid() {
        let mut s = state();
        assert_eq!(s.roll_window(11), Ok(()));
        assert_eq!(s.ask_consumed_base_atoms, 0);
        assert_eq!(s.bid_consumed_base_atoms, 0);
        assert_eq!(s.window_start_mid, 500);
        // Only the window fields change; the caller writes mid and slot.
        assert_eq!((s.mid_price, s.last_update_slot), (500, 10));
    }

    #[test]
    fn roll_window_same_slot_errors_and_changes_nothing() {
        let mut s = state();
        let before = s;
        assert_eq!(s.roll_window(10), Err(PricingError::AlreadyUpdatedThisSlot));
        assert_eq!(s, before);
    }

    #[test]
    fn roll_window_earlier_slot_errors_and_changes_nothing() {
        let mut s = state();
        let before = s;
        assert_eq!(s.roll_window(9), Err(PricingError::SlotRegression));
        assert_eq!(s, before);
    }

    #[test]
    fn validate_mid_checks_the_ladder_with_the_new_mid() {
        let mut s = state();
        s.level_size_base_atoms = 1;
        s.num_levels = 1;
        s.half_spread_bps = 1;
        assert_eq!(s.validate_mid(500), Ok(()));
        assert_eq!(s.validate_mid(0), Err(PricingError::InvalidParams));
        assert_eq!(s.validate_mid(1), Err(PricingError::InvalidParams));
        assert_eq!(s.validate_mid(u64::MAX), Err(PricingError::InvalidParams));
    }

    #[test]
    fn reset_window_sets_everything() {
        let mut s = state();
        s.reset_window(600, 12);
        assert_eq!(
            (
                s.mid_price,
                s.window_start_mid,
                s.last_update_slot,
                s.ask_consumed_base_atoms,
                s.bid_consumed_base_atoms
            ),
            (600, 600, 12, 0, 0)
        );
    }
}
