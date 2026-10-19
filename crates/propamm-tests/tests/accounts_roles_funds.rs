//! Market account, roles and funds, against the real program in `LiteSVM`.

mod common;

use common::*;
use litesvm::LiteSVM;
use litesvm_token::spl_token::state::Account as TokenAccountState;
use litesvm_token::{CreateMint, get_spl_account};
use propamm::error::PropAmmError;
use propamm::state::MarketParams;
use propamm_pricing::MarketState;
use solana_keypair::Keypair;
use solana_signer::Signer;

const FORCE_PRICE_CU_BUDGET: u64 = 2_150;
const UPDATE_PRICE_CU_BUDGET: u64 = 2_400;

fn bad_params() -> Vec<MarketParams> {
    vec![
        MarketParams {
            level_size_base_atoms: 0,
            ..PARAMS
        },
        MarketParams {
            num_levels: 0,
            ..PARAMS
        },
        MarketParams {
            num_levels: 33,
            ..PARAMS
        },
        MarketParams {
            half_spread_bps: 10_000,
            ..PARAMS
        },
        // 9_990 + 3 * 4 = 10_002 >= 10_000: deepest level would cross zero.
        MarketParams {
            half_spread_bps: 9_990,
            level_step_bps: 4,
            ..PARAMS
        },
        MarketParams {
            max_age_slots: 0,
            ..PARAMS
        },
        MarketParams {
            max_move_bps: 0,
            ..PARAMS
        },
        MarketParams {
            max_move_bps: 10_000,
            ..PARAMS
        },
        MarketParams {
            max_move_bps: 101,
            ..PARAMS
        },
        MarketParams {
            max_age_slots: 1_001,
            ..PARAMS
        },
        // Depth (32 * u64::MAX / 16) overflows u64.
        MarketParams {
            level_size_base_atoms: u64::MAX / 16,
            num_levels: 32,
            ..PARAMS
        },
    ]
}

fn assert_params_eq(s: &MarketState, p: &MarketParams) {
    assert_eq!(s.level_size_base_atoms, p.level_size_base_atoms);
    assert_eq!(s.num_levels, p.num_levels);
    assert_eq!(s.half_spread_bps, p.half_spread_bps);
    assert_eq!(s.level_step_bps, p.level_step_bps);
    assert_eq!(s.max_age_slots, p.max_age_slots);
    assert_eq!(s.max_move_bps, p.max_move_bps);
}

#[test]
fn initialize_sets_every_field_and_creates_vaults() {
    let env = Env::initialized();
    let s = env.state();
    assert_eq!(s.admin, env.admin.pubkey().to_bytes());
    assert_eq!(s.updater, env.updater.pubkey().to_bytes());
    assert_eq!(s.base_mint, env.base_mint.to_bytes());
    assert_eq!(s.quote_mint, env.quote_mint.to_bytes());
    assert_eq!(s.base_vault, env.base_vault.to_bytes());
    assert_eq!(s.quote_vault, env.quote_vault.to_bytes());
    assert_params_eq(&s, &PARAMS);
    assert_eq!(
        s.bump,
        Pubkey::find_program_address(
            &[
                b"market",
                env.admin.pubkey().as_ref(),
                env.base_mint.as_ref(),
                env.quote_mint.as_ref()
            ],
            &propamm::ID
        )
        .1
    );
    assert_eq!(
        (
            s.mid_price,
            s.last_update_slot,
            s.ask_consumed_base_atoms,
            s.bid_consumed_base_atoms,
            s.paused
        ),
        (0, 0, 0, 0, 0)
    );
    assert_eq!(s.padding, [0; 7]);

    let market_acc = env.svm.get_account(&env.market).unwrap();
    assert_eq!(market_acc.owner, propamm::ID);
    assert_eq!(
        market_acc.data.len(),
        8 + propamm_pricing::MARKET_STATE_SIZE
    );

    for (vault, mint) in [
        (env.base_vault, env.base_mint),
        (env.quote_vault, env.quote_mint),
    ] {
        let v = get_spl_account::<TokenAccountState>(&env.svm, &vault).unwrap();
        assert_eq!(v.owner, env.market);
        assert_eq!(v.mint, mint);
        assert_eq!(v.amount, 0);
    }
}

#[test]
fn initialize_twice_fails_and_keeps_state() {
    let mut env = Env::initialized();
    let before = env.state();
    // A different payer would become admin if re-initialization worked.
    let mut ix = env.initialize_ix(init_params(env.other.pubkey()));
    ix.accounts[0].pubkey = env.other.pubkey();
    let res = env.send_other(ix);
    assert!(res.is_err(), "re-initialize must fail");
    assert_eq!(env.state(), before);
}

