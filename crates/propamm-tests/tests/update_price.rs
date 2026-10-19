//! `update_price` and `force_price`, against the real program in `LiteSVM`.

mod common;

use common::*;
use propamm::error::PropAmmError;
use solana_signer::Signer;

/// With `max_move_bps = 100` the band is 1% of `MID`.
const BAND: u64 = 2_000_000_000;

/// Initialized market, warped to slot 10, first price forced to `MID` in slot 10.
fn priced() -> Env {
    let mut env = Env::initialized();
    env.svm.warp_to_slot(10);
    env.force(MID).unwrap();
    env
}

/// `(mid, ask consumed, bid consumed, window_start_mid, last_update_slot)`.
fn snapshot(env: &Env) -> (u64, u64, u64, u64, u64) {
    let s = env.state();
    (
        s.mid_price,
        s.ask_consumed_base_atoms,
        s.bid_consumed_base_atoms,
        s.window_start_mid,
        s.last_update_slot,
    )
}

#[test]
fn force_price_by_admin_sets_window_and_resets_consumed() {
    let mut env = Env::initialized();
    env.svm.warp_to_slot(10);
    env.edit_state(|s| {
        s.ask_consumed_base_atoms = 5;
        s.bid_consumed_base_atoms = 6;
        s.window_start_mid = 1;
    });
    env.force(MID).unwrap();
    let s = env.state();
    assert_eq!(
        (
            s.mid_price,
            s.window_start_mid,
            s.last_update_slot,
            s.ask_consumed_base_atoms,
            s.bid_consumed_base_atoms
        ),
        (MID, MID, 10, 0, 0)
    );
    // No move limit: a 10x jump is fine for the admin.
    env.svm.warp_to_slot(11);
    env.force(MID * 10).unwrap();
    assert_eq!(env.state().mid_price, MID * 10);
    assert_eq!(env.state().window_start_mid, MID * 10);
    assert_eq!(env.state().last_update_slot, 11);
}

#[test]
fn force_price_rejects_non_admin_and_updater() {
    let mut env = Env::initialized();
    let before = env.state();
    let other = env.other.pubkey();
    let ix = env.force_price_ix(&other, MID);
    assert_program_err(&env.send_other(ix), PropAmmError::Unauthorized);
    let updater = env.updater.pubkey();
    let ix = env.force_price_ix(&updater, MID);
    assert_program_err(&env.send_updater(ix), PropAmmError::Unauthorized);
    assert_eq!(env.state(), before);
}

#[test]
fn force_price_validates_the_ladder() {
    let mut env = Env::initialized();
    let before = env.state();
    // mid 0 and a mid so small that the deepest bid floors to zero.
    for bad in [0, 1] {
        assert_program_err(&env.force(bad), PropAmmError::InvalidParams);
    }
    assert_eq!(env.state(), before);
}

#[test]
fn update_price_before_force_price_fails() {
    let mut env = Env::initialized();
    env.svm.warp_to_slot(10);
    assert_program_err(&env.update(MID), PropAmmError::NotInitializedPrice);
    assert_eq!(env.state().mid_price, 0);
}

#[test]
fn update_price_authorization() {
    let mut env = priced();
    env.svm.warp_to_slot(11);
    let before = env.state();
    let admin = env.admin.pubkey();
    let ix = env.update_price_ix(&admin, MID + 1);
    assert_program_err(&env.send_admin(ix), PropAmmError::Unauthorized);
    let other = env.other.pubkey();
    let ix = env.update_price_ix(&other, MID + 1);
    assert_program_err(&env.send_other(ix), PropAmmError::Unauthorized);
    assert_eq!(env.state(), before);

    env.update(MID + 1).unwrap();
    let s = env.state();
    assert_eq!((s.mid_price, s.last_update_slot), (MID + 1, 11));
}

#[test]
fn update_price_validates_the_ladder() {
    let mut env = priced();
    // Open the band fully so only ladder validation can reject: mid 1 floors the deepest bid to 0.
    env.edit_state(|s| s.max_move_bps = 10_000);
    env.svm.warp_to_slot(11);
    assert_program_err(&env.update(1), PropAmmError::InvalidParams);
    assert_program_err(&env.update(0), PropAmmError::InvalidParams);
    assert_eq!(env.state().mid_price, MID);
}

