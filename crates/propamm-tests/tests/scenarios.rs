//! Scenario and attack tests: SOL/USDC (9/6 decimals), on-chain mid 200.00, half spread 2 bps.
//!
//! The external market price is a test variable. It is used only to value the maker's P&L:
//! `pnl = quote received - quote given + (base received - base given) * external price`,
//! read from the vault balances. Run with `--nocapture` to see the summary rows.

// `TxResult` (from `common`) holds LiteSVM's large error type.
#![allow(clippy::result_large_err)]
// Test arithmetic: every cast is on values bounded by u64 amounts and prices (checked by the asserts).
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

mod common;

use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use common::*;
use litesvm_token::{MintTo, TOKEN_ID};
use propamm::error::PropAmmError;
use propamm::instruction as ix;
use propamm::instructions::SwapSide;
use propamm::state::MarketParams;
use propamm_pricing::{PRICE_SCALE, Side, mul_div_ceil, mul_div_floor};
use solana_keypair::Keypair;
use solana_signer::Signer;

const SOL: u64 = 1_000_000_000;
const USDC: u64 = 1_000_000;
const BPS: u128 = 10_000;
const MAX_AGE: u64 = 8;
const MAX_MOVE_BPS: u16 = 100;
const HALF_SPREAD_BPS: u16 = 2;
/// Depth per side per window of `PARAMS_1` (and of `PARAMS_LADDER`).
const DEPTH: u64 = 50 * SOL;

/// One level of 50 SOL: depth per side per window is exactly 50 SOL, so prices are mid +- 2 bps.
const PARAMS_1: MarketParams = MarketParams {
    level_size_base_atoms: DEPTH,
    num_levels: 1,
    half_spread_bps: HALF_SPREAD_BPS,
    level_step_bps: 1,
    max_age_slots: MAX_AGE,
    max_move_bps: MAX_MOVE_BPS,
};
/// Same depth (80 SOL) spread over 8 levels of 10 SOL, 1 bps apart. Used to test order splitting.
const PARAMS_LADDER: MarketParams = MarketParams {
    level_size_base_atoms: 10 * SOL,
    num_levels: 8,
    ..PARAMS_1
};

/// Funding of the vaults and of the trader (so depth, not balances, is the limit).
const VAULT_SOL: u64 = 1_000 * SOL;
const VAULT_USDC: u64 = 1_000_000 * USDC;

/// Price in USDC cents to the program's scaled price (quote atoms per base atom x 1e12).
/// 200.00 USDC (20,000 cents) -> `200_000_000_000`.
const fn price(cents: u64) -> u64 {
    cents * 10_000_000
}
const FAIR: u64 = price(20_000);
/// External price 200.40 (+20 bps from `FAIR`, inside the 100 bps band) used by scenarios 1, 2 and 6.
const EXT: u64 = price(20_040);

struct World {
    env: Env,
    trader_base: Pubkey,
    trader_quote: Pubkey,
}

fn mint_to(env: &mut Env, mint: &Pubkey, account: &Pubkey, amount: u64) {
    let admin = env.admin.insecure_clone();
    MintTo::new(&mut env.svm, &admin, mint, account, amount)
        .send()
        .unwrap();
}

/// Market with funded vaults, price forced to 200.00 at slot 10. `trader` is `env.other`.
fn world(params: MarketParams) -> World {
    let mut env = Env::initialized_with(Env::bare_with_decimals(9), params);
    let (admin, base_mint, quote_mint) = (env.admin.pubkey(), env.base_mint, env.quote_mint);
    let (admin_base, admin_quote) = (env.admin_base, env.admin_quote);
    mint_to(&mut env, &base_mint, &admin_base, VAULT_SOL);
    mint_to(&mut env, &quote_mint, &admin_quote, VAULT_USDC);
    for (mint, vault, token, amount) in [
        (base_mint, env.base_vault, admin_base, VAULT_SOL),
        (quote_mint, env.quote_vault, admin_quote, VAULT_USDC),
    ] {
        let ix = env.deposit_ix(&admin, mint, vault, token, amount);
        env.send_admin(ix).unwrap();
    }
    let (admin_kp, other) = (env.admin.insecure_clone(), env.other.insecure_clone());
    let trader_quote = token_account(&mut env.svm, &admin_kp, &quote_mint, &other);
    let trader_base = env.other_base;
    mint_to(&mut env, &quote_mint, &trader_quote, VAULT_USDC);
    mint_to(&mut env, &base_mint, &trader_base, VAULT_SOL);
    env.svm.warp_to_slot(10);
    env.force(FAIR).unwrap();
    World {
        env,
        trader_base,
        trader_quote,
    }
}

