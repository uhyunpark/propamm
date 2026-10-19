//! Publish policy (pure): when to send `update_price` and which mid to send. Design:
//! `docs/design-oracle.md`, "Publishing".
//!
//! Prices on chain are `quote atoms per base atom * PRICE_SCALE` (`u64`). The fair value is
//! USD per 1 whole base token (`f64`). `price_to_usd` and `usd_to_price` convert with the mint
//! decimals.

use propamm_pricing::{PRICE_SCALE, move_band};

use crate::BPS;

/// Token amounts and prices are converted with these decimals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimals {
    pub base: u8,
    pub quote: u8,
}

/// Scale factor `10^(quote - base)` between USD per whole token and quote atoms per base atom.
fn decimals_factor(d: Decimals) -> f64 {
    10_f64.powi(i32::from(d.quote) - i32::from(d.base))
}

/// USD per whole base token for an on-chain mid.
// Precision loss is irrelevant for a display and policy value.
#[allow(clippy::cast_precision_loss)]
pub fn price_to_usd(mid: u64, d: Decimals) -> f64 {
    mid as f64 / PRICE_SCALE as f64 / decimals_factor(d)
}

/// On-chain mid for a USD price. `None` when the price is not positive, not finite or out of range.
// The range check below guards the cast.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn usd_to_price(usd: f64, d: Decimals) -> Option<u64> {
    #[allow(clippy::cast_precision_loss)]
    let scaled = (usd * decimals_factor(d) * PRICE_SCALE as f64).round();
    #[allow(clippy::cast_precision_loss)]
    let ok = scaled.is_finite() && scaled >= 1.0 && scaled < u64::MAX as f64;
    ok.then_some(scaled as u64)
}

/// Whole-token amount to atoms (rounded to the nearest atom). `None` when negative or out of range.
// The range check below guards the cast.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn ui_to_atoms(ui: f64, decimals: u8) -> Option<u64> {
    let atoms = (ui * 10_f64.powi(i32::from(decimals))).round();
    (atoms.is_finite() && atoms >= 0.0 && atoms < u64::MAX as f64).then_some(atoms as u64)
}

/// Atoms to whole tokens.
// Precision loss is irrelevant for a display value.
#[allow(clippy::cast_precision_loss)]
pub fn atoms_to_ui(atoms: u64, decimals: u8) -> f64 {
    atoms as f64 / 10_f64.powi(i32::from(decimals))
}

/// Share of the inventory value held in the base token, valued at `fair` (USD per base).
/// `None` when the vaults are empty or `fair` is not positive.
pub fn base_value_share(base_ui: f64, quote_ui: f64, fair: f64) -> Option<f64> {
    let base_value = base_ui * fair;
    let total = base_value + quote_ui;
    (total > 0.0 && fair > 0.0).then(|| base_value / total)
}

/// Inventory skew in bps: `-skew_max_bps * clamp((share - 0.5) * 2, -1, 1)`.
/// Too much base gives a negative skew: the mid sits below fair, so our bid is lower (we buy
/// less base) and our ask is lower (we sell more). Too little base gives a positive skew.
/// Unknown inventory gives 0.
pub fn skew_bps(base_value_share: Option<f64>, skew_max_bps: f64) -> f64 {
    base_value_share.map_or(0.0, |share| {
        -skew_max_bps * ((share - 0.5) * 2.0).clamp(-1.0, 1.0)
    })
}

/// Fair value shifted by the skew.
pub fn skewed_fair(fair: f64, skew_bps: f64) -> f64 {
    fair * (1.0 + skew_bps / BPS)
}

/// Clamps `target` into the move band around the current on-chain mid. Returns the clamped mid
/// and whether the band cut it. An unrepresentable band leaves the target unchanged.
pub fn clamp_to_band(target: u64, onchain_mid: u64, max_move_bps: u16) -> (u64, bool) {
    match move_band(onchain_mid, max_move_bps) {
        Ok((low, high)) => {
            let clamped = target.clamp(low, high);
            (clamped, clamped != target)
        }
        Err(_) => (target, false),
    }
}