#[test]
fn move_limit_is_inclusive_on_both_sides() {
    for (ok, bad) in [(MID + BAND, MID + BAND + 1), (MID - BAND, MID - BAND - 1)] {
        let mut env = priced();
        env.edit_state(|s| {
            s.ask_consumed_base_atoms = 11;
            s.bid_consumed_base_atoms = 22;
        });
        env.svm.warp_to_slot(11);
        let before = snapshot(&env);
        assert_program_err(&env.update(bad), PropAmmError::PriceMoveTooLarge);
        assert_eq!(snapshot(&env), before);
        env.update(ok).unwrap();
        assert_eq!(env.state().mid_price, ok);
    }
}

#[test]
fn second_update_in_a_slot_is_rejected() {
    let mut env = priced();
    env.edit_state(|s| {
        s.ask_consumed_base_atoms = 11;
        s.bid_consumed_base_atoms = 22;
    });
    env.svm.warp_to_slot(11);
    env.update(MID + 1).unwrap();
    // Neither a further move, nor the same mid, nor a move back is accepted in slot 11.
    let before = snapshot(&env);
    for mid in [MID + 2, MID + 1, MID] {
        assert_program_err(&env.update(mid), PropAmmError::AlreadyUpdatedThisSlot);
        assert_eq!(snapshot(&env), before);
    }
    // The next slot allows one more update, anchored at the previous mid.
    env.svm.warp_to_slot(12);
    env.update(MID + 1 + BAND).unwrap();
    assert_eq!(env.state().window_start_mid, MID + 1);
}

#[test]
fn update_in_the_force_price_slot_is_rejected() {
    let mut env = priced();
    let before = snapshot(&env);
    assert_program_err(&env.update(MID + 1), PropAmmError::AlreadyUpdatedThisSlot);
    assert_eq!(snapshot(&env), before);
}

#[test]
fn every_update_starts_a_window() {
    let mut env = priced();
    env.edit_state(|s| {
        s.ask_consumed_base_atoms = 11;
        s.bid_consumed_base_atoms = 22;
    });
    env.svm.warp_to_slot(11);
    env.update(MID + 5).unwrap();
    assert_eq!(env.consumed(), (0, 0));
    let s = env.state();
    assert_eq!(s.window_start_mid, MID);
    assert_eq!((s.mid_price, s.last_update_slot), (MID + 5, 11));

    env.edit_state(|s| s.ask_consumed_base_atoms = 3);
    env.svm.warp_to_slot(12);
    env.update(MID + 9).unwrap();
    assert_eq!(env.consumed(), (0, 0));
    let s = env.state();
    assert_eq!(s.window_start_mid, MID + 5);
    assert_eq!((s.mid_price, s.last_update_slot), (MID + 9, 12));
}

#[test]
fn update_in_an_earlier_slot_fails() {
    let mut env = priced();
    env.edit_state(|s| {
        s.ask_consumed_base_atoms = 11;
        s.bid_consumed_base_atoms = 22;
    });
    env.svm.warp_to_slot(9);
    let before = snapshot(&env);
    assert_program_err(&env.update(MID), PropAmmError::SlotRegression);
    assert_eq!(snapshot(&env), before);
}

#[test]
fn set_params_invalidation_clears_on_the_next_update_in_a_later_slot() {
    let mut env = priced();
    env.svm.warp_to_slot(11);
    env.update(MID + 1).unwrap();
    let admin = env.admin.pubkey();
    let ix = env.set_params_ix(&admin, PARAMS);
    env.send_admin(ix).unwrap();
    assert_eq!(env.consumed(), (u64::MAX, u64::MAX));
    // set_params in the slot of the last update: an update in the same slot is rejected,
    // so the sentinel stays.
    assert_program_err(&env.update(MID + 2), PropAmmError::AlreadyUpdatedThisSlot);
    assert_eq!(env.consumed(), (u64::MAX, u64::MAX));
    env.svm.warp_to_slot(12);
    env.update(MID + 2).unwrap();
    assert_eq!(env.consumed(), (0, 0));
}

#[test]
fn paused_blocks_update_price_but_not_force_price() {
    let mut env = priced();
    let admin = env.admin.pubkey();
    let ix = env.set_paused_ix(&admin, true);
    env.send_admin(ix).unwrap();
    env.svm.warp_to_slot(11);
    assert_program_err(&env.update(MID + 1), PropAmmError::Paused);
    env.force(MID * 2).unwrap();
    assert_eq!(env.state().mid_price, MID * 2);
    assert_eq!(env.state().paused, 1);
}