impl World {
    fn swap(&mut self, side: SwapSide, amount_in: u64, min_out: u64) -> TxResult {
        let e = &self.env;
        let ix = Instruction {
            program_id: propamm::ID,
            accounts: propamm::accounts::Swap {
                trader: e.other.pubkey(),
                market: e.market,
                base_mint: e.base_mint,
                quote_mint: e.quote_mint,
                base_vault: e.base_vault,
                quote_vault: e.quote_vault,
                trader_base: self.trader_base,
                trader_quote: self.trader_quote,
                token_program: TOKEN_ID,
            }
            .to_account_metas(None),
            data: ix::Swap {
                side,
                amount_in,
                min_out,
            }
            .data(),
        };
        self.env.send_other(ix)
    }

    /// Quote atoms needed to buy `base` atoms at ask level 0 of the current mid (rounded up).
    fn buy_cost(&self, base: u64) -> u64 {
        let ask = self.env.state().ladder().ask_price(0).unwrap();
        mul_div_ceil(u128::from(base), u128::from(ask), PRICE_SCALE)
            .unwrap()
            .try_into()
            .unwrap()
    }

    /// Buy exactly `base` atoms at the current level-0 ask (`min_out = base`).
    fn buy_base(&mut self, base: u64) -> TxResult {
        let cost = self.buy_cost(base);
        self.swap(SwapSide::BuyBase, cost, base)
    }

    fn sell_base(&mut self, base: u64) -> TxResult {
        self.swap(SwapSide::SellBase, base, 0)
    }

    fn slot(&mut self, slot: u64) {
        self.env.svm.warp_to_slot(slot);
    }

    /// `(base vault, quote vault)` balances.
    fn vaults(&self) -> (i128, i128) {
        let e = &self.env;
        (
            i128::from(e.balance(&e.base_vault)),
            i128::from(e.balance(&e.quote_vault)),
        )
    }

    /// Maker P&L in quote atoms since `before`, valuing base at `ext` (scaled price).
    fn pnl(&self, before: (i128, i128), ext: u64) -> i128 {
        let (b, q) = self.vaults();
        (q - before.1) + quote_value(b - before.0, ext)
    }

    /// Slot N-1 (a forced 200.00 mid) then slot N = 20. Returns the vault balances at N.
    fn at_slot_n(&mut self) -> (i128, i128) {
        self.slot(19);
        self.env.force(FAIR).unwrap();
        self.slot(20);
        self.vaults()
    }
}

/// Quote atoms worth `base_atoms` at the scaled `price` (truncated toward zero).
fn quote_value(base_atoms: i128, price: u64) -> i128 {
    base_atoms * i128::from(price) / i128::try_from(PRICE_SCALE).unwrap()
}

/// How far the band lets the mid move in one slot.
fn band_move(mid: u64) -> u64 {
    (u128::from(mid) * u128::from(MAX_MOVE_BPS) / BPS) as u64
}

/// Quote atoms as signed USDC with 6 decimals, e.g. `-17.599200`.
fn usdc(atoms: i128) -> String {
    let sign = if atoms < 0 { "-" } else { "+" };
    let a = atoms.unsigned_abs();
    format!("{sign}{}.{:06}", a / 1_000_000, a % 1_000_000)
}

fn row(scenario: &str, pnl: &str, notes: &str) {
    println!("| {scenario:<44} | {pnl:>12} USDC | {notes}");
}

/// Off-chain P&L of an arbitrageur who buys `base` at the market's current quote and values it at `ext`.
fn arb_pnl_if_buying(w: &World, base: u64, ext: u64) -> i128 {
    let cost = w.buy_cost(base);
    let s = w.env.state();
    let fill = s
        .ladder()
        .quote_exact_in(Side::BuyBase, s.consumed(Side::BuyBase), cost)
        .unwrap();
    let value = mul_div_floor(u128::from(fill.amount_out), u128::from(ext), PRICE_SCALE).unwrap();
    value as i128 - i128::from(cost)
}