#[test]
fn initialize_rejects_same_base_and_quote_mint() {
    let mut env = Env::bare();
    env.quote_mint = env.base_mint;
    env.market = market_pda(&env.admin.pubkey(), &env.base_mint, &env.quote_mint);
    env.base_vault = vault_pda(&env.market, &env.base_mint);
    env.quote_vault = env.base_vault;
    let ix = env.initialize_ix(init_params(env.updater.pubkey()));
    let res = env.send_admin(ix);
    // Both vault PDAs collide, so the second vault `init` fails with the System program's
    // "account already in use" (custom 0).
    assert_custom_code(&res, 0);
    assert!(env.svm.get_account(&env.market).is_none());
}

#[test]
fn initialize_rejects_default_or_admin_updater() {
    for use_admin in [false, true] {
        let mut env = Env::bare();
        let bad = if use_admin {
            env.admin.pubkey()
        } else {
            Pubkey::default()
        };
        let ix = env.initialize_ix(init_params(bad));
        assert_program_err(&env.send_admin(ix), PropAmmError::InvalidUpdater);
        assert!(env.svm.get_account(&env.market).is_none());
    }
}

#[test]
fn initialize_rejects_invalid_params() {
    for bad in bad_params() {
        let mut env = Env::bare();
        let ix = env.initialize_ix(init_params_from(env.updater.pubkey(), bad));
        let res = env.send_admin(ix);
        assert_program_err(&res, PropAmmError::InvalidParams);
        assert!(env.svm.get_account(&env.market).is_none());
    }
}

#[test]
fn deposit_moves_tokens_into_the_matching_vault() {
    let mut env = Env::initialized();
    let admin = env.admin.pubkey();
    let ix = env.deposit_ix(&admin, env.base_mint, env.base_vault, env.admin_base, 300);
    env.send_admin(ix).unwrap();
    let ix = env.deposit_ix(
        &admin,
        env.quote_mint,
        env.quote_vault,
        env.admin_quote,
        700,
    );
    env.send_admin(ix).unwrap();
    assert_eq!(env.balance(&env.base_vault), 300);
    assert_eq!(env.balance(&env.quote_vault), 700);
    assert_eq!(env.balance(&env.admin_base), MINTED - 300);
    assert_eq!(env.balance(&env.admin_quote), MINTED - 700);
}

#[test]
fn withdraw_moves_tokens_back_to_admin() {
    let mut env = Env::initialized();
    let admin = env.admin.pubkey();
    for (mint, vault, token) in [
        (env.base_mint, env.base_vault, env.admin_base),
        (env.quote_mint, env.quote_vault, env.admin_quote),
    ] {
        let ix = env.deposit_ix(&admin, mint, vault, token, 1_000);
        env.send_admin(ix).unwrap();
        let ix = env.withdraw_ix(&admin, mint, vault, token, 400);
        env.send_admin(ix).unwrap();
        assert_eq!(env.balance(&vault), 600);
        assert_eq!(env.balance(&token), MINTED - 600);
    }
}

#[test]
fn withdraw_more_than_vault_holds_fails() {
    let mut env = Env::initialized();
    let admin = env.admin.pubkey();
    env.fund_base(100);
    let ix = env.withdraw_ix(&admin, env.base_mint, env.base_vault, env.admin_base, 101);
    let res = env.send_admin(ix);
    // SPL Token `InsufficientFunds`.
    assert_custom_code(&res, 1);
    assert_eq!(env.balance(&env.base_vault), 100);
}

#[test]
fn deposit_with_wrong_vault_or_mint_fails() {
    let mut env = Env::initialized();
    let admin = env.admin.pubkey();
    // Base mint into the quote vault.
    let ix = env.deposit_ix(&admin, env.base_mint, env.quote_vault, env.admin_base, 10);
    assert_program_err(&env.send_admin(ix), PropAmmError::InvalidVault);
    // A mint that is not part of the market.
    let admin_kp = env.admin.insecure_clone();
    let rogue_mint = CreateMint::new(&mut env.svm, &admin_kp).send().unwrap();
    let rogue_token = token_account(&mut env.svm, &admin_kp, &rogue_mint, &admin_kp);
    let ix = env.deposit_ix(&admin, rogue_mint, env.base_vault, rogue_token, 10);
    assert_program_err(&env.send_admin(ix), PropAmmError::InvalidMint);
    assert_eq!(env.balance(&env.base_vault), 0);
    assert_eq!(env.balance(&env.quote_vault), 0);
}

