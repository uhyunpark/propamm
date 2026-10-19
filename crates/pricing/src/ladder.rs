//! Price ladder: "ticks with fixed liquidity".
//!
//! Per side there are `num_levels` levels of `level_size_base_atoms` each. Prices come
//! from the mid price on the fly:
//!
//! - ask level i (trader buys base): `ceil(mid * (BPS + half_spread + i * step) / BPS)`
//! - bid level i (trader sells base): `floor(mid * (BPS - half_spread - i * step) / BPS)`
//!
//! `consumed` is tracked per side and reset by the first price update in a new slot. There is no healing
//! across sides. Max tradable per side per window is `num_levels * level_size`.
//! Every rounding favors the maker.

use crate::error::PricingError;
use crate::math::{BPS_DENOM, PRICE_SCALE, mul_div_ceil, mul_div_floor};

/// Maximum number of levels per side (bounds the on-chain compute of the walk).
pub const MAX_LEVELS: u8 = 32;

/// Trade direction from the trader's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Trader buys base and pays quote (walks the asks).
    BuyBase,
    /// Trader sells base and receives quote (walks the bids).
    SellBase,
}

/// Ladder parameters. Prices are scaled by `PRICE_SCALE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ladder {
    /// Mid price, quote atoms per base atom times `PRICE_SCALE`.
    pub mid_price: u64,
    /// Half spread in basis points.
    pub half_spread_bps: u16,
    /// Extra spread per level in basis points.
    pub level_step_bps: u16,
    /// Base atoms per level.
    pub level_size_base_atoms: u64,
    /// Levels per side, `1..=MAX_LEVELS`.
    pub num_levels: u8,
}

/// Result of a quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    /// Output amount in atoms (base for `BuyBase`, quote for `SellBase`).
    pub amount_out: u64,
    /// Consumed base atoms on this side after the fill.
    pub consumed_after: u64,
}

/// Whether a price posted at `last_update_slot` is still fresh at `current_slot`.
pub fn is_fresh(current_slot: u64, last_update_slot: u64, max_age_slots: u64) -> bool {
    current_slot.saturating_sub(last_update_slot) <= max_age_slots
}

fn floor_u64(a: impl Into<u128>, b: impl Into<u128>, d: u128) -> Result<u64, PricingError> {
    let value = mul_div_floor(a.into(), b.into(), d).ok_or(PricingError::Overflow)?;
    u64::try_from(value).map_err(|_| PricingError::Overflow)
}

fn ceil_u64(a: impl Into<u128>, b: impl Into<u128>, d: u128) -> Result<u64, PricingError> {
    let value = mul_div_ceil(a.into(), b.into(), d).ok_or(PricingError::Overflow)?;
    u64::try_from(value).map_err(|_| PricingError::Overflow)
}

fn add(a: u64, b: u64) -> Result<u64, PricingError> {
    a.checked_add(b).ok_or(PricingError::Overflow)
}

fn sub(a: u64, b: u64) -> Result<u64, PricingError> {
    a.checked_sub(b).ok_or(PricingError::Overflow)
}

impl Ladder {
    /// Total spread of `level` in bps: `half_spread + level * step`.
    fn offset_bps(&self, level: u8) -> Result<u64, PricingError> {
        if level >= self.num_levels {
            return Err(PricingError::InvalidParams);
        }
        u64::from(level)
            .checked_mul(u64::from(self.level_step_bps))
            .and_then(|v| v.checked_add(u64::from(self.half_spread_bps)))
            .ok_or(PricingError::Overflow)
    }

    /// Checks everything except the mid price: sizes, level count and spread range.
    ///
    /// Use this when the mid is not known yet (it is `0` until the first price update).
    /// [`Ladder::validate`] adds the checks that need a real mid.
    pub fn validate_shape(&self) -> Result<(), PricingError> {
        if self.level_size_base_atoms == 0 || self.num_levels == 0 || self.num_levels > MAX_LEVELS {
            return Err(PricingError::InvalidParams);
        }
        let last = self.num_levels.saturating_sub(1);
        if u128::from(self.offset_bps(last)?) >= BPS_DENOM {
            return Err(PricingError::InvalidParams);
        }
        // The per-side depth must fit in u64, or the consumed counters could not reach it.
        if self.depth_base_atoms().is_err() {
            return Err(PricingError::InvalidParams);
        }
        Ok(())
    }