// Scenario 1. Slot N, external price 200.40, on-chain mid 200.00 since slot N-1.
// The updater's update lands first, so the ask is 200.44008 and the maker is on the right side.
#[test]
fn update_first() {
    let mut w = world(PARAMS_1);
    let before = w.at_slot_n();
    w.env.update(EXT).unwrap(); // update_price(200.40) lands first

    // The would-be arbitrageur buys 50 SOL at the new ask 200.44008 against an external 200.40: a loss.
    let arb = arb_pnl_if_buying(&w, DEPTH, EXT);
    assert_eq!(arb, -2_004_000, "50 SOL x 0.04008"); // exact: both prices are exact in atoms

    // The trader buys 10 SOL at 200.44008. Maker earns 10 x (200.44008 - 200.40) = 0.4008.
    w.buy_base(10 * SOL).unwrap();
    let pnl = w.pnl(before, EXT);
    assert_eq!(pnl, 400_800);
    row(
        "1 update_first",
        &usdc(pnl),
        &format!("trader 10 SOL @200.44008; arb would lose {}", usdc(arb)),
    );
}

// Scenario 2. Same conditions, but the arbitrageur's swap lands before the update.
#[test]
fn update_late() {
    let mut w = world(PARAMS_1);
    let before = w.at_slot_n();

    // A buy larger than the window depth (50 SOL) fails: the loss is bounded by the depth.
    let too_big = w.buy_cost(DEPTH) + USDC;
    assert_program_err(
        &w.swap(SwapSide::BuyBase, too_big, 0),
        PropAmmError::InsufficientDepth,
    );
    // The arbitrageur buys the full 50 SOL at the stale ask 200.04. Maker loses 50 x (200.40 - 200.04) = 18.00.
    w.buy_base(DEPTH).unwrap();
    let after_arb = w.pnl(before, EXT);
    assert_eq!(after_arb, -18 * i128::from(USDC));
    // The window is exhausted: even 1 more USDC of buy fails.
    assert_program_err(
        &w.swap(SwapSide::BuyBase, USDC, 0),
        PropAmmError::InsufficientDepth,
    );

    // Then the update lands (fresh window), and the trader buys 10 SOL at 200.44008 (+0.4008).
    w.env.update(EXT).unwrap();
    w.buy_base(10 * SOL).unwrap();
    let pnl = w.pnl(before, EXT);
    assert_eq!(pnl, -17_599_200, "-18 + 0.4008");
    row(
        "2 update_late",
        &usdc(pnl),
        &format!(
            "arb 50 SOL @200.04 = {}; trader 10 SOL = +0.4008",
            usdc(after_arb)
        ),
    );
}

// Scenario 3. No update for max_age_slots + 1: every swap is rejected; an update or force_price restores trading.
#[test]
fn stale_rejected() {
    let mut w = world(PARAMS_1); // price posted at slot 10
    let before = w.vaults();

    // Boundary: age == max_age is still fresh.
    w.slot(10 + MAX_AGE);
    w.buy_base(SOL).unwrap();
    // age == max_age + 1: arb and trader are both rejected, on both sides.
    w.slot(10 + MAX_AGE + 1);
    assert_program_err(&w.sell_base(SOL), PropAmmError::StalePrice); // arb
    assert_program_err(
        &w.swap(SwapSide::BuyBase, 100 * USDC, 0),
        PropAmmError::StalePrice,
    ); // trader
    assert_program_err(
        &w.swap(SwapSide::SellBase, SOL, 0),
        PropAmmError::StalePrice,
    );

    // update_price within the band of the last mid restores trading.
    let mid = w.env.state().mid_price;
    w.env.update(mid + mid / 1_000).unwrap(); // +10 bps
    w.buy_base(SOL).unwrap();

    // Stale again: admin force_price restores it too.
    let posted = w.env.state().last_update_slot;
    w.slot(posted + MAX_AGE + 1);
    assert_program_err(&w.buy_base(SOL), PropAmmError::StalePrice);
    w.env.force(FAIR).unwrap();
    w.buy_base(SOL).unwrap();

    // Nothing was taken while stale: only the three 1 SOL buys filled, at asks 200.04, 200.24004 and 200.04.
    let pnl = w.pnl(before, FAIR);
    assert_eq!(pnl, 320_040, "0.04 + 0.24004 + 0.04");
    row(
        "3 stale_rejected",
        &usdc(pnl),
        "swaps fail StalePrice at age 9; update_price / force_price restore",
    );
}

