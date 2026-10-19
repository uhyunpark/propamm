//! `swap`, against the real program in `LiteSVM`.

// `TxResult` (from `common`) holds LiteSVM's large error type, so every helper returning it trips this lint.
#![allow(clippy::result_large_err)]

mod common;

use anchor_lang::__private::base64::{Engine, engine::general_purpose::STANDARD};
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AnchorDeserialize, Discriminator, InstructionData, ToAccountMetas};
use common::*;
use litesvm_token::{CreateAccount, CreateMint, MintTo, TOKEN_ID};
use propamm::error::PropAmmError;
use propamm::instruction as ix;
use propamm::instructions::{SwapEvent, SwapSide};
use propamm::state::MarketParams;
use propamm_pricing::{PRICE_SCALE, Side, mul_div_ceil};
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Anchor framework error codes for constraint failures.
const CONSTRAINT_TOKEN_MINT: u32 = 2014;
const CONSTRAINT_TOKEN_OWNER: u32 = 2015;
const INVALID_PROGRAM_ID: u32 = 3008;
const OWNED_BY_WRONG_PROGRAM: u32 = 3007;
const DUPLICATE_MUTABLE_ACCOUNT: u32 = 2040;
/// SPL Token `InsufficientFunds`.
const SPL_INSUFFICIENT_FUNDS: u32 = 1;
const TOKEN_2022_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Funded generously so depth, not balances, is the limit unless a test says otherwise.
const FUND: u64 = 5_000_000_000;
/// Quote atoms for a mid-sized buy (about 500 base atoms of depth at `MID`: 0.2 quote per base atom).
const BUY: u64 = 100_000_000;
/// Base atoms for a mid-sized sell.
const SELL: u64 = 500_000_000;
// Measured (LiteSVM, release .so): BuyBase 14,945 CU, SellBase 14,448 CU. Budgets leave ~20% headroom.
const BUY_CU_BUDGET: u64 = 17_700;
const SELL_CU_BUDGET: u64 = 17_100;
// Worst case: 32 levels, the amount walks all of them. Measured (LiteSVM, release .so): BuyBase 31,154 CU, SellBase 26,514 CU; ~20% headroom.
const BUY_WORST_CU_BUDGET: u64 = 37_400;
const SELL_WORST_CU_BUDGET: u64 = 31_900;
/// Level size of the 32-level market (small so the vaults and the trader can cover the full depth).
const DEEP_LEVEL: u64 = 10_000_000;

struct Trade {
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

/// Market with both vaults funded, the first price forced to `MID` at slot 10. The trader is `other`.
fn build(base_fund: u64, quote_fund: u64, priced: bool) -> Trade {
    let mut env = Env::initialized();
    let (admin, base_mint, quote_mint) = (env.admin.pubkey(), env.base_mint, env.quote_mint);
    let admin_base = env.admin_base;
    mint_to(&mut env, &base_mint, &admin_base, FUND);
    let admin_quote = env.admin_quote;
    mint_to(&mut env, &quote_mint, &admin_quote, FUND);
    for (mint, vault, token, amount) in [
        (base_mint, env.base_vault, admin_base, base_fund),
        (quote_mint, env.quote_vault, admin_quote, quote_fund),
    ] {
        let ix = env.deposit_ix(&admin, mint, vault, token, amount);
        env.send_admin(ix).unwrap();
    }
    let (admin_kp, other) = (env.admin.insecure_clone(), env.other.insecure_clone());
    let trader_quote = token_account(&mut env.svm, &admin_kp, &quote_mint, &other);
    let trader_base = env.other_base;
    mint_to(&mut env, &quote_mint, &trader_quote, FUND);
    mint_to(&mut env, &base_mint, &trader_base, FUND);
    env.svm.warp_to_slot(10);
    if priced {
        env.force(MID).unwrap();
    }
    Trade {
        env,
        trader_base,
        trader_quote,
    }
}

fn setup_with(base_fund: u64, quote_fund: u64) -> Trade {
    build(base_fund, quote_fund, true)
}

fn setup() -> Trade {
    setup_with(FUND, FUND)
}

impl Trade {
    fn accounts(&self) -> propamm::accounts::Swap {
        let e = &self.env;
        propamm::accounts::Swap {
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
    }

    fn ix_with(
        accounts: &propamm::accounts::Swap,
        side: SwapSide,
        amount_in: u64,
        min_out: u64,
    ) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: accounts.to_account_metas(None),
            data: ix::Swap {
                side,
                amount_in,
                min_out,
            }
            .data(),
        }
    }

