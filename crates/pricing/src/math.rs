//! Fixed-point helpers. Every division in the crate goes through this module.

/// Price scale: prices are quote atoms per 1 base atom, multiplied by this.
pub const PRICE_SCALE: u128 = 1_000_000_000_000;

/// Basis points per 1.0.
pub const BPS_DENOM: u128 = 10_000;

/// Computes `floor(a * b / d)`. Returns `None` if `d == 0` or `a * b` overflows `u128`.
pub fn mul_div_floor(a: u128, b: u128, d: u128) -> Option<u128> {
    a.checked_mul(b)?.checked_div(d)
}

/// Computes `ceil(a * b / d)`. Returns `None` if `d == 0` or the result overflows `u128`.
pub fn mul_div_ceil(a: u128, b: u128, d: u128) -> Option<u128> {
    let product = a.checked_mul(b)?;
    let quotient = product.checked_div(d)?;
    if product.checked_rem(d)? == 0 {
        Some(quotient)
    } else {
        quotient.checked_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_and_ceil() {
        assert_eq!(mul_div_floor(10, 10, 3), Some(33));
        assert_eq!(mul_div_ceil(10, 10, 3), Some(34));
        assert_eq!(mul_div_floor(10, 9, 3), Some(30));
        assert_eq!(mul_div_ceil(10, 9, 3), Some(30));
        assert_eq!(mul_div_floor(0, 5, 3), Some(0));
        assert_eq!(mul_div_ceil(0, 5, 3), Some(0));
    }

    #[test]
    fn zero_divisor_is_none() {
        assert_eq!(mul_div_floor(1, 1, 0), None);
        assert_eq!(mul_div_ceil(1, 1, 0), None);
    }

    #[test]
    fn overflow_is_none() {
        assert_eq!(mul_div_floor(u128::MAX, 2, 1), None);
        assert_eq!(mul_div_ceil(u128::MAX, 2, 1), None);
        assert_eq!(mul_div_ceil(u128::MAX, 1, 2), Some(u128::MAX / 2 + 1));
    }
}