#[test]
fn non_admin_cannot_move_funds_or_change_config() {
    let mut env = Env::initialized();
    let other = env.other.pubkey();
    // Seed the vault so a withdraw would otherwise succeed.
    env.fund_base(500);
    let before = env.state();

    let ix = env.deposit_ix(&other, env.base_mint, env.base_vault, env.other_base, 10);
    assert_program_err(&env.send_other(ix), PropAmmError::Unauthorized);
    let ix = env.withdraw_ix(&other, env.base_mint, env.base_vault, env.other_base, 10);
    assert_program_err(&env.send_other(ix), PropAmmError::Unauthorized);
    let ix = env.set_params_ix(
        &other,
        MarketParams {
            half_spread_bps: 5,
            ..PARAMS
        },
    );
    assert_program_err(&env.send_other(ix), PropAmmError::Unauthorized);
    let ix = env.set_updater_ix(&other, other);
    assert_program_err(&env.send_other(ix), PropAmmError::Unauthorized);
    let ix = env.set_paused_ix(&other, true);
    assert_program_err(&env.send_other(ix), PropAmmError::Unauthorized);

    assert_eq!(env.state(), before);
    assert_eq!(env.balance(&env.base_vault), 500);
    assert_eq!(env.balance(&env.other_base), MINTED);
}

#[test]
fn updater_cannot_withdraw() {
    let mut env = Env::initialized();
    env.fund_base(500);
    let updater = env.updater.insecure_clone();
    env.svm.airdrop(&updater.pubkey(), 1_000_000_000).unwrap();
    let up_token = token_account(
        &mut env.svm,
        &env.admin.insecure_clone(),
        &env.base_mint,
        &updater,
    );
    let ix = env.withdraw_ix(
        &updater.pubkey(),
        env.base_mint,
        env.base_vault,
        up_token,
        10,
    );
    let res = send(&mut env.svm, ix, &updater);
    assert_program_err(&res, PropAmmError::Unauthorized);
}

#[test]
fn set_params_validates_and_persists() {
    let mut env = Env::initialized();
    let admin = env.admin.pubkey();
    for bad in bad_params() {
        let ix = env.set_params_ix(&admin, bad);
        assert_program_err(&env.send_admin(ix), PropAmmError::InvalidParams);
        assert_params_eq(&env.state(), &PARAMS);
    }
    let new = MarketParams {
        level_size_base_atoms: 5_000_000_000,
        num_levels: 32,
        half_spread_bps: 7,
        level_step_bps: 2,
        max_age_slots: 1_000,
        max_move_bps: 100,
    };
    let ix = env.set_params_ix(&admin, new);
    env.send_admin(ix).unwrap();
    assert_params_eq(&env.state(), &new);
}

#[test]
fn set_params_invalidates_the_current_window() {
    let mut env = Env::initialized();
    let admin = env.admin.pubkey();
    assert_eq!(env.state().ask_consumed_base_atoms, 0);
    assert_eq!(env.state().bid_consumed_base_atoms, 0);
    let ix = env.set_params_ix(&admin, PARAMS);
    env.send_admin(ix).unwrap();
    assert_eq!(env.state().ask_consumed_base_atoms, u64::MAX);
    assert_eq!(env.state().bid_consumed_base_atoms, u64::MAX);
    // A rejected set_params leaves the window untouched.
    let before = env.state();
    let ix = env.set_params_ix(
        &admin,
        MarketParams {
            max_move_bps: 0,
            ..PARAMS
        },
    );
    assert!(env.send_admin(ix).is_err());
    assert_eq!(env.state(), before);
}