    fn swap_with(
        &mut self,
        accounts: &propamm::accounts::Swap,
        side: SwapSide,
        amount_in: u64,
        min_out: u64,
    ) -> TxResult {
        let ix = Self::ix_with(accounts, side, amount_in, min_out);
        self.env.send_other(ix)
    }

    fn swap(&mut self, side: SwapSide, amount_in: u64, min_out: u64) -> TxResult {
        let accounts = self.accounts();
        self.swap_with(&accounts, side, amount_in, min_out)
    }

    /// `(trader base, trader quote, base vault, quote vault)`.
    fn balances(&self) -> (u64, u64, u64, u64) {
        let e = &self.env;
        (
            e.balance(&self.trader_base),
            e.balance(&self.trader_quote),
            e.balance(&e.base_vault),
            e.balance(&e.quote_vault),
        )
    }
}

fn buy() -> SwapSide {
    SwapSide::BuyBase
}
fn sell() -> SwapSide {
    SwapSide::SellBase
}

/// Off-chain quote on the market's current state.
fn quote(env: &Env, side: Side, amount_in: u64) -> propamm_pricing::Fill {
    let s = env.state();
    s.ladder()
        .quote_exact_in(side, s.consumed(side), amount_in)
        .unwrap()
}

#[test]
fn buy_happy_path_matches_offchain_quote() {
    let mut t = setup();
    let (tb, tq, bv, qv) = t.balances();
    let before = t.env.state();
    let fill = quote(&t.env, Side::BuyBase, BUY);
    t.env.svm.warp_to_slot(20);
    t.swap(buy(), BUY, fill.amount_out).unwrap();
    assert_eq!(
        t.balances(),
        (
            tb + fill.amount_out,
            tq - BUY,
            bv - fill.amount_out,
            qv + BUY
        )
    );
    let after = t.env.state();
    assert_eq!(after.ask_consumed_base_atoms, fill.consumed_after);
    assert_eq!(after.ask_consumed_base_atoms, fill.amount_out);
    let mut expected = before;
    expected.ask_consumed_base_atoms = fill.consumed_after;
    assert_eq!(after, expected, "only ask consumed may change");
}

#[test]
fn sell_happy_path_matches_offchain_quote() {
    let mut t = setup();
    let (tb, tq, bv, qv) = t.balances();
    let before = t.env.state();
    let fill = quote(&t.env, Side::SellBase, SELL);
    t.env.svm.warp_to_slot(20);
    t.swap(sell(), SELL, fill.amount_out).unwrap();
    assert_eq!(
        t.balances(),
        (
            tb - SELL,
            tq + fill.amount_out,
            bv + SELL,
            qv - fill.amount_out
        )
    );
    let after = t.env.state();
    assert_eq!(after.bid_consumed_base_atoms, SELL);
    let mut expected = before;
    expected.bid_consumed_base_atoms = fill.consumed_after;
    assert_eq!(after, expected, "only bid consumed may change");
}

#[test]
fn min_out_boundary() {
    let mut t = setup();
    let out = quote(&t.env, Side::BuyBase, BUY).amount_out;
    let (before, bal) = (t.env.state(), t.balances());
    assert_program_err(&t.swap(buy(), BUY, out + 1), PropAmmError::SlippageExceeded);
    assert_eq!((t.env.state(), t.balances()), (before, bal));
    t.swap(buy(), BUY, out).unwrap();
    assert_eq!(t.balances().0, bal.0 + out);
}