// Scenario 4. Splitting an order never gets a better total than a single order on an identical market.
/// Base atoms the trader gets for `total_quote` spent in `parts` equal swaps on a fresh market.
fn buy_in_parts(params: MarketParams, total_quote: u64, parts: u64) -> u64 {
    let mut w = world(params);
    let base_before = w.env.balance(&w.trader_base);
    for _ in 0..parts {
        w.swap(SwapSide::BuyBase, total_quote / parts, 0).unwrap();
    }
    w.env.balance(&w.trader_base) - base_before
}

#[test]
fn split_order_never_helps() {
    // One level: the price is flat, so splitting changes nothing (50 SOL = 10 x 5 SOL).
    let q1 = 10_002 * USDC;
    let (single1, split1) = (
        buy_in_parts(PARAMS_1, q1, 1),
        buy_in_parts(PARAMS_1, q1, 10),
    );
    assert_eq!(split1, single1);
    // Eight levels: 50 SOL walks levels 0..=4 (200.04 .. 200.12); consumed carries across the splits.
    let q8 = 10_004 * USDC;
    let (single8, split8) = (
        buy_in_parts(PARAMS_LADDER, q8, 1),
        buy_in_parts(PARAMS_LADDER, q8, 10),
    );
    assert!(split8 <= single8, "split {split8} > single {single8}");
    row(
        "4 split_order_never_helps",
        "n/a",
        &format!("1 lvl: {single1} vs {split1}; 8 lvl: {single8} vs {split8} base atoms"),
    );
}

// Scenario 5.
//
// Bound (docs/design-onchain.md, residual risk). One update per slot and `max_move_bps = b`: the
// mid moves at most floor(m*b/BPS) per slot, compounding. Each window offers at most D = depth per
// side, and over K slots there are at most K+1 windows (the one before the first update plus K
// fresh ones). Valued at a fixed fair price F, per side:
//   buy side  loss <= sum_{k=0..K} D * max(0, F - ask(L_k)),   L_0 = F, L_k = L_{k-1} - floor(L_{k-1}*b/BPS)
//   sell side loss <= sum_{k=0..K} D * max(0, bid(U_k) - F),   U_0 = F, U_k = U_{k-1} + floor(U_{k-1}*b/BPS)
// The attacker moves the mid one way, so each side is bounded on its own. The coarser closed form is
// (K+1) * D * F * (1 - (1-b)^K (1+h)) per side, because every window's ask is at least ask(L_K) and
// every bid at most bid(U_K). Returns `(tight, closed_form)` in quote atoms, each term rounded up.
fn bound(k: u64, depth: u64, down: bool) -> (i128, i128) {
    let h = u128::from(HALF_SPREAD_BPS);
    let (mut mid, mut tight, mut worst) = (FAIR, 0, 0);
    for _ in 0..=k {
        let price_loss = if down {
            let ask = mul_div_ceil(u128::from(mid), BPS + h, BPS).unwrap() as u64;
            i128::from(FAIR) - i128::from(ask)
        } else {
            let bid = mul_div_floor(u128::from(mid), BPS - h, BPS).unwrap() as u64;
            i128::from(bid) - i128::from(FAIR)
        };
        let price_loss = u128::try_from(price_loss.max(0)).unwrap();
        worst = mul_div_ceil(u128::from(depth), price_loss, PRICE_SCALE).unwrap() as i128;
        tight += worst;
        mid = if down {
            mid - band_move(mid)
        } else {
            mid + band_move(mid)
        };
    }
    // `worst` is the last window's term, the largest since the mid only moves one way.
    (tight, i128::from(k + 1) * worst)
}

/// The attacker drives the mid to the band edge each slot (`down`: buy cheap; else sell dear) and trades full depth.
fn attack(w: &mut World, k: u64, down: bool) {
    for i in 1..=k {
        w.slot(10 + i);
        let mid = w.env.state().mid_price;
        let delta = band_move(mid);
        w.env
            .update(if down { mid - delta } else { mid + delta })
            .unwrap(); // one update per slot, at the band edge
        if down {
            w.buy_base(DEPTH).unwrap();
        } else {
            w.sell_base(DEPTH).unwrap();
        }
    }
}