    /// Checks the ladder parameters.
    pub fn validate(&self) -> Result<(), PricingError> {
        if self.mid_price == 0 {
            return Err(PricingError::InvalidParams);
        }
        self.validate_shape()?;
        let last = self.num_levels.saturating_sub(1);
        if self.bid_price(last)? == 0 {
            return Err(PricingError::InvalidParams);
        }
        // Every level price must fit in u64 so the walk can price any level it reaches.
        if self.ask_price(last).is_err() {
            return Err(PricingError::InvalidParams);
        }
        Ok(())
    }

    /// Ask price of `level` (rounded up). Errors if `level >= num_levels`.
    pub fn ask_price(&self, level: u8) -> Result<u64, PricingError> {
        let factor = BPS_DENOM
            .checked_add(u128::from(self.offset_bps(level)?))
            .ok_or(PricingError::Overflow)?;
        ceil_u64(self.mid_price, factor, BPS_DENOM)
    }

    /// Bid price of `level` (rounded down). Errors if `level >= num_levels`.
    pub fn bid_price(&self, level: u8) -> Result<u64, PricingError> {
        let factor = BPS_DENOM
            .checked_sub(u128::from(self.offset_bps(level)?))
            .ok_or(PricingError::InvalidParams)?;
        floor_u64(self.mid_price, factor, BPS_DENOM)
    }

    /// Max base atoms tradable per side per window: `num_levels * level_size`.
    pub fn depth_base_atoms(&self) -> Result<u64, PricingError> {
        u64::from(self.num_levels)
            .checked_mul(self.level_size_base_atoms)
            .ok_or(PricingError::Overflow)
    }

    /// Exact-in quote on `side`, given the base atoms already consumed on that side.
    ///
    /// `amount_in` is quote atoms for `BuyBase` and base atoms for `SellBase`.
    pub fn quote_exact_in(
        &self,
        side: Side,
        consumed_base_atoms: u64,
        amount_in: u64,
    ) -> Result<Fill, PricingError> {
        self.validate()?;
        if amount_in == 0 {
            return Err(PricingError::ZeroInput);
        }
        let fill = match side {
            Side::BuyBase => self.walk_buy(consumed_base_atoms, amount_in)?,
            Side::SellBase => self.walk_sell(consumed_base_atoms, amount_in)?,
        };
        if fill.amount_out == 0 {
            return Err(PricingError::ZeroOutput);
        }
        Ok(fill)
    }

    /// Splits `consumed` into the current level and the offset inside it.
    /// Errors with `InsufficientDepth` when `consumed` is past the last level.
    fn position(&self, consumed: u64) -> Result<(u8, u64), PricingError> {
        let size = self.level_size_base_atoms;
        let level = consumed
            .checked_div(size)
            .ok_or(PricingError::InvalidParams)?;
        let offset = consumed
            .checked_rem(size)
            .ok_or(PricingError::InvalidParams)?;
        let level = u8::try_from(level)
            .ok()
            .filter(|level| *level < self.num_levels)
            .ok_or(PricingError::InsufficientDepth)?;
        Ok((level, offset))
    }

    fn walk_buy(&self, mut consumed: u64, amount_in: u64) -> Result<Fill, PricingError> {
        let mut remaining = amount_in;
        let mut out = 0_u64;
        while remaining > 0 {
            let (level, offset) = self.position(consumed)?;
            let price = self.ask_price(level)?;
            let avail = sub(self.level_size_base_atoms, offset)?;
            // Compared in u128: the cost to clear a huge level can exceed u64 even though a small buy is fine.
            let cost_full = mul_div_ceil(avail.into(), price.into(), PRICE_SCALE)
                .ok_or(PricingError::Overflow)?;
            let base = if u128::from(remaining) >= cost_full {
                // cost_full <= remaining, so it fits in u64.
                let charged = u64::try_from(cost_full).map_err(|_| PricingError::Overflow)?;
                remaining = sub(remaining, charged)?;
                avail
            } else {
                let base = floor_u64(remaining, PRICE_SCALE, u128::from(price))?;
                remaining = 0;
                base
            };
            out = add(out, base)?;
            consumed = add(consumed, base)?;
        }
        Ok(Fill {
            amount_out: out,
            consumed_after: consumed,
        })
    }