#[test]
fn two_admins_can_each_create_a_market_for_the_same_pair() {
    let mut env = Env::initialized();
    let admin2 = Keypair::new();
    env.svm.airdrop(&admin2.pubkey(), 10_000_000_000).unwrap();
    // The first admin is the mint authority, so it pays for and mints into admin2's accounts.
    let (payer, base_mint, quote_mint) =
        (env.admin.insecure_clone(), env.base_mint, env.quote_mint);
    let admin_base = token_account(&mut env.svm, &payer, &base_mint, &admin2);
    let admin_quote = token_account(&mut env.svm, &payer, &quote_mint, &admin2);
    let market2 = market_pda(&admin2.pubkey(), &base_mint, &quote_mint);
    assert_ne!(market2, env.market);
    let mut env2 = Env {
        svm: std::mem::replace(&mut env.svm, LiteSVM::new()),
        admin: admin2.insecure_clone(),
        updater: env.updater.insecure_clone(),
        other: env.other.insecure_clone(),
        base_mint,
        quote_mint,
        market: market2,
        base_vault: vault_pda(&market2, &base_mint),
        quote_vault: vault_pda(&market2, &quote_mint),
        admin_base,
        admin_quote,
        other_base: env.other_base,
    };
    let ix = env2.initialize_ix(init_params(env2.updater.pubkey()));
    env2.send_admin(ix).unwrap();
    assert_eq!(env2.state().admin, admin2.pubkey().to_bytes());
    // Withdraw works for the second market (signer seeds include its admin).
    env2.fund_base(1_000);
    let ix = env2.withdraw_ix(
        &admin2.pubkey(),
        env2.base_mint,
        env2.base_vault,
        env2.admin_base,
        400,
    );
    env2.send_admin(ix).unwrap();
    assert_eq!(env2.balance(&env2.base_vault), 600);
    // The first market is untouched.
    let first: MarketState = {
        let data = env2.svm.get_account(&env2.market).unwrap().data;
        bytemuck::pod_read_unaligned(&data[8..])
    };
    assert_ne!(first.admin, [0u8; 32]);
}

#[test]
fn set_updater_rotates_and_set_paused_toggles() {
    let mut env = Env::initialized();
    let admin = env.admin.pubkey();
    let new_updater = Pubkey::new_unique();
    let ix = env.set_updater_ix(&admin, new_updater);
    env.send_admin(ix).unwrap();
    assert_eq!(env.state().updater, new_updater.to_bytes());

    // Default key and the admin are rejected, and the stored updater stays.
    for bad in [Pubkey::default(), admin] {
        let ix = env.set_updater_ix(&admin, bad);
        assert_program_err(&env.send_admin(ix), PropAmmError::InvalidUpdater);
        assert_eq!(env.state().updater, new_updater.to_bytes());
    }

    for paused in [true, false, true] {
        let ix = env.set_paused_ix(&admin, paused);
        env.send_admin(ix).unwrap();
        assert_eq!(env.state().paused, u8::from(paused));
    }
}

/// Prints compute units per instruction (run with `--nocapture`). Asserts budgets for the
/// price instructions only.
#[test]
fn compute_units_per_instruction() {
    let mut env = Env::bare();
    let admin = env.admin.pubkey();
    let mut rows = vec![];
    let ix = env.initialize_ix(init_params(env.updater.pubkey()));
    rows.push((
        "initialize",
        env.send_admin(ix).unwrap().compute_units_consumed,
    ));
    let ix = env.deposit_ix(&admin, env.base_mint, env.base_vault, env.admin_base, 1_000);
    rows.push((
        "deposit",
        env.send_admin(ix).unwrap().compute_units_consumed,
    ));
    let ix = env.withdraw_ix(&admin, env.base_mint, env.base_vault, env.admin_base, 500);
    rows.push((
        "withdraw",
        env.send_admin(ix).unwrap().compute_units_consumed,
    ));
    let ix = env.set_params_ix(&admin, PARAMS);
    rows.push((
        "set_params",
        env.send_admin(ix).unwrap().compute_units_consumed,
    ));
    let ix = env.set_updater_ix(&admin, env.updater.pubkey());
    rows.push((
        "set_updater",
        env.send_admin(ix).unwrap().compute_units_consumed,
    ));
    let ix = env.set_paused_ix(&admin, true);
    rows.push((
        "set_paused",
        env.send_admin(ix).unwrap().compute_units_consumed,
    ));
    let ix = env.set_paused_ix(&admin, false);
    env.send_admin(ix).unwrap();
    env.svm.warp_to_slot(10);
    let force_cu = env.force(MID).unwrap().compute_units_consumed;
    rows.push(("force_price", force_cu));
    env.svm.warp_to_slot(11);
    let update_cu = env.update(MID + 1).unwrap().compute_units_consumed;
    rows.push(("update_price", update_cu));
    // Measured (LiteSVM, release .so): force_price 1,553 CU, update_price 1,761 CU. Budgets leave
    // ~20% headroom; the Pinocchio port is compared against these numbers.
    assert!(
        force_cu <= FORCE_PRICE_CU_BUDGET,
        "force_price {force_cu} CU"
    );
    assert!(
        update_cu <= UPDATE_PRICE_CU_BUDGET,
        "update_price {update_cu} CU"
    );
    println!("instruction   CU");
    for (name, cu) in rows {
        println!("{name:<12} {cu:>6}");
    }
}