#[test]
fn paused_rejects_and_unpause_restores() {
    let mut t = setup();
    let admin = t.env.admin.pubkey();
    let ix = t.env.set_paused_ix(&admin, true);
    t.env.send_admin(ix).unwrap();
    assert_program_err(&t.swap(buy(), BUY, 0), PropAmmError::Paused);
    assert_program_err(&t.swap(sell(), SELL, 0), PropAmmError::Paused);
    let ix = t.env.set_paused_ix(&admin, false);
    t.env.send_admin(ix).unwrap();
    t.swap(buy(), BUY, 0).unwrap();
}

#[test]
fn unpriced_market_rejects() {
    let mut t = build(FUND, FUND, false);
    assert_eq!(t.env.state().mid_price, 0);
    assert_program_err(&t.swap(buy(), BUY, 0), PropAmmError::NotInitializedPrice);
    assert_program_err(&t.swap(sell(), SELL, 0), PropAmmError::NotInitializedPrice);
}

#[test]
fn staleness_boundary() {
    let mut t = setup();
    let max_age = t.env.state().max_age_slots;
    // Price posted at slot 10: age == max_age is still fresh.
    t.env.svm.warp_to_slot(10 + max_age);
    t.swap(buy(), BUY, 0).unwrap();
    let (before, bal) = (t.env.state(), t.balances());
    t.env.svm.warp_to_slot(10 + max_age + 1);
    assert_program_err(&t.swap(buy(), BUY, 0), PropAmmError::StalePrice);
    assert_program_err(&t.swap(sell(), SELL, 0), PropAmmError::StalePrice);
    assert_eq!((t.env.state(), t.balances()), (before, bal));
    // A fresh update makes it tradable again.
    t.env.update(MID).unwrap();
    t.swap(buy(), BUY, 0).unwrap();
}

#[test]
fn depth_exhaustion_accumulation_and_refresh() {
    let mut t = setup();
    let depth = t.env.state().ladder().depth_base_atoms().unwrap();
    let mut total_out = 0;
    // Consumed accumulates across swaps within the slot window.
    for _ in 0..3 {
        let fill = quote(&t.env, Side::BuyBase, BUY);
        t.swap(buy(), BUY, 0).unwrap();
        total_out += fill.amount_out;
        assert_eq!(t.env.consumed().0, total_out);
    }
    // Buying more than the remaining depth fails with no partial fill.
    let too_much = 800_000_000; // quote; clears more than the remaining depth
    let before = (t.env.state(), t.balances());
    assert!(total_out < depth);
    assert_program_err(&t.swap(buy(), too_much, 0), PropAmmError::InsufficientDepth);
    assert_eq!((t.env.state(), t.balances()), before);
    // The bid side is independent and untouched.
    assert_eq!(t.env.consumed().1, 0);
    // The next update in a later slot reopens the depth.
    t.env.svm.warp_to_slot(11);
    t.env.update(MID).unwrap();
    assert_eq!(t.env.consumed(), (0, 0));
    t.swap(buy(), 400_000_000, 0).unwrap();
}

#[test]
fn order_splitting_never_beats_one_trade() {
    let n = 10;
    // Totals span several levels (level size 1e9 base atoms, 0.2 quote per base atom).
    for (side, total) in [(buy(), 500_000_000), (sell(), 2_000_000_000)] {
        // BuyBase receives base (index 0); SellBase receives quote (index 1).
        let recv = |t: &Trade, start: (u64, u64, u64, u64)| {
            let now = t.balances();
            if side == buy() {
                now.0 - start.0
            } else {
                now.1 - start.1
            }
        };
        let mut single = setup();
        let start = single.balances();
        single.swap(side, total, 0).unwrap();
        let single_out = recv(&single, start);
        let mut split = setup();
        for _ in 0..n {
            split.swap(side, total / n, 0).unwrap();
        }
        let split_out = recv(&split, start);
        assert!(
            split_out <= single_out,
            "split {split_out} beat single {single_out}"
        );
    }
}

