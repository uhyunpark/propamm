//! Pins the `MarketState` byte layout shared by the program and the Pinocchio port.

use core::mem::{align_of, offset_of, size_of};
use propamm_pricing::{MARKET_STATE_SIZE, MarketState, Side};

#[test]
fn size_and_align() {
    assert_eq!(size_of::<MarketState>(), 264);
    assert_eq!(size_of::<MarketState>(), MARKET_STATE_SIZE);
    assert_eq!(align_of::<MarketState>(), 8);
}

#[test]
fn field_offsets() {
    assert_eq!(offset_of!(MarketState, admin), 0);
    assert_eq!(offset_of!(MarketState, quote_vault), 160);
    assert_eq!(offset_of!(MarketState, mid_price), 192);
    assert_eq!(offset_of!(MarketState, last_update_slot), 200);
    assert_eq!(offset_of!(MarketState, ask_consumed_base_atoms), 208);
    assert_eq!(offset_of!(MarketState, bid_consumed_base_atoms), 216);
    assert_eq!(offset_of!(MarketState, level_size_base_atoms), 224);
    assert_eq!(offset_of!(MarketState, max_age_slots), 232);
    assert_eq!(offset_of!(MarketState, half_spread_bps), 240);
    assert_eq!(offset_of!(MarketState, num_levels), 246);
    assert_eq!(offset_of!(MarketState, padding), 249);
    // Appended after the padding.
    assert_eq!(offset_of!(MarketState, window_start_mid), 256);
}

#[test]
fn bytemuck_roundtrip() {
    let mut buf = [0_u8; MARKET_STATE_SIZE];
    let state: &MarketState = bytemuck::from_bytes(&buf);
    assert_eq!(*state, bytemuck::Zeroable::zeroed());
    assert!(!state.is_paused());

    let mut state = *state;
    state.mid_price = 200_000_000_000;
    state.num_levels = 4;
    state.paused = 1;
    state.set_consumed(Side::BuyBase, 7);
    state.set_consumed(Side::SellBase, 9);
    buf.copy_from_slice(bytemuck::bytes_of(&state));

    let back: &MarketState = bytemuck::from_bytes(&buf);
    assert_eq!(*back, state);
    assert_eq!(back.consumed(Side::BuyBase), 7);
    assert_eq!(back.consumed(Side::SellBase), 9);
    assert_eq!(back.ladder().mid_price, 200_000_000_000);
    assert!(back.is_paused());
    // mid_price lives at byte 192, little-endian.
    assert_eq!(buf[192..200], 200_000_000_000_u64.to_le_bytes());
}
