//! Move limit: how far a new mid may stray from the mid at the start of the slot window.

use crate::error::PricingError;
use crate::math::{BPS_DENOM, mul_div_floor};

/// Inclusive band `(low, high)` of mids allowed around `window_start_mid`.
///
/// The half width is `floor(window_start_mid * max_move_bps / BPS_DENOM)`, so the band is
/// never wider than the exact bps range. `high` saturates at `u64::MAX`.
pub fn move_band(window_start_mid: u64, max_move_bps: u16) -> Result<(u64, u64), PricingError> {
    let delta = mul_div_floor(
        u128::from(window_start_mid),
        u128::from(max_move_bps),
        BPS_DENOM,
    )
    .and_then(|d| u64::try_from(d).ok())
    .ok_or(PricingError::Overflow)?;
    let low = window_start_mid
        .checked_sub(delta)
        .ok_or(PricingError::Overflow)?;
    Ok((low, window_start_mid.saturating_add(delta)))
}

/// Whether `new_mid` lies inside the inclusive [`move_band`] around `window_start_mid`.
pub fn within_move_limit(
    window_start_mid: u64,
    new_mid: u64,
    max_move_bps: u16,
) -> Result<bool, PricingError> {
    let (low, high) = move_band(window_start_mid, max_move_bps)?;
    Ok((low..=high).contains(&new_mid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_is_inclusive_and_symmetric() {
        // 1% of 200e9 = 2e9.
        assert_eq!(
            move_band(200_000_000_000, 100),
            Ok((198_000_000_000, 202_000_000_000))
        );
        for (mid, ok) in [
            (198_000_000_000, true),
            (197_999_999_999, false),
            (202_000_000_000, true),
            (202_000_000_001, false),
            (200_000_000_000, true),
        ] {
            assert_eq!(
                within_move_limit(200_000_000_000, mid, 100),
                Ok(ok),
                "{mid}"
            );
        }
    }

    #[test]
    fn band_rounds_narrower() {
        // 1% of 150 = 1.5 -> floor 1.
        assert_eq!(move_band(150, 100), Ok((149, 151)));
        assert_eq!(within_move_limit(150, 152, 100), Ok(false));
        assert_eq!(within_move_limit(150, 148, 100), Ok(false));
        // Too small for any move.
        assert_eq!(move_band(99, 100), Ok((99, 99)));
        assert_eq!(within_move_limit(99, 100, 100), Ok(false));
    }

    #[test]
    fn zero_bps_pins_the_mid() {
        assert_eq!(within_move_limit(1_000, 1_000, 0), Ok(true));
        assert_eq!(within_move_limit(1_000, 1_001, 0), Ok(false));
    }

    #[test]
    fn high_saturates_at_u64_max() {
        let (low, high) = move_band(u64::MAX, 100).unwrap();
        assert_eq!(high, u64::MAX);
        assert_eq!(low, u64::MAX - u64::MAX / 100);
        assert_eq!(within_move_limit(u64::MAX, u64::MAX, 100), Ok(true));
    }

    #[test]
    fn band_wider_than_mid_errors() {
        // max_move_bps is capped by the program at 100, but the helper stays total.
        assert_eq!(move_band(10, u16::MAX), Err(PricingError::Overflow));
    }
}