#[test]
fn compromised_updater_loss_is_bounded() {
    const K: u64 = 8;
    let [(mut w, buy_loss), (_, sell_loss)] = [true, false].map(|down| {
        let mut w = world(PARAMS_1);
        let before = w.vaults();
        attack(&mut w, K, down);
        let loss = -w.pnl(before, FAIR);
        (w, loss)
    });
    for (down, side, loss) in [(true, "buy", buy_loss), (false, "sell", sell_loss)] {
        let (tight, closed_form) = bound(K, DEPTH, down);
        assert!(loss > 0, "attacker profits: {}", usdc(loss));
        assert!(loss <= tight, "{side}: {} > {}", usdc(loss), usdc(tight));
        assert!(tight <= closed_form);
        row(
            &format!("5 compromised_updater ({side} side, K={K})"),
            &usdc(-loss),
            &format!("bound {} (closed form {})", usdc(tight), usdc(closed_form)),
        );
    }

    // Incident runbook on the buy-side market: pause -> set_updater(new) -> force_price(fair) -> unpause.
    let admin = w.env.admin.pubkey();
    let new_updater = Keypair::new();
    let ix = w.env.set_paused_ix(&admin, true);
    w.env.send_admin(ix).unwrap();
    let ix = w.env.set_updater_ix(&admin, new_updater.pubkey());
    w.env.send_admin(ix).unwrap();
    w.env.force(FAIR).unwrap();
    let ix = w.env.set_paused_ix(&admin, false);
    w.env.send_admin(ix).unwrap();
    assert_eq!(w.env.state().mid_price, FAIR);

    w.slot(10 + K + 2);
    // The old key can no longer post prices; the new key can.
    assert_program_err(&w.env.update(FAIR), PropAmmError::Unauthorized);
    w.env
        .svm
        .airdrop(&new_updater.pubkey(), 1_000_000_000)
        .unwrap();
    let ix = w.env.update_price_ix(&new_updater.pubkey(), FAIR);
    send(&mut w.env.svm, ix, &new_updater).unwrap();
    // Swaps work again at the forced price: 1 SOL costs ask(200.00) = 200.04 USDC.
    assert_eq!(w.buy_cost(SOL), 200_040_000);
    w.buy_base(SOL).unwrap();
}

// Scenario 6. The updater stops after the trader took 10 SOL; the external price jumps to 200.40.
#[test]
fn stalled_updater_exposes_only_remaining_depth() {
    let mut w = world(PARAMS_1); // price posted at slot 10
    w.slot(11);
    w.buy_base(10 * SOL).unwrap(); // consumes 10 of 50 SOL of ask depth
    let remaining = DEPTH - 10 * SOL;
    assert_eq!(w.env.consumed().0, 10 * SOL);
    let before = w.vaults();

    // Last fresh slot (age == max_age). More than the remaining depth is refused on both sides.
    w.slot(10 + MAX_AGE);
    assert_program_err(
        &w.swap(SwapSide::BuyBase, w.buy_cost(remaining) + USDC, 0),
        PropAmmError::InsufficientDepth,
    );
    assert_program_err(&w.sell_base(DEPTH + 1), PropAmmError::InsufficientDepth);
    // The arbitrageur takes exactly the remaining depth at the stale ask 200.04.
    w.buy_base(remaining).unwrap();
    let pnl = w.pnl(before, EXT);
    assert_eq!(pnl, -14_400_000, "40 SOL x (200.40 - 200.04)");
    assert_eq!(
        before.0 - w.vaults().0,
        i128::from(remaining),
        "base exposure = remaining depth"
    );
    assert_program_err(&w.buy_base(1), PropAmmError::InsufficientDepth);

    // One slot later the price is stale and nothing else can be taken.
    w.slot(10 + MAX_AGE + 1);
    assert_program_err(&w.buy_base(SOL), PropAmmError::StalePrice);
    assert_program_err(&w.sell_base(SOL), PropAmmError::StalePrice);
    row(
        "6 stalled_updater",
        &usdc(pnl),
        "remaining ask depth 40 SOL taken @200.04 vs 200.40, then StalePrice",
    );
}