#[test]
fn set_params_invalidates_until_next_update() {
    let mut t = setup();
    let admin = t.env.admin.pubkey();
    let ix = t.env.set_params_ix(&admin, PARAMS);
    t.env.send_admin(ix).unwrap();
    assert_program_err(&t.swap(buy(), BUY, 0), PropAmmError::InsufficientDepth);
    assert_program_err(&t.swap(sell(), SELL, 0), PropAmmError::InsufficientDepth);
    t.env.svm.warp_to_slot(11);
    t.env.update(MID).unwrap();
    t.swap(buy(), BUY, 0).unwrap();
    t.swap(sell(), SELL, 0).unwrap();
}

#[test]
fn underfunded_out_vault_rejects() {
    // Base vault holds less than a 100M-quote buy returns; quote vault holds less than a sell returns.
    let mut t = setup_with(1_000, 1_000);
    let (before, bal) = (t.env.state(), t.balances());
    assert_program_err(
        &t.swap(buy(), BUY, 0),
        PropAmmError::InsufficientVaultBalance,
    );
    assert_program_err(
        &t.swap(sell(), SELL, 0),
        PropAmmError::InsufficientVaultBalance,
    );
    assert_eq!((t.env.state(), t.balances()), (before, bal));
}

#[test]
fn unit_confusion_attempts_fail() {
    let mut t = setup();
    let (before, bal) = (t.env.state(), t.balances());
    let ok = t.accounts();
    let admin_quote = t.env.admin_quote;
    let fake_mint = {
        let admin = t.env.admin.insecure_clone();
        CreateMint::new(&mut t.env.svm, &admin).send().unwrap()
    };
    // Extra trader-owned accounts of the opposite mint, so no account appears twice.
    let (admin, trader) = (t.env.admin.insecure_clone(), t.env.other.insecure_clone());
    let (base_mint, quote_mint) = (t.env.base_mint, t.env.quote_mint);
    let other_quote = token_account(&mut t.env.svm, &admin, &quote_mint, &trader);
    let other_base = token_account(&mut t.env.svm, &admin, &base_mint, &trader);
    // Token account with the right mint but owned by someone else.
    let stranger = Keypair::new();
    let strangers_quote = {
        let admin = t.env.admin.insecure_clone();
        CreateAccount::new(&mut t.env.svm, &admin, &t.env.quote_mint)
            .owner(&stranger.pubkey())
            .send()
            .unwrap()
    };

    let mut cases: Vec<(propamm::accounts::Swap, u32)> = vec![];
    let code = |e: PropAmmError| u32::from(e);
    let mut a = ok;
    a.trader_base = other_quote;
    cases.push((a, CONSTRAINT_TOKEN_MINT));
    let mut a = ok;
    a.trader_quote = other_base;
    cases.push((a, CONSTRAINT_TOKEN_MINT));
    let mut a = ok;
    (a.base_vault, a.quote_vault) = (t.env.quote_vault, t.env.base_vault);
    cases.push((a, code(PropAmmError::InvalidVault)));
    let mut a = ok;
    a.quote_vault = other_quote;
    cases.push((a, code(PropAmmError::InvalidVault)));
    let mut a = ok;
    a.base_mint = t.env.quote_mint;
    cases.push((a, code(PropAmmError::InvalidMint)));
    let mut a = ok;
    a.quote_mint = t.env.base_mint;
    cases.push((a, code(PropAmmError::InvalidMint)));
    let mut a = ok;
    a.base_mint = fake_mint;
    cases.push((a, code(PropAmmError::InvalidMint)));
    let mut a = ok;
    a.trader_quote = admin_quote;
    cases.push((a, CONSTRAINT_TOKEN_OWNER));
    let mut a = ok;
    a.trader_quote = strangers_quote;
    cases.push((a, CONSTRAINT_TOKEN_OWNER));
    let mut a = ok;
    a.token_program = anchor_spl_token_2022_id();
    cases.push((a, INVALID_PROGRAM_ID));
    let mut a = ok;
    a.token_program = Pubkey::new_unique();
    cases.push((a, INVALID_PROGRAM_ID));

    for (i, (accounts, expected)) in cases.iter().enumerate() {
        for side in [buy(), sell()] {
            let res = t.swap_with(accounts, side, BUY, 0);
            assert!(res.is_err(), "case {i} should fail");
            assert_custom_code(&res, *expected);
        }
    }
    assert_eq!((t.env.state(), t.balances()), (before, bal));
}