    fn walk_sell(&self, mut consumed: u64, amount_in: u64) -> Result<Fill, PricingError> {
        let mut remaining = amount_in;
        let mut out = 0_u64;
        while remaining > 0 {
            let (level, offset) = self.position(consumed)?;
            let price = self.bid_price(level)?;
            let avail = sub(self.level_size_base_atoms, offset)?;
            let take = remaining.min(avail);
            out = add(out, floor_u64(take, price, PRICE_SCALE)?)?;
            remaining = sub(remaining, take)?;
            consumed = add(consumed, take)?;
        }
        Ok(Fill {
            amount_out: out,
            consumed_after: consumed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SOL/USDC at $200, 1 SOL per level, 4 levels.
    const SOL: Ladder = Ladder {
        mid_price: 200_000_000_000,
        half_spread_bps: 2,
        level_step_bps: 1,
        level_size_base_atoms: 1_000_000_000,
        num_levels: 4,
    };

    #[test]
    fn known_prices() {
        // mid / 1e4 = 20_000_000 per bps factor unit.
        assert_eq!(SOL.ask_price(0), Ok(200_040_000_000)); // 10002
        assert_eq!(SOL.ask_price(1), Ok(200_060_000_000)); // 10003
        assert_eq!(SOL.ask_price(2), Ok(200_080_000_000)); // 10004
        assert_eq!(SOL.bid_price(0), Ok(199_960_000_000)); // 9998
        assert_eq!(SOL.bid_price(1), Ok(199_940_000_000)); // 9997
        assert_eq!(SOL.bid_price(2), Ok(199_920_000_000)); // 9996
        assert_eq!(SOL.ask_price(4), Err(PricingError::InvalidParams));
        assert_eq!(SOL.depth_base_atoms(), Ok(4_000_000_000));
    }

    #[test]
    fn ask_rounds_up_bid_rounds_down() {
        let l = Ladder {
            mid_price: 10_001,
            half_spread_bps: 1,
            level_step_bps: 0,
            level_size_base_atoms: 1,
            num_levels: 1,
        };
        // 10_001 * 10_001 / 10_000 = 10_002.0001 -> ceil 10_003.
        assert_eq!(l.ask_price(0), Ok(10_003));
        // 10_001 * 9_999 / 10_000 = 9_999.9999 -> floor 9_999.
        assert_eq!(l.bid_price(0), Ok(9_999));
    }

    #[test]
    fn buy_succeeds_when_level_cost_exceeds_u64() {
        // Clearing level 0 costs ~5.7e19 quote atoms (> u64::MAX), but a small buy only needs a partial fill.
        let l = Ladder {
            mid_price: 100_000_000_000_000,
            half_spread_bps: 2,
            level_step_bps: 1,
            level_size_base_atoms: u64::MAX / 32,
            num_levels: 32,
        };
        assert!(l.validate().is_ok());
        let price = l.ask_price(0).unwrap();
        assert!(ceil_u64(l.level_size_base_atoms, price, PRICE_SCALE).is_err());
        let fill = l.quote_exact_in(Side::BuyBase, 0, 1_000_000).unwrap();
        // floor(1e6 * 1e12 / 100_020_000_000_000) = 9_998.
        assert_eq!(fill.amount_out, 9_998);
        assert_eq!(fill.consumed_after, 9_998);
        // Same from the deepest level.
        let deep = l.level_size_base_atoms * 31;
        let fill = l.quote_exact_in(Side::BuyBase, deep, 1_000_000).unwrap();
        assert!(fill.amount_out > 0);
        assert_eq!(fill.consumed_after, deep + fill.amount_out);
    }

    #[test]
    fn buy_crosses_level_boundary() {
        // Level 0: 1e9 base costs ceil(1e9 * 200_040_000_000 / 1e12) = 200_040_000.
        // Left: 300_040_000 - 200_040_000 = 100_000_000 quote at ask1 = 200_060_000_000.
        // Base = floor(1e8 * 1e12 / 200_060_000_000) = 499_850_044.
        let fill = SOL.quote_exact_in(Side::BuyBase, 0, 300_040_000).unwrap();
        assert_eq!(fill.amount_out, 1_499_850_044);
        assert_eq!(fill.consumed_after, 1_499_850_044);
    }

    #[test]
    fn buy_starting_mid_level() {
        // consumed 500_000_000: avail 5e8 at ask0, cost ceil(5e8 * 200_040_000_000 / 1e12) = 100_020_000.
        let fill = SOL
            .quote_exact_in(Side::BuyBase, 500_000_000, 100_020_000)
            .unwrap();
        assert_eq!(fill.amount_out, 500_000_000);
        assert_eq!(fill.consumed_after, 1_000_000_000);
    }

    #[test]
    fn sell_crosses_level_boundary() {
        // Level 0: 1e9 * 199_960_000_000 / 1e12 = 199_960_000.
        // Level 1: 5e8 * 199_940_000_000 / 1e12 = 99_970_000.
        let fill = SOL
            .quote_exact_in(Side::SellBase, 0, 1_500_000_000)
            .unwrap();
        assert_eq!(fill.amount_out, 299_930_000);
        assert_eq!(fill.consumed_after, 1_500_000_000);
    }

    #[test]
    fn depth_exhausted() {
        // Exact depth (4e9 base) is fillable on the sell side; one more atom is not.
        assert!(SOL.quote_exact_in(Side::SellBase, 0, 4_000_000_000).is_ok());
        assert_eq!(
            SOL.quote_exact_in(Side::SellBase, 0, 4_000_000_001),
            Err(PricingError::InsufficientDepth)
        );
        assert_eq!(
            SOL.quote_exact_in(Side::BuyBase, 0, u64::MAX),
            Err(PricingError::InsufficientDepth)
        );
    }

    #[test]
    fn consumed_at_depth() {
        assert_eq!(
            SOL.quote_exact_in(Side::BuyBase, 4_000_000_000, 1_000),
            Err(PricingError::InsufficientDepth)
        );
        assert_eq!(
            SOL.quote_exact_in(Side::SellBase, 4_000_000_000, 1),
            Err(PricingError::InsufficientDepth)
        );
    }

    #[test]
    fn zero_input() {
        assert_eq!(
            SOL.quote_exact_in(Side::BuyBase, 0, 0),
            Err(PricingError::ZeroInput)
        );
        assert_eq!(
            SOL.quote_exact_in(Side::SellBase, 0, 0),
            Err(PricingError::ZeroInput)
        );
    }

    #[test]
    fn tiny_buy_is_zero_output() {
        // 2 quote atoms per base atom: 1 quote atom buys floor(1e12 / 2.0004e12) = 0 base.
        let l = Ladder {
            mid_price: 2_000_000_000_000,
            ..SOL
        };
        assert_eq!(
            l.quote_exact_in(Side::BuyBase, 0, 1),
            Err(PricingError::ZeroOutput)
        );
    }

    #[test]
    fn tiny_sell_is_zero_output() {
        // 1 base atom * 0.19996 quote atoms floors to 0.
        assert_eq!(
            SOL.quote_exact_in(Side::SellBase, 0, 1),
            Err(PricingError::ZeroOutput)
        );
    }

    #[test]
    fn invalid_params_rejected() {
        let bad = [
            Ladder {
                mid_price: 0,
                ..SOL
            },
            Ladder {
                level_size_base_atoms: 0,
                ..SOL
            },
            Ladder {
                num_levels: 0,
                ..SOL
            },
            Ladder {
                num_levels: MAX_LEVELS + 1,
                ..SOL
            },
            // half_spread alone reaches 100%.
            Ladder {
                half_spread_bps: 10_000,
                ..SOL
            },
            // 9_990 + 3 * 4 = 10_002 >= 10_000.
            Ladder {
                half_spread_bps: 9_990,
                level_step_bps: 4,
                ..SOL
            },
            // Deepest bid price rounds to zero.
            Ladder {
                mid_price: 1,
                half_spread_bps: 1,
                level_step_bps: 0,
                ..SOL
            },
        ];
        for l in bad {
            assert_eq!(l.validate(), Err(PricingError::InvalidParams), "{l:?}");
            assert_eq!(
                l.quote_exact_in(Side::BuyBase, 0, 1),
                Err(PricingError::InvalidParams)
            );
        }
        assert_eq!(SOL.validate(), Ok(()));
    }

    #[test]
    fn validate_requires_deepest_ask_to_fit_u64() {
        // ceil(u64::MAX * 10_002 / 10_000) > u64::MAX.
        let near_max = Ladder {
            mid_price: u64::MAX,
            ..SOL
        };
        assert_eq!(near_max.ask_price(0), Err(PricingError::Overflow));
        assert_eq!(near_max.validate(), Err(PricingError::InvalidParams));
        // Level 0 fits but the deepest level does not.
        let mid = u64::MAX / 10_010 * 10_000;
        let deep = Ladder {
            mid_price: mid,
            half_spread_bps: 1,
            level_step_bps: 5,
            ..SOL
        };
        assert!(deep.ask_price(0).is_ok());
        assert!(deep.ask_price(3).is_err());
        assert_eq!(deep.validate(), Err(PricingError::InvalidParams));
        // Largest mid whose deepest ask (+16 bps) still fits is accepted.
        let fits = Ladder {
            mid_price: mid / 2,
            ..deep
        };
        assert_eq!(fits.validate(), Ok(()));
    }

    #[test]
    fn validate_shape_ignores_mid() {
        let no_mid = Ladder {
            mid_price: 0,
            ..SOL
        };
        assert_eq!(no_mid.validate_shape(), Ok(()));
        assert_eq!(no_mid.validate(), Err(PricingError::InvalidParams));
        // Deepest bid rounding to zero needs a real mid, so the shape check accepts it.
        let tiny_mid = Ladder {
            mid_price: 1,
            half_spread_bps: 1,
            level_step_bps: 0,
            ..SOL
        };
        assert_eq!(tiny_mid.validate_shape(), Ok(()));
        assert_eq!(tiny_mid.validate(), Err(PricingError::InvalidParams));
    }

    #[test]
    fn validate_shape_rejects_bad_shape() {
        let bad = [
            Ladder {
                level_size_base_atoms: 0,
                ..SOL
            },
            Ladder {
                num_levels: 0,
                ..SOL
            },
            Ladder {
                num_levels: MAX_LEVELS + 1,
                ..SOL
            },
            Ladder {
                half_spread_bps: 10_000,
                ..SOL
            },
            // Depth = 32 * (u64::MAX / 16) overflows u64.
            Ladder {
                level_size_base_atoms: u64::MAX / 16,
                num_levels: 32,
                ..SOL
            },
            Ladder {
                half_spread_bps: 9_990,
                level_step_bps: 4,
                ..SOL
            },
        ];
        for l in bad {
            assert_eq!(
                l.validate_shape(),
                Err(PricingError::InvalidParams),
                "{l:?}"
            );
        }
        // Boundaries: 9_999 total offset is the largest accepted, 32 levels the most.
        let edge = Ladder {
            half_spread_bps: 9_996,
            level_step_bps: 1,
            num_levels: 4,
            ..SOL
        };
        assert_eq!(edge.validate_shape(), Ok(()));
        let max = Ladder {
            num_levels: MAX_LEVELS,
            ..SOL
        };
        assert_eq!(max.validate_shape(), Ok(()));
        // Largest level size whose 32-level depth still fits u64.
        let deep = Ladder {
            level_size_base_atoms: u64::MAX / 32,
            num_levels: MAX_LEVELS,
            ..SOL
        };
        assert_eq!(deep.validate_shape(), Ok(()));
        let over = Ladder {
            level_size_base_atoms: u64::MAX / 32 + 1,
            ..deep
        };
        assert_eq!(over.validate_shape(), Err(PricingError::InvalidParams));
    }

    #[test]
    fn freshness_edges() {
        assert!(is_fresh(100, 100, 0));
        assert!(is_fresh(110, 100, 10));
        assert!(!is_fresh(111, 100, 10));
        // Clock behind the update slot saturates to age 0.
        assert!(is_fresh(5, 100, 0));
    }

    #[test]
    fn extremes_do_not_panic() {
        // BTC-like: 1_000 quote atoms per base atom.
        let btc = Ladder {
            mid_price: 1_000_000_000_000_000,
            half_spread_bps: 5,
            level_step_bps: 2,
            level_size_base_atoms: u64::MAX / 64,
            num_levels: 32,
        };
        for amount in [1, 1_000, u64::MAX / 2, u64::MAX] {
            for side in [Side::BuyBase, Side::SellBase] {
                for consumed in [0, 1, u64::MAX / 2, u64::MAX] {
                    let r = btc.quote_exact_in(side, consumed, amount);
                    assert!(matches!(
                        r,
                        Ok(_)
                            | Err(PricingError::Overflow
                                | PricingError::InsufficientDepth
                                | PricingError::ZeroOutput)
                    ));
                }
            }
        }
        let huge = Ladder {
            mid_price: u64::MAX,
            level_size_base_atoms: u64::MAX,
            num_levels: 32,
            ..btc
        };
        assert!(huge.depth_base_atoms().is_err());
        let _ = huge.quote_exact_in(Side::BuyBase, 0, u64::MAX);
        let _ = huge.quote_exact_in(Side::SellBase, 0, u64::MAX);
    }
}
