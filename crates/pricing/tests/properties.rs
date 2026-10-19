//! Property tests for the ladder quote.

use propamm_pricing::{Ladder, PRICE_SCALE, Side};
use proptest::prelude::*;

fn side() -> impl Strategy<Value = Side> {
    prop_oneof![Just(Side::BuyBase), Just(Side::SellBase)]
}

fn ladder() -> impl Strategy<Value = Ladder> {
    (
        1_000_000_000_u64..=1_000_000_000_000_000,
        0_u16..=100,
        0_u16..=20,
        1_u64..=1_000_000_000_000,
        1_u8..=32,
    )
        .prop_map(
            |(mid_price, half_spread_bps, level_step_bps, size, num_levels)| Ladder {
                mid_price,
                half_spread_bps,
                level_step_bps,
                level_size_base_atoms: size,
                num_levels,
            },
        )
}

/// `ppm` parts per million of the ladder depth, in base atoms.
fn depth_ppm(l: &Ladder, ppm: u64) -> u128 {
    u128::from(l.num_levels) * u128::from(l.level_size_base_atoms) * u128::from(ppm) / 1_000_000
}

/// Input amount: quote atoms for buys (up to ~depth worth), base atoms for sells.
fn amount_in(l: &Ladder, side: Side, frac_ppm: u64) -> u64 {
    let base = depth_ppm(l, frac_ppm);
    let amount = match side {
        Side::SellBase => base,
        Side::BuyBase => base * u128::from(l.mid_price) * 11 / 10 / PRICE_SCALE,
    };
    u64::try_from(amount).unwrap_or(u64::MAX).max(1)
}

proptest! {
    #[test]
    fn splitting_never_helps(
        l in ladder(), s in side(), frac in 1_u64..=1_200_000, n in 2_u64..=16, start_ppm in 0_u64..=500_000,
    ) {
        let start = u64::try_from(depth_ppm(&l, start_ppm)).unwrap();
        let total = amount_in(&l, s, frac);
        let Ok(single) = l.quote_exact_in(s, start, total) else { return Ok(()); };
        let part = total / n;
        let mut consumed = start;
        let mut sum = 0_u128;
        for i in 0..n {
            let amt = if i + 1 == n { total - part * (n - 1) } else { part };
            let Ok(f) = l.quote_exact_in(s, consumed, amt) else { return Ok(()); };
            consumed = f.consumed_after;
            sum += u128::from(f.amount_out);
        }
        prop_assert!(sum <= u128::from(single.amount_out), "split {sum} > single {}", single.amount_out);
    }

    #[test]
    fn monotonic_in_input(
        l in ladder(), s in side(), f1 in 1_u64..=1_200_000, f2 in 1_u64..=1_200_000,
    ) {
        let (a, b) = (amount_in(&l, s, f1.min(f2)), amount_in(&l, s, f1.max(f2)));
        if let (Ok(x), Ok(y)) = (l.quote_exact_in(s, 0, a), l.quote_exact_in(s, 0, b)) {
            prop_assert!(x.amount_out <= y.amount_out);
        }
    }

    #[test]
    fn monotonic_in_consumed(
        l in ladder(), s in side(), frac in 1_u64..=300_000, p1 in 0_u64..=1_000_000, p2 in 0_u64..=1_000_000,
    ) {
        let at = |p: u64| u64::try_from(depth_ppm(&l, p)).unwrap();
        let (c1, c2) = (at(p1.min(p2)), at(p1.max(p2)));
        let amt = amount_in(&l, s, frac);
        if let (Ok(x), Ok(y)) = (l.quote_exact_in(s, c1, amt), l.quote_exact_in(s, c2, amt)) {
            // Per-level rounding differs by at most 1 output atom per level boundary
            // crossed. For buys the rounding is in quote atoms, worth PRICE_SCALE / ask
            // base atoms each.
            let per_level = match s {
                Side::SellBase => 1,
                Side::BuyBase => PRICE_SCALE / u128::from(l.ask_price(0).unwrap()) + 1,
            };
            let slack = per_level * (u128::from(l.num_levels) + 1);
            prop_assert!(u128::from(x.amount_out) + slack >= u128::from(y.amount_out));
        }
    }

    #[test]
    fn maker_favor_bound(l in ladder(), s in side(), frac in 1_u64..=1_200_000) {
        let amt = amount_in(&l, s, frac);
        if let Ok(f) = l.quote_exact_in(s, 0, amt) {
            let out = u128::from(f.amount_out);
            let amt = u128::from(amt);
            match s {
                Side::BuyBase => {
                    prop_assert!(amt * PRICE_SCALE >= out * u128::from(l.ask_price(0).unwrap()));
                }
                Side::SellBase => {
                    prop_assert!(out * PRICE_SCALE <= amt * u128::from(l.bid_price(0).unwrap()));
                }
            }
        }
    }

    #[test]
    fn no_panics(
        mid in any::<u64>(), hs in any::<u16>(), step in any::<u16>(), size in any::<u64>(),
        levels in any::<u8>(), s in side(), consumed in any::<u64>(), amt in any::<u64>(),
    ) {
        let l = Ladder {
            mid_price: mid,
            half_spread_bps: hs,
            level_step_bps: step,
            level_size_base_atoms: size,
            num_levels: levels,
        };
        let _ = l.quote_exact_in(s, consumed, amt);
        let _ = l.depth_base_atoms();
        let _ = l.ask_price(levels);
        let _ = l.bid_price(levels);
    }
}