fn anchor_spl_token_2022_id() -> Pubkey {
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
        .parse()
        .unwrap()
}

#[test]
fn zero_input_and_zero_output() {
    let mut t = setup();
    assert_program_err(&t.swap(buy(), 0, 0), PropAmmError::ZeroInput);
    assert_program_err(&t.swap(sell(), 0, 0), PropAmmError::ZeroInput);
    // One base atom sells for 0.1998 quote atoms: floors to zero.
    assert_program_err(&t.swap(sell(), 1, 0), PropAmmError::ZeroOutput);
    // At 10 quote atoms per base atom, one quote atom buys no base atom.
    t.env.svm.warp_to_slot(11);
    t.env.force(MID * 50).unwrap();
    assert_program_err(&t.swap(buy(), 1, 0), PropAmmError::ZeroOutput);
}

#[test]
fn compute_units() {
    let mut t = setup();
    let buy_cu = t.swap(buy(), BUY, 0).unwrap().compute_units_consumed;
    let sell_cu = t.swap(sell(), SELL, 0).unwrap().compute_units_consumed;
    println!("swap BuyBase {buy_cu} CU, SellBase {sell_cu} CU");
    assert!(buy_cu <= BUY_CU_BUDGET, "BuyBase {buy_cu} CU");
    assert!(sell_cu <= SELL_CU_BUDGET, "SellBase {sell_cu} CU");
}

/// Market with 32 levels of `DEEP_LEVEL` base atoms each, priced and fresh.
fn setup_deep() -> Trade {
    let mut t = setup();
    let admin = t.env.admin.pubkey();
    let params = MarketParams {
        level_size_base_atoms: DEEP_LEVEL,
        num_levels: 32,
        ..PARAMS
    };
    let ix = t.env.set_params_ix(&admin, params);
    t.env.send_admin(ix).unwrap();
    t.env.svm.warp_to_slot(11);
    t.env.update(MID).unwrap();
    assert_eq!(t.env.consumed(), (0, 0));
    t
}

/// Quote atoms that exactly clear every level of the ask side from `consumed == 0`.
fn full_depth_buy_cost(env: &Env) -> u64 {
    let l = env.state().ladder();
    (0..l.num_levels)
        .map(|i| {
            let price = u128::from(l.ask_price(i).unwrap());
            let cost = mul_div_ceil(u128::from(l.level_size_base_atoms), price, PRICE_SCALE);
            u64::try_from(cost.unwrap()).unwrap()
        })
        .sum()
}

#[test]
fn compute_units_worst_case() {
    let mut t = setup_deep();
    let depth = t.env.state().ladder().depth_base_atoms().unwrap();
    let buy_in = full_depth_buy_cost(&t.env);
    let fill = quote(&t.env, Side::BuyBase, buy_in);
    assert_eq!(fill.consumed_after, depth, "buy must walk all 32 levels");
    assert_eq!(quote(&t.env, Side::SellBase, depth).consumed_after, depth);
    let buy_cu = t.swap(buy(), buy_in, 0).unwrap().compute_units_consumed;
    let sell_cu = t.swap(sell(), depth, 0).unwrap().compute_units_consumed;
    assert_eq!(t.env.consumed(), (depth, depth));
    println!("swap worst case (32 levels) BuyBase {buy_cu} CU, SellBase {sell_cu} CU");
    assert!(buy_cu <= BUY_WORST_CU_BUDGET, "BuyBase worst {buy_cu} CU");
    assert!(
        sell_cu <= SELL_WORST_CU_BUDGET,
        "SellBase worst {sell_cu} CU"
    );
}