/// Distance between two mids in bps of `reference`.
// Precision loss is irrelevant for a policy comparison.
#[allow(clippy::cast_precision_loss)]
pub fn gap_bps(value: u64, reference: u64) -> f64 {
    if reference == 0 {
        return f64::INFINITY;
    }
    (value as f64 - reference as f64) / reference as f64 * BPS
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PolicyParams {
    /// Send when the (clamped) target differs from the on-chain mid by at least this many bps.
    pub threshold_bps: f64,
    /// Send at least this often (in slots since the last on-chain update). Must be below
    /// `max_age_slots`.
    pub heartbeat_slots: u64,
    /// Warn once the target has been clamped for more than this many slots in a row.
    pub clamp_warn_slots: u64,
    /// Send nothing while the on-chain state was last confirmed longer ago than this.
    pub max_state_age_ms: u64,
}

impl PolicyParams {
    /// The heartbeat must be below the market's `max_age_slots`, or the price goes stale
    /// between heartbeats.
    pub fn validate(&self, max_age_slots: u64) -> Result<(), String> {
        if self.heartbeat_slots >= max_age_slots {
            return Err(format!(
                "heartbeat_slots {} must be below the market's max_age_slots {max_age_slots}",
                self.heartbeat_slots
            ));
        }
        Ok(())
    }
}

/// Everything the policy looks at in one evaluation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Inputs {
    pub slot: u64,
    /// `None` when the fair engine halts or the Pyth breaker is tripped.
    pub target: Option<u64>,
    pub onchain_mid: u64,
    pub last_update_slot: u64,
    /// Time since the on-chain state (mid, band reference) was last confirmed by ws or RPC.
    pub state_age_ms: u64,
    pub max_move_bps: u16,
    pub paused: bool,
    /// An earlier update has been sent and is not yet visible in the market account.
    pub pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// Fair engine or circuit breaker says no.
    Halted,
    /// The market has no price yet (admin `force_price` first) or is paused.
    NotTradable,
    /// The on-chain state is too old to trust as the band reference.
    StaleState,
    /// An update was already sent in this slot.
    SameSlot,
    /// An earlier update is still in flight and was sent in the previous slot.
    InFlight,
    Quiet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Hold(Hold),
    Send { mid: u64, clamped: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Evaluation {
    pub decision: Decision,
    /// The target has been clamped for more than `clamp_warn_slots` slots: recommend `force-price`.
    pub clamp_warning: bool,
}

/// Stateful part of the policy: last send slot and the clamp streak.
#[derive(Debug, Clone)]
pub struct Publisher {
    params: PolicyParams,
    last_sent_slot: Option<u64>,
    clamped_since: Option<u64>,
}

impl Publisher {
    pub fn new(params: PolicyParams) -> Self {
        Self {
            params,
            last_sent_slot: None,
            clamped_since: None,
        }
    }

    /// Records that an update was sent in `slot`.
    pub fn record_sent(&mut self, slot: u64) {
        self.last_sent_slot = Some(slot);
    }

    /// Decides what to do in `inputs.slot`. Does not record a send; call `record_sent`.
    pub fn evaluate(&mut self, i: &Inputs) -> Evaluation {
        let hold = |decision| Evaluation {
            decision: Decision::Hold(decision),
            clamp_warning: false,
        };
        let Some(target) = i.target else {
            self.clamped_since = None;
            return hold(Hold::Halted);
        };
        if i.paused || i.onchain_mid == 0 {
            self.clamped_since = None;
            return hold(Hold::NotTradable);
        }
        if i.state_age_ms > self.params.max_state_age_ms {
            self.clamped_since = None;
            return hold(Hold::StaleState);
        }
        let (mid, clamped) = clamp_to_band(target, i.onchain_mid, i.max_move_bps);
        let warning = self.track_clamp(i.slot, clamped);
        let evaluation = |decision| Evaluation {
            decision,
            clamp_warning: warning,
        };
        if let Some(last) = self.last_sent_slot {
            if i.slot <= last {
                return evaluation(Decision::Hold(Hold::SameSlot));
            }
            // One update per slot, and while the previous one is not visible yet the band
            // reference (the on-chain mid) is stale, so wait one more slot.
            if i.pending && i.slot <= last.saturating_add(1) {
                return evaluation(Decision::Hold(Hold::InFlight));
            }
        }
        let due = gap_bps(mid, i.onchain_mid).abs() >= self.params.threshold_bps
            || i.slot.saturating_sub(i.last_update_slot) >= self.params.heartbeat_slots;
        if due {
            evaluation(Decision::Send { mid, clamped })
        } else {
            evaluation(Decision::Hold(Hold::Quiet))
        }
    }

    /// Tracks the clamp streak; true once it exceeds `clamp_warn_slots` (then re-arms).
    fn track_clamp(&mut self, slot: u64, clamped: bool) -> bool {
        if !clamped {
            self.clamped_since = None;
            return false;
        }
        let since = *self.clamped_since.get_or_insert(slot);
        if slot.saturating_sub(since) > self.params.clamp_warn_slots {
            self.clamped_since = Some(slot);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Decimals = Decimals { base: 9, quote: 6 };
    const MID: u64 = 200_000_000_000;

    fn publisher() -> Publisher {
        Publisher::new(PolicyParams {
            threshold_bps: 1.0,
            heartbeat_slots: 10,
            clamp_warn_slots: 5,
            max_state_age_ms: 6_000,
        })
    }

    fn inputs(slot: u64, target: Option<u64>) -> Inputs {
        Inputs {
            slot,
            target,
            onchain_mid: MID,
            last_update_slot: slot - 1,
            state_age_ms: 0,
            max_move_bps: 100,
            paused: false,
            pending: false,
        }
    }

    /// `bps` basis points above `MID`.
    fn above(bps: u64) -> u64 {
        MID + MID / 10_000 * bps
    }

    #[test]
    fn unit_conversions_round_trip() {
        assert_eq!(usd_to_price(200.0, D), Some(MID));
        assert!((price_to_usd(MID, D) - 200.0).abs() < 1e-9);
        assert_eq!(usd_to_price(-1.0, D), None);
        assert_eq!(usd_to_price(f64::NAN, D), None);
        assert_eq!(usd_to_price(1e30, D), None);
        assert_eq!(ui_to_atoms(1.5, 9), Some(1_500_000_000));
        assert_eq!(ui_to_atoms(-1.0, 9), None);
        assert!((atoms_to_ui(2_500_000, 6) - 2.5).abs() < 1e-12);
    }

    #[test]
    fn skew_follows_the_formula() {
        // Balanced inventory: no skew.
        assert!(skew_bps(Some(0.5), 3.0).abs() < 1e-12);
        // All base: full negative skew. All quote: full positive.
        assert!((skew_bps(Some(1.0), 3.0) + 3.0).abs() < 1e-12);
        assert!((skew_bps(Some(0.0), 3.0) - 3.0).abs() < 1e-12);
        // Quarter of the way: share 0.75 is half of the max.
        assert!((skew_bps(Some(0.75), 3.0) + 1.5).abs() < 1e-12);
        // The share is clamped, unknown inventory gives zero.
        assert!((skew_bps(Some(1.7), 3.0) + 3.0).abs() < 1e-12);
        assert!(skew_bps(None, 3.0).abs() < 1e-12);
    }

    #[test]
    fn share_is_valued_at_fair() {
        // 1000 SOL at 200 and 200,000 USDC: half and half.
        let share = base_value_share(1000.0, 200_000.0, 200.0).unwrap();
        assert!((share - 0.5).abs() < 1e-12);
        // Same balances, higher fair: base weighs more.
        assert!(base_value_share(1000.0, 200_000.0, 300.0).unwrap() > 0.5);
        assert_eq!(base_value_share(0.0, 0.0, 200.0), None);
        assert_eq!(base_value_share(1.0, 1.0, 0.0), None);
    }

    #[test]
    fn skewed_fair_shifts_by_bps() {
        assert!((skewed_fair(200.0, -3.0) - 199.94).abs() < 1e-9);
    }

    #[test]
    fn below_threshold_holds_and_above_sends() {
        let mut p = publisher();
        let e = p.evaluate(&inputs(100, Some(above(0))));
        assert_eq!(e.decision, Decision::Hold(Hold::Quiet));
        let e = p.evaluate(&inputs(101, Some(above(2))));
        assert_eq!(
            e.decision,
            Decision::Send {
                mid: above(2),
                clamped: false
            }
        );
        // A move down counts the same.
        let e = p.evaluate(&inputs(102, Some(MID - MID / 10_000 * 2)));
        assert!(matches!(e.decision, Decision::Send { .. }));
    }

    #[test]
    fn threshold_is_inclusive() {
        let mut p = publisher();
        let e = p.evaluate(&inputs(100, Some(above(1))));
        assert!(matches!(e.decision, Decision::Send { .. }));
    }

    #[test]
    fn heartbeat_sends_even_when_quiet() {
        let mut p = publisher();
        let mut i = inputs(100, Some(MID));
        i.last_update_slot = 91;
        assert_eq!(p.evaluate(&i).decision, Decision::Hold(Hold::Quiet));
        i.last_update_slot = 90;
        assert_eq!(
            p.evaluate(&i).decision,
            Decision::Send {
                mid: MID,
                clamped: false
            }
        );
    }

    #[test]
    fn at_most_one_send_per_slot() {
        let mut p = publisher();
        let i = inputs(100, Some(above(5)));
        assert!(matches!(p.evaluate(&i).decision, Decision::Send { .. }));
        p.record_sent(100);
        assert_eq!(p.evaluate(&i).decision, Decision::Hold(Hold::SameSlot));
        // An earlier slot (clock noise) never sends either.
        let earlier = inputs(99, Some(above(5)));
        assert_eq!(
            p.evaluate(&earlier).decision,
            Decision::Hold(Hold::SameSlot)
        );
        // The next slot may send.
        assert!(matches!(
            p.evaluate(&inputs(101, Some(above(5)))).decision,
            Decision::Send { .. }
        ));
    }

    #[test]
    fn pending_update_blocks_the_next_slot_only() {
        let mut p = publisher();
        p.record_sent(100);
        let mut i = inputs(101, Some(above(5)));
        i.pending = true;
        assert_eq!(p.evaluate(&i).decision, Decision::Hold(Hold::InFlight));
        i.slot = 102;
        assert!(matches!(p.evaluate(&i).decision, Decision::Send { .. }));
        i.slot = 101;
        i.pending = false;
        assert!(matches!(p.evaluate(&i).decision, Decision::Send { .. }));
    }

    #[test]
    fn target_is_clamped_to_the_band_around_the_onchain_mid() {
        let mut p = publisher();
        // 2% above, band is 1%.
        let e = p.evaluate(&inputs(100, Some(above(200))));
        assert_eq!(
            e.decision,
            Decision::Send {
                mid: above(100),
                clamped: true
            }
        );
        let e = p.evaluate(&inputs(101, Some(MID - MID / 10_000 * 200)));
        assert_eq!(
            e.decision,
            Decision::Send {
                mid: MID - MID / 100,
                clamped: true
            }
        );
        // The band edge itself is allowed and not reported as clamped.
        let e = p.evaluate(&inputs(102, Some(above(100))));
        assert_eq!(
            e.decision,
            Decision::Send {
                mid: above(100),
                clamped: false
            }
        );
    }

    #[test]
    fn long_clamp_streak_warns_and_re_arms() {
        let mut p = publisher();
        let far = Some(above(200));
        let warned: Vec<bool> = (100..=112)
            .map(|slot| p.evaluate(&inputs(slot, far)).clamp_warning)
            .collect();
        // Streak starts at 100; more than 5 slots later (106) it warns, then re-arms.
        assert_eq!(warned.iter().position(|w| *w), Some(6));
        assert_eq!(warned.iter().filter(|w| **w).count(), 2);
        // An unclamped slot resets the streak.
        assert!(!p.evaluate(&inputs(113, Some(MID))).clamp_warning);
        assert!(!p.evaluate(&inputs(114, far)).clamp_warning);
    }

    #[test]
    fn halt_and_not_tradable_send_nothing_even_on_heartbeat() {
        let mut p = publisher();
        let mut i = inputs(100, None);
        i.last_update_slot = 0;
        assert_eq!(p.evaluate(&i).decision, Decision::Hold(Hold::Halted));
        i.target = Some(above(50));
        i.paused = true;
        assert_eq!(p.evaluate(&i).decision, Decision::Hold(Hold::NotTradable));
        i.paused = false;
        i.onchain_mid = 0;
        assert_eq!(p.evaluate(&i).decision, Decision::Hold(Hold::NotTradable));
    }

    #[test]
    fn stale_state_sends_nothing_even_on_heartbeat() {
        let mut p = publisher();
        let mut i = inputs(100, Some(above(50)));
        i.last_update_slot = 0;
        i.state_age_ms = 6_000;
        assert!(matches!(p.evaluate(&i).decision, Decision::Send { .. }));
        i.state_age_ms = 6_001;
        assert_eq!(p.evaluate(&i).decision, Decision::Hold(Hold::StaleState));
    }

    #[test]
    fn heartbeat_must_be_below_max_age() {
        let p = publisher().params;
        assert!(p.validate(11).is_ok());
        assert!(p.validate(10).is_err());
        assert!(p.validate(0).is_err());
    }

    #[test]
    fn gap_bps_is_signed() {
        assert!((gap_bps(above(10), MID) - 10.0).abs() < 1e-6);
        assert!(gap_bps(MID - 1, MID) < 0.0);
        assert!(gap_bps(1, 0).is_infinite());
    }
}