/// A second market by another admin: `(market, base_vault, quote_vault)`.
fn init_market(env: &mut Env, base: &Pubkey, quote: &Pubkey) -> (Pubkey, Pubkey, Pubkey) {
    let admin = Keypair::new();
    env.svm.airdrop(&admin.pubkey(), 10_000_000_000).unwrap();
    let market = market_pda(&admin.pubkey(), base, quote);
    let (base_vault, quote_vault) = (vault_pda(&market, base), vault_pda(&market, quote));
    let ix = Instruction {
        program_id: propamm::ID,
        accounts: propamm::accounts::Initialize {
            admin: admin.pubkey(),
            base_mint: *base,
            quote_mint: *quote,
            market,
            base_vault,
            quote_vault,
            token_program: TOKEN_ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: ix::Initialize {
            params: init_params(env.updater.pubkey()),
        }
        .data(),
    };
    send(&mut env.svm, ix, &admin).unwrap();
    (market, base_vault, quote_vault)
}

#[test]
fn account_substitution_fails() {
    let mut t = setup();
    let (before, bal) = (t.env.state(), t.balances());
    let ok = t.accounts();
    let admin = t.env.admin.insecure_clone();
    let (base_mint, quote_mint) = (t.env.base_mint, t.env.quote_mint);
    // Market B: same mints, other admin. Market C: its own mints.
    let (_, b_base_vault, b_quote_vault) = init_market(&mut t.env, &base_mint, &quote_mint);
    let c_base = CreateMint::new(&mut t.env.svm, &admin).send().unwrap();
    let c_quote = CreateMint::new(&mut t.env.svm, &admin).send().unwrap();
    init_market(&mut t.env, &c_base, &c_quote);
    // A trader-owned token account whose owner program is Token-2022 (copy of a valid account, re-owned).
    let t22: Pubkey = TOKEN_2022_ID.parse().unwrap();
    let mut acc = t.env.svm.get_account(&t.trader_quote).unwrap();
    acc.owner = t22;
    let t22_account = Pubkey::new_unique();
    t.env.svm.set_account(t22_account, acc).unwrap();

    let mut cases: Vec<(&str, propamm::accounts::Swap, u32)> = vec![];
    let dup = DUPLICATE_MUTABLE_ACCOUNT;
    let mut a = ok;
    a.trader_quote = t.env.quote_vault;
    cases.push(("quote_vault as trader_quote", a, dup));
    let mut a = ok;
    a.trader_base = t.env.base_vault;
    cases.push(("base_vault as trader_base", a, dup));
    let mut a = ok;
    a.trader_quote = b_quote_vault;
    cases.push(("B quote_vault as trader_quote", a, CONSTRAINT_TOKEN_OWNER));
    let mut a = ok;
    a.trader_base = b_base_vault;
    cases.push(("B base_vault as trader_base", a, CONSTRAINT_TOKEN_OWNER));
    let invalid_vault = u32::from(PropAmmError::InvalidVault);
    let mut a = ok;
    a.base_vault = b_base_vault;
    cases.push(("B base_vault in vault slot", a, invalid_vault));
    let mut a = ok;
    a.quote_vault = b_quote_vault;
    cases.push(("B quote_vault in vault slot", a, invalid_vault));
    let invalid_mint = u32::from(PropAmmError::InvalidMint);
    let mut a = ok;
    a.base_mint = c_base;
    cases.push(("C base mint in mint slot", a, invalid_mint));
    let mut a = ok;
    a.quote_mint = c_quote;
    cases.push(("C quote mint in mint slot", a, invalid_mint));
    let mut a = ok;
    a.trader_quote = t22_account;
    cases.push((
        "Token-2022 account as trader_quote",
        a,
        OWNED_BY_WRONG_PROGRAM,
    ));
    let mut a = ok;
    a.trader_base = t22_account;
    cases.push((
        "Token-2022 account as trader_base",
        a,
        OWNED_BY_WRONG_PROGRAM,
    ));

    for (name, accounts, expected) in &cases {
        for side in [buy(), sell()] {
            let res = t.swap_with(accounts, side, BUY, 0);
            assert!(res.is_err(), "{name} should fail");
            assert_custom_code(&res, *expected);
        }
    }
    assert_eq!((t.env.state(), t.balances()), (before, bal));
}

#[test]
fn multi_level_fill_starting_mid_level_matches_offchain() {
    for (side, first, big) in [(buy(), BUY, 500_000_000), (sell(), SELL, 1_500_000_000)] {
        let mut t = setup();
        let pside = if side == buy() {
            Side::BuyBase
        } else {
            Side::SellBase
        };
        t.swap(side, first, 0).unwrap();
        let level = t.env.state().ladder().level_size_base_atoms;
        let start = t.env.state().consumed(pside);
        assert!(!start.is_multiple_of(level), "must start mid-level");
        let fill = quote(&t.env, pside, big);
        assert!(
            fill.consumed_after / level >= start / level + 2,
            "crosses 2+ levels"
        );
        let (tb, tq, bv, qv) = t.balances();
        t.swap(side, big, fill.amount_out).unwrap();
        let expected = if side == buy() {
            (
                tb + fill.amount_out,
                tq - big,
                bv - fill.amount_out,
                qv + big,
            )
        } else {
            (
                tb - big,
                tq + fill.amount_out,
                bv + big,
                qv - fill.amount_out,
            )
        };
        assert_eq!(t.balances(), expected);
        assert_eq!(t.env.state().consumed(pside), fill.consumed_after);
    }
}

#[test]
fn sell_min_out_boundary() {
    let mut t = setup();
    let out = quote(&t.env, Side::SellBase, SELL).amount_out;
    let (before, bal) = (t.env.state(), t.balances());
    assert_program_err(
        &t.swap(sell(), SELL, out + 1),
        PropAmmError::SlippageExceeded,
    );
    assert_eq!((t.env.state(), t.balances()), (before, bal));
    t.swap(sell(), SELL, out).unwrap();
    assert_eq!(t.balances().1, bal.1 + out);
}

#[test]
fn insufficient_trader_funds_rolls_back() {
    for side in [buy(), sell()] {
        let mut t = setup();
        let (admin, trader) = (t.env.admin.insecure_clone(), t.env.other.insecure_clone());
        let (base_mint, quote_mint) = (t.env.base_mint, t.env.quote_mint);
        // Trader holds 1 atom less than it pays in.
        let amount_in = if side == buy() { BUY } else { SELL };
        let mint = if side == buy() { quote_mint } else { base_mint };
        let poor = CreateAccount::new(&mut t.env.svm, &admin, &mint)
            .owner(&trader.pubkey())
            .send()
            .unwrap();
        if side == buy() {
            t.trader_quote = poor;
        } else {
            t.trader_base = poor;
        }
        mint_to(&mut t.env, &mint, &poor, amount_in - 1);
        let (before, bal) = (t.env.state(), t.balances());
        let res = t.swap(side, amount_in, 0);
        assert_custom_code(&res, SPL_INSUFFICIENT_FUNDS);
        assert_eq!((t.env.state(), t.balances()), (before, bal));
        assert_eq!(t.env.consumed(), (0, 0));
    }
}

#[test]
fn out_vault_balance_exactly_amount_out() {
    let buy_out = quote(&setup().env, Side::BuyBase, BUY).amount_out;
    let sell_out = quote(&setup().env, Side::SellBase, SELL).amount_out;
    // Exactly enough passes and empties the out vault.
    let mut t = setup_with(buy_out, FUND);
    t.swap(buy(), BUY, buy_out).unwrap();
    assert_eq!(t.balances().2, 0);
    let mut t = setup_with(FUND, sell_out);
    t.swap(sell(), SELL, sell_out).unwrap();
    assert_eq!(t.balances().3, 0);
    // One atom less fails.
    let mut t = setup_with(buy_out - 1, FUND);
    assert_program_err(
        &t.swap(buy(), BUY, 0),
        PropAmmError::InsufficientVaultBalance,
    );
    let mut t = setup_with(FUND, sell_out - 1);
    assert_program_err(
        &t.swap(sell(), SELL, 0),
        PropAmmError::InsufficientVaultBalance,
    );
}

#[test]
fn exact_depth_fill_then_one_atom_fails_and_other_side_independent() {
    for side in [buy(), sell()] {
        let mut t = setup();
        let depth = t.env.state().ladder().depth_base_atoms().unwrap();
        let amount_in = if side == buy() {
            full_depth_buy_cost(&t.env)
        } else {
            depth
        };
        t.swap(side, amount_in, 0).unwrap();
        let (ask, bid) = t.env.consumed();
        assert_eq!(if side == buy() { ask } else { bid }, depth);
        let (before, bal) = (t.env.state(), t.balances());
        assert_program_err(&t.swap(side, 1, 0), PropAmmError::InsufficientDepth);
        assert_eq!((t.env.state(), t.balances()), (before, bal));
        // The other side still trades.
        if side == buy() {
            t.swap(sell(), SELL, 0).unwrap();
            assert_eq!(t.env.consumed(), (depth, SELL));
        } else {
            t.swap(buy(), BUY, 0).unwrap();
            assert_eq!(t.env.consumed().1, depth);
            assert!(t.env.consumed().0 > 0);
        }
    }
}

#[test]
fn same_slot_round_trip_always_loses() {
    for (quote_amount, base_amount) in [
        (BUY, SELL),
        (300_000_000, 1_500_000_000),
        (700_000_000, 3_000_000_000),
    ] {
        // Buy `amount` quote of base, then sell all of it back.
        let mut t = setup();
        let (tb, tq, _, _) = t.balances();
        t.swap(buy(), quote_amount, 0).unwrap();
        let got = t.balances().0 - tb;
        t.swap(sell(), got, 0).unwrap();
        let (b, q, _, _) = t.balances();
        assert_eq!(b, tb);
        assert!(q < tq, "buy then sell: {q} >= {tq}");

        // Sell `amount` base, then spend all quote received on base.
        let mut t = setup();
        let (tb, tq, _, _) = t.balances();
        t.swap(sell(), base_amount, 0).unwrap();
        let got = t.balances().1 - tq;
        t.swap(buy(), got, 0).unwrap();
        let (b, q, _, _) = t.balances();
        assert_eq!(q, tq);
        assert!(b < tb, "sell then buy: {b} >= {tb}");
    }
}

#[test]
fn swap_event_fields() {
    let mut t = setup();
    t.env.svm.warp_to_slot(20);
    let fill = quote(&t.env, Side::BuyBase, BUY);
    let res = t.swap(buy(), BUY, 0).unwrap();
    let events: Vec<SwapEvent> = res
        .logs
        .iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .map(|b64| STANDARD.decode(b64).unwrap())
        .filter(|d| d.starts_with(SwapEvent::DISCRIMINATOR))
        .map(|d| SwapEvent::deserialize(&mut &d[SwapEvent::DISCRIMINATOR.len()..]).unwrap())
        .collect();
    assert_eq!(events.len(), 1, "logs: {:?}", res.logs);
    let e = &events[0];
    assert_eq!(e.side, SwapSide::BuyBase);
    assert_eq!(e.amount_in, BUY);
    assert_eq!(e.amount_out, fill.amount_out);
    assert_eq!(e.consumed_after, fill.consumed_after);
    assert_eq!(e.mid_price, MID);
    assert_eq!(e.slot, 20);
    assert_eq!(e.market, t.env.market);
    assert_eq!(e.trader, t.env.other.pubkey());
}
