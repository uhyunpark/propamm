//! CU ladder. Parity of the Pinocchio port (`target/deploy/propamm_pinocchio.so`) and
//! of the Anchor `fast-update` fallback (`target/deploy/fast/propamm.so`) with the Anchor
//! program, plus the CU table (`cargo test -p propamm-tests --test pinocchio cu_ladder -- --nocapture`).
//!
//! Differential harness: both programs run in one `LiteSVM`. Every Pinocchio instruction is the
//! Anchor instruction with the same accounts (program-specific accounts swapped for their
//! "twins": market PDAs, vaults, and token accounts each side moves) and data
//! `[tag] ++ Anchor's Borsh args`. `Dual::both` sends both and asserts the same outcome (same
//! error, or success), then the same market bytes (modulo the vault addresses and bump, which are
//! program-specific PDAs), lamports and token balances on every twin pair.

// `TxResult` (from `common`) holds LiteSVM's large error type.
#![allow(clippy::result_large_err)]

mod common;

use anchor_lang::Discriminator;
use anchor_lang::prelude::AccountMeta;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use common::*;
use litesvm::LiteSVM;
use litesvm_token::{CreateAccount, CreateMint, MintTo, TOKEN_ID};
use propamm::error::PropAmmError;
use propamm::instruction as ix;
use propamm::instructions::SwapSide;
use propamm::state::{Market, MarketParams};
use propamm_pricing::{MARKET_STATE_SIZE, MarketState, PRICE_SCALE, Side, mul_div_ceil};
use solana_instruction_error::InstructionError;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction_error::TransactionError;

const PINO_ID: Pubkey = Pubkey::from_str_const("HTsHg7iAkb5YtrV9in5537j8iijh1VHrk1t2dFMpB287");
/// Fast-path tag (`programs/propamm/src/fast.rs`).
const FAST_TAG: u8 = 0x01;

/// Anchor framework codes the port reproduces.
const INSTRUCTION_FALLBACK_NOT_FOUND: u32 = 101;
const INSTRUCTION_DID_NOT_DESERIALIZE: u32 = 102;
const CONSTRAINT_MUT: u32 = 2000;
const CONSTRAINT_SEEDS: u32 = 2006;
const CONSTRAINT_TOKEN_MINT: u32 = 2014;
const CONSTRAINT_TOKEN_OWNER: u32 = 2015;
const DUPLICATE_MUTABLE_ACCOUNT: u32 = 2040;
const DISCRIMINATOR_NOT_FOUND: u32 = 3001;
const DISCRIMINATOR_MISMATCH: u32 = 3002;
const DID_NOT_DESERIALIZE: u32 = 3003;
const NOT_ENOUGH_KEYS: u32 = 3005;
const OWNED_BY_WRONG_PROGRAM: u32 = 3007;
const INVALID_PROGRAM_ID: u32 = 3008;
const ACCOUNT_NOT_SIGNER: u32 = 3010;
const ACCOUNT_NOT_INITIALIZED: u32 = 3012;
/// SPL Token `InsufficientFunds`; System `AccountAlreadyInUse`.
const SPL_INSUFFICIENT_FUNDS: u32 = 1;
const SYSTEM_ACCOUNT_IN_USE: u32 = 0;
const TOKEN_2022_ID: Pubkey = Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

const FUND: u64 = 5_000_000_000;
const BUY: u64 = 100_000_000;
const SELL: u64 = 500_000_000;
const DEEP_LEVEL: u64 = 10_000_000;
/// 1% of `MID` (`max_move_bps = 100`).
const BAND: u64 = 2_000_000_000;

// Budgets: measured (LiteSVM, release .so, platform-tools v1.57 as `anchor build` uses) + ~20%.
// Measured: update_price 969, force_price 766, deposit 1,811, withdraw 1,874, swap BuyBase 5,083 /
// SellBase 4,574, worst case (32 levels) 21,292 / 16,640; fast path update_price 1,624.
const PINO_UPDATE_CU_BUDGET: u64 = 1_170;
const PINO_FORCE_CU_BUDGET: u64 = 920;
const PINO_DEPOSIT_CU_BUDGET: u64 = 2_180;
const PINO_WITHDRAW_CU_BUDGET: u64 = 2_250;
const PINO_BUY_CU_BUDGET: u64 = 6_100;
const PINO_SELL_CU_BUDGET: u64 = 5_490;
const PINO_BUY_WORST_CU_BUDGET: u64 = 25_560;
const PINO_SELL_WORST_CU_BUDGET: u64 = 19_970;
const FAST_UPDATE_CU_BUDGET: u64 = 1_950;

fn so_path(rel: &str) -> String {
    let path = format!("{}/../../target/deploy/{rel}", env!("CARGO_MANIFEST_DIR"));
    assert!(
        std::path::Path::new(&path).exists(),
        "missing {path}: build it first (see programs/propamm-pinocchio/CLAUDE.md)"
    );
    path
}

fn pino_market_pda(admin: &Pubkey, base: &Pubkey, quote: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[b"market", admin.as_ref(), base.as_ref(), quote.as_ref()],
        &PINO_ID,
    )
}

fn pino_vault_pda(market: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market.as_ref(), mint.as_ref()], &PINO_ID).0
}

/// Pinocchio tag for an Anchor instruction discriminator.
fn pino_tag(disc: &[u8]) -> u8 {
    let table: [&[u8]; 9] = [
        ix::Initialize::DISCRIMINATOR,
        ix::Deposit::DISCRIMINATOR,
        ix::Withdraw::DISCRIMINATOR,
        ix::SetParams::DISCRIMINATOR,
        ix::SetUpdater::DISCRIMINATOR,
        ix::SetPaused::DISCRIMINATOR,
        ix::UpdatePrice::DISCRIMINATOR,
        ix::Swap::DISCRIMINATOR,
        ix::ForcePrice::DISCRIMINATOR,
    ];
    let i = table.iter().position(|d| *d == disc).expect("known ix");
    u8::try_from(i).unwrap()
}

/// The largest mid whose deepest ask (+5 bps with `PARAMS`) still fits `u64`.
fn max_ok_mid() -> u64 {
    u64::try_from(u128::from(u64::MAX) * 10_000 / 10_005).unwrap()
}

/// Copies `src` to a fresh address, optionally changing its owner, then editing its data.
fn copy_account(
    svm: &mut LiteSVM,
    src: &Pubkey,
    owner: Option<Pubkey>,
    edit: &dyn Fn(&mut Vec<u8>),
) -> Pubkey {
    let mut acc = svm.get_account(src).unwrap();
    acc.owner = owner.unwrap_or(acc.owner);
    edit(&mut acc.data);
    let key = Pubkey::new_unique();
    svm.set_account(key, acc).unwrap();
    key
}

fn read_state(svm: &LiteSVM, market: &Pubkey) -> MarketState {
    bytemuck::pod_read_unaligned(&svm.get_account(market).unwrap().data[8..])
}

/// Market bytes without the program-specific PDA fields.
fn normalized(mut s: MarketState) -> MarketState {
    s.base_vault = [0; 32];
    s.quote_vault = [0; 32];
    s.bump = 0;
    s
}

/// `Ok(())` or the transaction error, for comparing outcomes.
fn outcome(res: &TxResult) -> Result<(), TransactionError> {
    res.as_ref().map(|_| ()).map_err(|e| e.err.clone())
}

fn custom(code: u32) -> Result<(), TransactionError> {
    Err(TransactionError::InstructionError(
        0,
        InstructionError::Custom(code),
    ))
}

fn code(e: PropAmmError) -> u32 {
    u32::from(e)
}

/// Both programs side by side; `env` holds the Anchor market.
struct Dual {
    env: Env,
    /// `(Anchor-side key, Pinocchio-side key)`.
    twins: Vec<(Pubkey, Pubkey)>,
    /// The trader (`env.other`)'s quote token account (its base account is `env.other_base`).
    trader_quote: Pubkey,
}

impl Dual {
    /// Programs loaded, mints and twin token accounts funded, no market yet.
    fn bare() -> Self {
        let mut env = Env::bare();
        env.svm
            .add_program_from_file(PINO_ID, so_path("propamm_pinocchio.so"))
            .unwrap();
        let (admin, other) = (env.admin.insecure_clone(), env.other.insecure_clone());
        env.svm
            .airdrop(&env.updater.pubkey(), 1_000_000_000)
            .unwrap();
        let (base_mint, quote_mint) = (env.base_mint, env.quote_mint);
        let p_market = pino_market_pda(&admin.pubkey(), &base_mint, &quote_mint).0;
        let trader_quote = token_account(&mut env.svm, &admin, &quote_mint, &other);
        let mut twins = vec![
            (env.market, p_market),
            (env.base_vault, pino_vault_pda(&p_market, &base_mint)),
            (env.quote_vault, pino_vault_pda(&p_market, &quote_mint)),
        ];
        for (account, mint, owner) in [
            (env.admin_base, base_mint, &admin),
            (env.admin_quote, quote_mint, &admin),
            (env.other_base, base_mint, &other),
            (trader_quote, quote_mint, &other),
        ] {
            twins.push((account, token_account(&mut env.svm, &admin, &mint, owner)));
        }
        Self {
            env,
            twins,
            trader_quote,
        }
    }

    /// `bare` plus `initialize` with `PARAMS` on both.
    fn initialized() -> Self {
        let mut d = Self::bare();
        let ix = d.env.initialize_ix(init_params(d.env.updater.pubkey()));
        d.admin(ix).unwrap();
        d
    }

    /// `initialized`, vaults funded with `FUND`, trader funded, first price `MID` forced at slot 10.
    fn priced() -> Self {
        let mut d = Self::initialized();
        let admin = d.env.admin.pubkey();
        for (mint, account) in [
            (d.env.base_mint, d.env.admin_base),
            (d.env.quote_mint, d.env.admin_quote),
            (d.env.base_mint, d.env.other_base),
            (d.env.quote_mint, d.trader_quote),
        ] {
            d.mint_to_both(&mint, &account, FUND);
        }
        for (mint, vault, token) in [
            (d.env.base_mint, d.env.base_vault, d.env.admin_base),
            (d.env.quote_mint, d.env.quote_vault, d.env.admin_quote),
        ] {
            let ix = d.env.deposit_ix(&admin, mint, vault, token, FUND);
            d.admin(ix).unwrap();
        }
        d.env.svm.warp_to_slot(10);
        let ix = d.env.force_price_ix(&admin, MID);
        d.admin(ix).unwrap();
        d
    }

    fn twin(&self, key: Pubkey) -> Pubkey {
        self.twins
            .iter()
            .find(|(a, _)| *a == key)
            .map_or(key, |(_, p)| *p)
    }

    fn p_market(&self) -> Pubkey {
        self.twin(self.env.market)
    }

    fn mint_to_both(&mut self, mint: &Pubkey, account: &Pubkey, amount: u64) {
        let admin = self.env.admin.insecure_clone();
        for target in [*account, self.twin(*account)] {
            MintTo::new(&mut self.env.svm, &admin, mint, &target, amount)
                .send()
                .unwrap();
        }
    }

    /// The Pinocchio twin of an Anchor instruction.
    fn pino_ix(&self, ix: &Instruction) -> Instruction {
        let mut data = vec![pino_tag(&ix.data[..8])];
        data.extend_from_slice(&ix.data[8..]);
        Instruction {
            program_id: PINO_ID,
            accounts: ix
                .accounts
                .iter()
                .map(|m| AccountMeta {
                    pubkey: self.twin(m.pubkey),
                    ..m.clone()
                })
                .collect(),
            data,
        }
    }

    /// Sends `ix` to both programs and asserts the same outcome and state. Returns
    /// `(anchor, pinocchio)`.
    fn both_results(&mut self, ix: Instruction, payer: &Keypair) -> (TxResult, TxResult) {
        let p_ix = self.pino_ix(&ix);
        let a = send(&mut self.env.svm, ix, payer);
        let p = send(&mut self.env.svm, p_ix, payer);
        let logs = |r: &TxResult| match r {
            Ok(m) => m.logs.clone(),
            Err(e) => e.meta.logs.clone(),
        };
        assert_eq!(
            outcome(&a),
            outcome(&p),
            "outcome differs\nanchor: {:?}\npinocchio: {:?}",
            logs(&a),
            logs(&p)
        );
        self.assert_twins_match();
        (a, p)
    }

    /// [`Self::both_results`], returning the Pinocchio result.
    fn both(&mut self, ix: Instruction, payer: &Keypair) -> TxResult {
        self.both_results(ix, payer).1
    }

    fn admin(&mut self, ix: Instruction) -> TxResult {
        let k = self.env.admin.insecure_clone();
        self.both(ix, &k)
    }

    fn updater(&mut self, ix: Instruction) -> TxResult {
        let k = self.env.updater.insecure_clone();
        self.both(ix, &k)
    }

    fn other(&mut self, ix: Instruction) -> TxResult {
        let k = self.env.other.insecure_clone();
        self.both(ix, &k)
    }

    fn assert_twins_match(&self) {
        for (a, p) in &self.twins {
            let (aa, pa) = (self.env.svm.get_account(a), self.env.svm.get_account(p));
            let (aa, pa) = match (aa, pa) {
                (None, None) => continue,
                (Some(aa), Some(pa)) => (aa, pa),
                _ => panic!("only one of {a} / {p} exists"),
            };
            assert_eq!(aa.lamports, pa.lamports, "lamports {a} / {p}");
            assert_eq!(aa.data.len(), pa.data.len(), "data len {a} / {p}");
            if aa.owner == propamm::ID && aa.data.len() == 8 + MARKET_STATE_SIZE {
                assert_eq!(pa.owner, PINO_ID);
                assert_eq!(aa.data[..8], pa.data[..8], "discriminator");
                let (sa, sp): (MarketState, MarketState) = (
                    bytemuck::pod_read_unaligned(&aa.data[8..]),
                    bytemuck::pod_read_unaligned(&pa.data[8..]),
                );
                assert_eq!(normalized(sa), normalized(sp), "market {a} / {p}");
            } else if aa.owner == propamm::ID {
                assert_eq!(pa.owner, PINO_ID);
                assert_eq!(aa.data[..8], pa.data[..8], "discriminator");
            } else {
                assert_eq!(aa.owner, pa.owner, "owner {a} / {p}");
                if aa.owner == TOKEN_ID {
                    // mint + amount (the authority of a vault is its own program's market).
                    assert_eq!(aa.data[..32], pa.data[..32], "token mint {a} / {p}");
                    assert_eq!(aa.data[64..72], pa.data[64..72], "token amount {a} / {p}");
                }
            }
        }
    }

    fn state(&self) -> MarketState {
        self.env.state()
    }

    /// Applies `f` to both markets (only swap moves `consumed`, so tests set it directly).
    fn edit_state(&mut self, f: impl Fn(&mut MarketState)) {
        for market in [self.env.market, self.p_market()] {
            let mut acc = self.env.svm.get_account(&market).unwrap();
            let mut state = read_state(&self.env.svm, &market);
            f(&mut state);
            acc.data[8..].copy_from_slice(bytemuck::bytes_of(&state));
            self.env.svm.set_account(market, acc).unwrap();
        }
    }

    fn p_state(&self) -> MarketState {
        read_state(&self.env.svm, &self.p_market())
    }

    /// `(ask, bid)` consumed on the Pinocchio market.
    fn p_consumed(&self) -> (u64, u64) {
        let s = self.p_state();
        (s.ask_consumed_base_atoms, s.bid_consumed_base_atoms)
    }

    fn swap_accounts(&self) -> propamm::accounts::Swap {
        let e = &self.env;
        propamm::accounts::Swap {
            trader: e.other.pubkey(),
            market: e.market,
            base_mint: e.base_mint,
            quote_mint: e.quote_mint,
            base_vault: e.base_vault,
            quote_vault: e.quote_vault,
            trader_base: e.other_base,
            trader_quote: self.trader_quote,
            token_program: TOKEN_ID,
        }
    }

    fn swap_ix_with(
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

    fn swap_ix(&self, side: SwapSide, amount_in: u64, min_out: u64) -> Instruction {
        Self::swap_ix_with(&self.swap_accounts(), side, amount_in, min_out)
    }

    fn swap(&mut self, side: SwapSide, amount_in: u64, min_out: u64) -> TxResult {
        let ix = self.swap_ix(side, amount_in, min_out);
        self.other(ix)
    }

    fn update(&mut self, mid: u64) -> TxResult {
        let ix = self.env.update_price_ix(&self.env.updater.pubkey(), mid);
        self.updater(ix)
    }

    fn force(&mut self, mid: u64) -> TxResult {
        let ix = self.env.force_price_ix(&self.env.admin.pubkey(), mid);
        self.admin(ix)
    }

    /// Off-chain quote on the (shared) market state.
    fn quote(&self, side: Side, amount_in: u64) -> propamm_pricing::Fill {
        let s = self.state();
        s.ladder()
            .quote_exact_in(side, s.consumed(side), amount_in)
            .unwrap()
    }
}

#[test]
fn initialize_matches_anchor() {
    let d = Dual::initialized();
    let (p_market, bump) =
        pino_market_pda(&d.env.admin.pubkey(), &d.env.base_mint, &d.env.quote_mint);
    let s = d.p_state();
    assert_eq!(s.admin, d.env.admin.pubkey().to_bytes());
    assert_eq!(s.updater, d.env.updater.pubkey().to_bytes());
    assert_eq!(s.base_mint, d.env.base_mint.to_bytes());
    assert_eq!(s.quote_mint, d.env.quote_mint.to_bytes());
    assert_eq!(
        s.base_vault,
        pino_vault_pda(&p_market, &d.env.base_mint).to_bytes()
    );
    assert_eq!(
        s.quote_vault,
        pino_vault_pda(&p_market, &d.env.quote_mint).to_bytes()
    );
    assert_eq!(s.bump, bump);
    let acc = d.env.svm.get_account(&p_market).unwrap();
    assert_eq!(acc.owner, PINO_ID);
    assert_eq!(acc.data[..8], *Market::DISCRIMINATOR);
}

fn bad_params() -> Vec<MarketParams> {
    [
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
            half_spread_bps: 9_990,
            level_step_bps: 4,
            ..PARAMS
        },
        MarketParams {
            max_age_slots: 0,
            ..PARAMS
        },
        MarketParams {
            max_age_slots: 1_001,
            ..PARAMS
        },
        MarketParams {
            max_move_bps: 0,
            ..PARAMS
        },
        MarketParams {
            max_move_bps: 101,
            ..PARAMS
        },
        MarketParams {
            level_size_base_atoms: u64::MAX / 16,
            num_levels: 32,
            ..PARAMS
        },
    ]
    .to_vec()
}

#[test]
fn initialize_failures_match() {
    let invalid_params = custom(code(PropAmmError::InvalidParams));
    for bad in bad_params() {
        let mut d = Dual::bare();
        let ix = d
            .env
            .initialize_ix(init_params_from(d.env.updater.pubkey(), bad));
        assert_eq!(outcome(&d.admin(ix)), invalid_params);
    }
    for use_admin in [false, true] {
        let mut d = Dual::bare();
        let updater = if use_admin {
            d.env.admin.pubkey()
        } else {
            Pubkey::default()
        };
        let ix = d.env.initialize_ix(init_params(updater));
        assert_eq!(
            outcome(&d.admin(ix)),
            custom(code(PropAmmError::InvalidUpdater))
        );
    }

    let mut d = Dual::bare();
    let good = d.env.initialize_ix(init_params(d.env.updater.pubkey()));
    let mut cases: Vec<(&str, Instruction, Result<(), TransactionError>)> = vec![];
    let mut ix = good.clone();
    ix.accounts[3].pubkey = Pubkey::new_unique();
    cases.push(("market not the PDA", ix, custom(CONSTRAINT_SEEDS)));
    let mut ix = good.clone();
    ix.accounts[4].pubkey = Pubkey::new_unique();
    cases.push(("base vault not the PDA", ix, custom(CONSTRAINT_SEEDS)));
    let mut ix = good.clone();
    ix.accounts[6].pubkey = TOKEN_2022_ID;
    cases.push(("Token-2022 program", ix, custom(INVALID_PROGRAM_ID)));
    let mut ix = good.clone();
    ix.accounts[7].pubkey = TOKEN_ID;
    cases.push(("wrong system program", ix, custom(INVALID_PROGRAM_ID)));
    let mut ix = good.clone();
    ix.accounts[1].pubkey = d.env.admin_base;
    cases.push((
        "token account as base mint",
        ix,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidAccountData,
        )),
    ));
    let mut ix = good.clone();
    ix.accounts[2].pubkey = Pubkey::new_unique();
    cases.push(("missing quote mint", ix, custom(ACCOUNT_NOT_INITIALIZED)));
    let mut ix = good.clone();
    ix.accounts.truncate(7);
    cases.push(("no system program", ix, custom(NOT_ENOUGH_KEYS)));
    let mut ix = good.clone();
    ix.data.truncate(8 + 40);
    cases.push((
        "truncated args",
        ix,
        custom(INSTRUCTION_DID_NOT_DESERIALIZE),
    ));
    for (name, ix, expected) in cases {
        assert_eq!(outcome(&d.admin(ix)), expected, "{name}");
    }
    // A non-signer admin (fee paid by someone else).
    let mut ix = good.clone();
    ix.accounts[0].is_signer = false;
    assert_eq!(outcome(&d.other(ix)), custom(ACCOUNT_NOT_SIGNER));

    // Works once; re-initializing fails in the System program's `allocate` (account in use), and
    // another payer does not match the market seeds.
    d.admin(good.clone()).unwrap();
    let before = d.p_state();
    assert_eq!(outcome(&d.admin(good)), custom(SYSTEM_ACCOUNT_IN_USE));
    let mut ix = d.env.initialize_ix(init_params(d.env.admin.pubkey()));
    ix.accounts[0].pubkey = d.env.other.pubkey();
    assert_eq!(outcome(&d.other(ix)), custom(CONSTRAINT_SEEDS));
    assert_eq!(d.p_state(), before);

    // Equal mints: the two vault PDAs collide and the second `init` fails in the System program.
    let mut d = Dual::bare();
    let base = d.env.base_mint;
    let (a_market, p_market) = (
        market_pda(&d.env.admin.pubkey(), &base, &base),
        pino_market_pda(&d.env.admin.pubkey(), &base, &base).0,
    );
    d.twins = vec![
        (a_market, p_market),
        (
            vault_pda(&a_market, &base),
            pino_vault_pda(&p_market, &base),
        ),
    ];
    d.env.quote_mint = base;
    d.env.market = a_market;
    d.env.base_vault = vault_pda(&a_market, &base);
    d.env.quote_vault = d.env.base_vault;
    let ix = d.env.initialize_ix(init_params(d.env.updater.pubkey()));
    assert_eq!(outcome(&d.admin(ix)), custom(SYSTEM_ACCOUNT_IN_USE));
}

/// Pre-funded market and vault PDAs (anyone can send lamports to an address) still initialize:
/// both programs top up, `allocate` and `assign` like Anchor's `init`.
#[test]
fn initialize_with_prefunded_pdas_matches() {
    let mut d = Dual::bare();
    let pdas: Vec<Pubkey> = d.twins[..3].iter().flat_map(|(a, p)| [*a, *p]).collect();
    for pda in pdas {
        d.env.svm.airdrop(&pda, 1_000_000).unwrap();
    }
    let ix = d.env.initialize_ix(init_params(d.env.updater.pubkey()));
    d.admin(ix).unwrap();
    assert_eq!(normalized(d.p_state()), normalized(d.state()));
}

#[test]
fn admin_instructions_reject_everyone_else() {
    let mut d = Dual::priced();
    let (admin, other, updater) = (
        d.env.admin.pubkey(),
        d.env.other.pubkey(),
        d.env.updater.pubkey(),
    );
    let e = &d.env;
    let ixs = |signer: &Pubkey| {
        vec![
            e.set_params_ix(signer, PARAMS),
            e.set_updater_ix(signer, Pubkey::new_unique()),
            e.set_paused_ix(signer, true),
            e.force_price_ix(signer, MID),
            e.deposit_ix(signer, e.base_mint, e.base_vault, e.other_base, 1),
            e.withdraw_ix(signer, e.base_mint, e.base_vault, e.other_base, 1),
        ]
    };
    let (by_other, by_updater, by_admin) = (ixs(&other), ixs(&updater), ixs(&admin));
    let unauthorized = custom(code(PropAmmError::Unauthorized));
    for ix in by_other {
        assert_eq!(outcome(&d.other(ix)), unauthorized);
    }
    for ix in by_updater {
        assert_eq!(outcome(&d.updater(ix)), unauthorized);
    }
    // Admin key present but not signing.
    for mut ix in by_admin.clone() {
        ix.accounts[0].is_signer = false;
        assert_eq!(outcome(&d.other(ix)), custom(ACCOUNT_NOT_SIGNER));
    }
    // AdminOnly needs a writable market (Funds does not).
    for mut ix in by_admin.clone().into_iter().take(4) {
        ix.accounts[1].is_writable = false;
        assert_eq!(outcome(&d.admin(ix)), custom(CONSTRAINT_MUT));
    }
    for mut ix in by_admin {
        ix.accounts.truncate(1);
        assert_eq!(outcome(&d.admin(ix)), custom(NOT_ENOUGH_KEYS));
    }
}

/// Market substitutes: owned by another program, wrong discriminator, wrong size.
#[test]
fn market_account_checks_match() {
    let mut d = Dual::priced();
    let admin = d.env.admin.insecure_clone();
    let (a_market, p_market) = (d.env.market, d.p_market());
    // Each program's market handed to the other program: same bytes, wrong owner.
    let ix = d.env.set_paused_ix(&admin.pubkey(), true);
    let mut a_ix = ix.clone();
    a_ix.accounts[1].pubkey = p_market;
    assert_eq!(
        outcome(&send(&mut d.env.svm, a_ix, &admin)),
        custom(OWNED_BY_WRONG_PROGRAM)
    );
    let mut p_ix = d.pino_ix(&ix);
    p_ix.accounts[1].pubkey = a_market;
    assert_eq!(
        outcome(&send(&mut d.env.svm, p_ix, &admin)),
        custom(OWNED_BY_WRONG_PROGRAM)
    );

    // Copies owned by the right program with a corrupted discriminator or size.
    let fake = |d: &mut Dual, edit: &dyn Fn(&mut Vec<u8>)| {
        let pair = (
            copy_account(&mut d.env.svm, &a_market, None, edit),
            copy_account(&mut d.env.svm, &p_market, None, edit),
        );
        d.twins.push(pair);
        pair.0
    };
    let bad_disc = fake(&mut d, &|data| data[0] ^= 1);
    let short = fake(&mut d, &|data| data.truncate(100));
    for (market, expected) in [
        (bad_disc, DISCRIMINATOR_MISMATCH),
        (short, DID_NOT_DESERIALIZE),
    ] {
        let mut ix = d.env.set_paused_ix(&admin.pubkey(), true);
        ix.accounts[1].pubkey = market;
        assert_eq!(outcome(&d.admin(ix)), custom(expected));
        let mut ix = d.env.update_price_ix(&d.env.updater.pubkey(), MID);
        ix.accounts[1].pubkey = market;
        assert_eq!(outcome(&d.updater(ix)), custom(expected));
    }
}

#[test]
fn deposit_withdraw_match() {
    let mut d = Dual::initialized();
    let admin = d.env.admin.pubkey();
    let e = &d.env;
    let (bm, qm, bv, qv, ab, aq) = (
        e.base_mint,
        e.quote_mint,
        e.base_vault,
        e.quote_vault,
        e.admin_base,
        e.admin_quote,
    );
    d.admin(d.env.deposit_ix(&admin, bm, bv, ab, 300)).unwrap();
    d.admin(d.env.deposit_ix(&admin, qm, qv, aq, 700)).unwrap();
    d.admin(d.env.withdraw_ix(&admin, bm, bv, ab, 100)).unwrap();
    let p_vault = d.twin(bv);
    assert_eq!(d.env.balance(&p_vault), 200);
    assert_eq!(d.env.balance(&d.twin(ab)), MINTED - 200);

    let other_quote = {
        let k = d.env.admin.insecure_clone();
        let o = d.env.other.insecure_clone();
        token_account(&mut d.env.svm, &k, &qm, &o)
    };
    let fake_mint = {
        let k = d.env.admin.insecure_clone();
        CreateMint::new(&mut d.env.svm, &k).send().unwrap()
    };
    let cases: Vec<(&str, Instruction, Result<(), TransactionError>)> = vec![
        (
            "quote vault for base mint",
            d.env.deposit_ix(&admin, bm, qv, ab, 1),
            custom(code(PropAmmError::InvalidVault)),
        ),
        (
            "unknown mint",
            d.env.deposit_ix(&admin, fake_mint, bv, ab, 1),
            custom(code(PropAmmError::InvalidMint)),
        ),
        (
            "admin token of the other mint",
            d.env.deposit_ix(&admin, bm, bv, aq, 1),
            custom(CONSTRAINT_TOKEN_MINT),
        ),
        (
            "token account owned by someone else",
            d.env.withdraw_ix(&admin, qm, qv, other_quote, 1),
            custom(CONSTRAINT_TOKEN_OWNER),
        ),
        (
            "vault as admin token",
            d.env.withdraw_ix(&admin, bm, bv, bv, 1),
            custom(DUPLICATE_MUTABLE_ACCOUNT),
        ),
        (
            "more than the vault holds",
            d.env.withdraw_ix(&admin, bm, bv, ab, 201),
            custom(SPL_INSUFFICIENT_FUNDS),
        ),
    ];
    for (name, ix, expected) in cases {
        assert_eq!(outcome(&d.admin(ix)), expected, "{name}");
    }
    let mut ix = d.env.deposit_ix(&admin, bm, bv, ab, 1);
    ix.accounts[5].pubkey = TOKEN_2022_ID;
    assert_eq!(outcome(&d.admin(ix)), custom(INVALID_PROGRAM_ID));
    let mut ix = d.env.deposit_ix(&admin, bm, bv, ab, 1);
    ix.accounts[3].is_writable = false;
    assert_eq!(outcome(&d.admin(ix)), custom(CONSTRAINT_MUT));
}

#[test]
fn config_instructions_match() {
    let mut d = Dual::priced();
    let admin = d.env.admin.pubkey();
    for bad in bad_params() {
        let ix = d.env.set_params_ix(&admin, bad);
        assert_eq!(
            outcome(&d.admin(ix)),
            custom(code(PropAmmError::InvalidParams))
        );
    }
    let new = MarketParams {
        level_size_base_atoms: 7,
        num_levels: 3,
        half_spread_bps: 5,
        level_step_bps: 2,
        max_age_slots: 20,
        max_move_bps: 50,
    };
    d.admin(d.env.set_params_ix(&admin, new)).unwrap();
    assert_eq!(d.p_consumed(), (u64::MAX, u64::MAX));
    let new_updater = Pubkey::new_unique();
    d.admin(d.env.set_updater_ix(&admin, new_updater)).unwrap();
    assert_eq!(d.p_state().updater, new_updater.to_bytes());
    for bad in [Pubkey::default(), admin] {
        let ix = d.env.set_updater_ix(&admin, bad);
        assert_eq!(
            outcome(&d.admin(ix)),
            custom(code(PropAmmError::InvalidUpdater))
        );
    }
    for paused in [true, false, true] {
        d.admin(d.env.set_paused_ix(&admin, paused)).unwrap();
        assert_eq!(d.p_state().paused, u8::from(paused));
    }
    // Borsh decoding: a bool byte other than 0/1, and missing bytes.
    let mut ix = d.env.set_paused_ix(&admin, true);
    ix.data[8] = 2;
    assert_eq!(
        outcome(&d.admin(ix)),
        custom(INSTRUCTION_DID_NOT_DESERIALIZE)
    );
    let mut ix = d.env.set_updater_ix(&admin, new_updater);
    ix.data.truncate(8 + 31);
    assert_eq!(
        outcome(&d.admin(ix)),
        custom(INSTRUCTION_DID_NOT_DESERIALIZE)
    );
}

#[test]
fn update_price_semantics_match() {
    // Before the first force_price.
    let mut d = Dual::initialized();
    d.env.svm.warp_to_slot(5);
    assert_eq!(
        outcome(&d.update(MID)),
        custom(code(PropAmmError::NotInitializedPrice))
    );

    let mut d = Dual::priced();
    let (admin, other) = (d.env.admin.insecure_clone(), d.env.other.insecure_clone());
    let unauthorized = custom(code(PropAmmError::Unauthorized));
    d.env.svm.warp_to_slot(11);
    for signer in [&admin, &other] {
        let ix = d.env.update_price_ix(&signer.pubkey(), MID);
        assert_eq!(outcome(&d.both(ix, signer)), unauthorized);
    }
    let mut ix = d.env.update_price_ix(&d.env.updater.pubkey(), MID);
    ix.accounts[0].is_signer = false;
    assert_eq!(outcome(&d.other(ix)), custom(ACCOUNT_NOT_SIGNER));
    // Same slot as the force_price.
    d.env.svm.warp_to_slot(10);
    assert_eq!(
        outcome(&d.update(MID)),
        custom(code(PropAmmError::AlreadyUpdatedThisSlot))
    );
    // Move limit, inclusive on both sides; each success starts a window.
    d.env.svm.warp_to_slot(11);
    assert_eq!(
        outcome(&d.update(MID + BAND + 1)),
        custom(code(PropAmmError::PriceMoveTooLarge))
    );
    d.update(MID + BAND).unwrap();
    assert_eq!(
        outcome(&d.update(MID)),
        custom(code(PropAmmError::AlreadyUpdatedThisSlot))
    );
    d.edit_state(|s| s.ask_consumed_base_atoms = 5);
    d.env.svm.warp_to_slot(12);
    let low = (MID + BAND) - (MID + BAND) / 100;
    assert_eq!(
        outcome(&d.update(low - 1)),
        custom(code(PropAmmError::PriceMoveTooLarge))
    );
    d.update(low).unwrap();
    let s = d.p_state();
    assert_eq!(
        (
            s.mid_price,
            s.window_start_mid,
            s.last_update_slot,
            s.ask_consumed_base_atoms
        ),
        (low, MID + BAND, 12, 0)
    );
    // Earlier slot.
    d.env.svm.warp_to_slot(11);
    assert_eq!(
        outcome(&d.update(low)),
        custom(code(PropAmmError::SlotRegression))
    );
    // Ladder validation with the new mid: the largest mid whose deepest ask (+5 bps) fits u64,
    // then one more (inside the move band, but the ask overflows).
    let max_ok = max_ok_mid();
    d.env.svm.warp_to_slot(13);
    d.force(max_ok).unwrap();
    d.env.svm.warp_to_slot(14);
    assert_eq!(
        outcome(&d.update(max_ok + 1)),
        custom(code(PropAmmError::InvalidParams))
    );
    // Pause blocks update_price but not force_price.
    d.env.svm.warp_to_slot(15);
    d.admin(d.env.set_paused_ix(&admin.pubkey(), true)).unwrap();
    assert_eq!(
        outcome(&d.update(max_ok)),
        custom(code(PropAmmError::Paused))
    );
    d.force(MID).unwrap();
    // Read-only market, missing market.
    d.env.svm.warp_to_slot(16);
    d.admin(d.env.set_paused_ix(&admin.pubkey(), false))
        .unwrap();
    let mut ix = d.env.update_price_ix(&d.env.updater.pubkey(), MID);
    ix.accounts[1].is_writable = false;
    assert_eq!(outcome(&d.updater(ix)), custom(CONSTRAINT_MUT));
    let mut ix = d.env.update_price_ix(&d.env.updater.pubkey(), MID);
    ix.accounts.truncate(1);
    assert_eq!(outcome(&d.updater(ix)), custom(NOT_ENOUGH_KEYS));
    d.update(MID).unwrap();
}

#[test]
fn force_price_matches() {
    let mut d = Dual::initialized();
    d.env.svm.warp_to_slot(10);
    d.force(MID).unwrap();
    d.env.svm.warp_to_slot(11);
    d.force(MID * 10).unwrap();
    let s = d.p_state();
    assert_eq!(
        (s.mid_price, s.window_start_mid, s.last_update_slot),
        (MID * 10, MID * 10, 11)
    );
    for mid in [0, 1, u64::MAX] {
        assert_eq!(
            outcome(&d.force(mid)),
            custom(code(PropAmmError::InvalidParams))
        );
    }
}

/// Executes a swap on both and checks the output equals the off-chain quote on each side.
fn swap_and_check(d: &mut Dual, side: SwapSide, amount_in: u64) -> u64 {
    let pside = if side == SwapSide::BuyBase {
        Side::BuyBase
    } else {
        Side::SellBase
    };
    let fill = d.quote(pside, amount_in);
    let out_account = if side == SwapSide::BuyBase {
        d.env.other_base
    } else {
        d.trader_quote
    };
    let p_out = d.twin(out_account);
    let before = (d.env.balance(&out_account), d.env.balance(&p_out));
    d.swap(side, amount_in, fill.amount_out).unwrap();
    assert_eq!(d.env.balance(&out_account) - before.0, fill.amount_out);
    assert_eq!(d.env.balance(&p_out) - before.1, fill.amount_out);
    assert_eq!(d.p_state().consumed(pside), fill.consumed_after);
    fill.amount_out
}

#[test]
fn swap_happy_paths_match() {
    let mut d = Dual::priced();
    d.env.svm.warp_to_slot(20);
    for (side, amount) in [
        (SwapSide::BuyBase, BUY),
        (SwapSide::SellBase, SELL),
        (SwapSide::BuyBase, 500_000_000),
        (SwapSide::SellBase, 1_500_000_000),
    ] {
        swap_and_check(&mut d, side, amount);
    }
    // Swap never touches the window fields.
    let s = d.p_state();
    assert_eq!(
        (s.mid_price, s.window_start_mid, s.last_update_slot),
        (MID, MID, 10)
    );
}

/// 32 levels of `DEEP_LEVEL`, priced and fresh at slot 11.
fn deep() -> Dual {
    let mut d = Dual::priced();
    let params = MarketParams {
        level_size_base_atoms: DEEP_LEVEL,
        num_levels: 32,
        ..PARAMS
    };
    d.admin(d.env.set_params_ix(&d.env.admin.pubkey(), params))
        .unwrap();
    d.env.svm.warp_to_slot(11);
    d.update(MID).unwrap();
    d
}

/// Quote atoms that clear every ask level from `consumed == 0`.
fn full_depth_buy_cost(state: &MarketState) -> u64 {
    let l = state.ladder();
    (0..l.num_levels)
        .map(|i| {
            let price = u128::from(l.ask_price(i).unwrap());
            let cost = mul_div_ceil(u128::from(l.level_size_base_atoms), price, PRICE_SCALE);
            u64::try_from(cost.unwrap()).unwrap()
        })
        .sum()
}

#[test]
fn swap_worst_case_matches() {
    let mut d = deep();
    let depth = d.state().ladder().depth_base_atoms().unwrap();
    let buy_in = full_depth_buy_cost(&d.state());
    swap_and_check(&mut d, SwapSide::BuyBase, buy_in);
    swap_and_check(&mut d, SwapSide::SellBase, depth);
    assert_eq!(d.p_consumed(), (depth, depth));
    assert_eq!(
        outcome(&d.swap(SwapSide::BuyBase, 1, 0)),
        custom(code(PropAmmError::InsufficientDepth))
    );
}

#[test]
fn swap_failures_match() {
    let mut d = Dual::priced();
    let admin = d.env.admin.pubkey();
    let buy = SwapSide::BuyBase;
    let sell = SwapSide::SellBase;
    let out = d.quote(Side::BuyBase, BUY).amount_out;
    assert_eq!(
        outcome(&d.swap(buy, BUY, out + 1)),
        custom(code(PropAmmError::SlippageExceeded))
    );
    assert_eq!(
        outcome(&d.swap(buy, 0, 0)),
        custom(code(PropAmmError::ZeroInput))
    );
    assert_eq!(
        outcome(&d.swap(sell, 1, 0)),
        custom(code(PropAmmError::ZeroOutput))
    );
    assert_eq!(
        outcome(&d.swap(buy, 800_000_000_000, 0)),
        custom(code(PropAmmError::InsufficientDepth))
    );
    // Paused, then stale.
    d.admin(d.env.set_paused_ix(&admin, true)).unwrap();
    assert_eq!(
        outcome(&d.swap(buy, BUY, 0)),
        custom(code(PropAmmError::Paused))
    );
    d.admin(d.env.set_paused_ix(&admin, false)).unwrap();
    let max_age = d.state().max_age_slots;
    d.env.svm.warp_to_slot(10 + max_age);
    d.swap(buy, BUY, 0).unwrap();
    d.env.svm.warp_to_slot(11 + max_age);
    for side in [buy, sell] {
        assert_eq!(
            outcome(&d.swap(side, BUY, 0)),
            custom(code(PropAmmError::StalePrice))
        );
    }
    d.update(MID).unwrap();
    // set_params invalidates the window until the next update.
    d.admin(d.env.set_params_ix(&admin, PARAMS)).unwrap();
    assert_eq!(
        outcome(&d.swap(sell, SELL, 0)),
        custom(code(PropAmmError::InsufficientDepth))
    );
    // Unpriced market.
    let mut u = Dual::initialized();
    assert_eq!(
        outcome(&u.swap(buy, BUY, 0)),
        custom(code(PropAmmError::NotInitializedPrice))
    );
    // Invalid side byte.
    let mut ix = d.swap_ix(buy, BUY, 0);
    ix.data[8] = 2;
    assert_eq!(
        outcome(&d.other(ix)),
        custom(INSTRUCTION_DID_NOT_DESERIALIZE)
    );
}

#[test]
fn swap_underfunded_vault_and_trader_match() {
    let mut d = Dual::priced();
    let admin = d.env.admin.pubkey();
    let (bm, qm, bv, qv, ab, aq) = (
        d.env.base_mint,
        d.env.quote_mint,
        d.env.base_vault,
        d.env.quote_vault,
        d.env.admin_base,
        d.env.admin_quote,
    );
    d.admin(d.env.withdraw_ix(&admin, bm, bv, ab, FUND - 1_000))
        .unwrap();
    d.admin(d.env.withdraw_ix(&admin, qm, qv, aq, FUND - 1_000))
        .unwrap();
    for side in [SwapSide::BuyBase, SwapSide::SellBase] {
        assert_eq!(
            outcome(&d.swap(side, SELL, 0)),
            custom(code(PropAmmError::InsufficientVaultBalance))
        );
    }
    // Trader short of the input amount: SPL Token fails the transfer, state rolls back.
    let mut d = Dual::priced();
    let (k, o) = (d.env.admin.insecure_clone(), d.env.other.insecure_clone());
    let qm = d.env.quote_mint;
    let [poor, p_poor] = [(); 2].map(|()| {
        CreateAccount::new(&mut d.env.svm, &k, &qm)
            .owner(&o.pubkey())
            .send()
            .unwrap()
    });
    d.twins.push((poor, p_poor));
    d.mint_to_both(&qm, &poor, BUY - 1);
    let mut a = d.swap_accounts();
    a.trader_quote = poor;
    let ix = Dual::swap_ix_with(&a, SwapSide::BuyBase, BUY, 0);
    assert_eq!(outcome(&d.other(ix)), custom(SPL_INSUFFICIENT_FUNDS));
    assert_eq!(d.p_consumed().0, 0);
}

/// A second market on both programs (another admin, same updater).
fn init_second_market(d: &mut Dual, base: &Pubkey, quote: &Pubkey) -> (Pubkey, Pubkey) {
    let admin = Keypair::new();
    d.env.svm.airdrop(&admin.pubkey(), 10_000_000_000).unwrap();
    let a_market = market_pda(&admin.pubkey(), base, quote);
    let p_market = pino_market_pda(&admin.pubkey(), base, quote).0;
    let (a_base, a_quote) = (vault_pda(&a_market, base), vault_pda(&a_market, quote));
    d.twins.extend([
        (a_market, p_market),
        (a_base, pino_vault_pda(&p_market, base)),
        (a_quote, pino_vault_pda(&p_market, quote)),
    ]);
    let ix = Instruction {
        program_id: propamm::ID,
        accounts: propamm::accounts::Initialize {
            admin: admin.pubkey(),
            base_mint: *base,
            quote_mint: *quote,
            market: a_market,
            base_vault: a_base,
            quote_vault: a_quote,
            token_program: TOKEN_ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: ix::Initialize {
            params: init_params(d.env.updater.pubkey()),
        }
        .data(),
    };
    d.both(ix, &admin).unwrap();
    (a_base, a_quote)
}

/// Account slots of `swap` (the Anchor `Swap` field order).
const BASE_MINT: usize = 2;
const QUOTE_MINT: usize = 3;
const BASE_VAULT: usize = 4;
const QUOTE_VAULT: usize = 5;
const TRADER_BASE: usize = 6;
const TRADER_QUOTE: usize = 7;
const TOKEN_PROGRAM: usize = 8;

/// Unit confusion and account substitution (the `swap.rs` cases), on both programs.
#[test]
fn swap_substitutions_match() {
    let mut d = Dual::priced();
    let (k, trader) = (d.env.admin.insecure_clone(), d.env.other.insecure_clone());
    let (bm, qm) = (d.env.base_mint, d.env.quote_mint);
    let fake_mint = CreateMint::new(&mut d.env.svm, &k).send().unwrap();
    let other_quote = token_account(&mut d.env.svm, &k, &qm, &trader);
    let other_base = token_account(&mut d.env.svm, &k, &bm, &trader);
    let strangers_quote = CreateAccount::new(&mut d.env.svm, &k, &qm)
        .owner(&Keypair::new().pubkey())
        .send()
        .unwrap();
    let (b_base_vault, b_quote_vault) = init_second_market(&mut d, &bm, &qm);
    let mut t22 = d.env.svm.get_account(&d.trader_quote).unwrap();
    t22.owner = TOKEN_2022_ID;
    let t22_account = Pubkey::new_unique();
    d.env.svm.set_account(t22_account, t22).unwrap();

    let invalid_vault = code(PropAmmError::InvalidVault);
    let invalid_mint = code(PropAmmError::InvalidMint);
    let e = &d.env;
    let cases = [
        // Wrong mint, vault, owner or program in a fixed role.
        (TRADER_BASE, other_quote, CONSTRAINT_TOKEN_MINT),
        (TRADER_QUOTE, other_base, CONSTRAINT_TOKEN_MINT),
        (QUOTE_VAULT, other_quote, invalid_vault),
        (BASE_MINT, e.quote_mint, invalid_mint),
        (QUOTE_MINT, e.base_mint, invalid_mint),
        (BASE_MINT, fake_mint, invalid_mint),
        (TRADER_QUOTE, e.admin_quote, CONSTRAINT_TOKEN_OWNER),
        (TRADER_QUOTE, strangers_quote, CONSTRAINT_TOKEN_OWNER),
        (TOKEN_PROGRAM, TOKEN_2022_ID, INVALID_PROGRAM_ID),
        (TOKEN_PROGRAM, Pubkey::new_unique(), INVALID_PROGRAM_ID),
        // A vault in a second role, another market's vaults, a Token-2022-owned account.
        (BASE_VAULT, e.quote_vault, DUPLICATE_MUTABLE_ACCOUNT),
        (TRADER_QUOTE, e.quote_vault, DUPLICATE_MUTABLE_ACCOUNT),
        (TRADER_BASE, b_base_vault, CONSTRAINT_TOKEN_OWNER),
        (BASE_VAULT, b_base_vault, invalid_vault),
        (QUOTE_VAULT, b_quote_vault, invalid_vault),
        (TRADER_QUOTE, t22_account, OWNED_BY_WRONG_PROGRAM),
    ];
    for (slot, key, expected) in cases {
        for side in [SwapSide::BuyBase, SwapSide::SellBase] {
            let mut ix = d.swap_ix(side, BUY, 0);
            ix.accounts[slot].pubkey = key;
            assert_eq!(
                outcome(&d.other(ix)),
                custom(expected),
                "slot {slot} = {key}"
            );
        }
    }
}

#[test]
fn swap_signer_mut_and_key_count_match() {
    let mut d = Dual::priced();
    let mut ix = d.swap_ix(SwapSide::BuyBase, BUY, 0);
    ix.accounts[0].is_signer = false;
    assert_eq!(outcome(&d.admin(ix)), custom(ACCOUNT_NOT_SIGNER));
    let mut ix = d.swap_ix(SwapSide::BuyBase, BUY, 0);
    ix.accounts[BASE_VAULT].is_writable = false;
    assert_eq!(outcome(&d.other(ix)), custom(CONSTRAINT_MUT));
    let mut ix = d.swap_ix(SwapSide::BuyBase, BUY, 0);
    ix.accounts.truncate(8);
    assert_eq!(outcome(&d.other(ix)), custom(NOT_ENOUGH_KEYS));
}

/// `Env` running the `fast-update` build at the Anchor program id, priced at `MID` in slot 10.
fn fast_env() -> Env {
    let mut env = Env::bare();
    env.svm
        .add_program_from_file(propamm::ID, so_path("fast/propamm.so"))
        .unwrap();
    let mut env = Env::initialized_with(env, PARAMS);
    env.svm.warp_to_slot(10);
    env.force(MID).unwrap();
    let updater = env.updater.pubkey();
    env.svm.airdrop(&updater, 1_000_000_000).unwrap();
    env
}

/// Raw fast-path instruction: `[FAST_TAG, mid LE]`, accounts `[updater (s), market (w)]` (the
/// Anchor `UpdatePrice` order).
fn fast_ix(market: Pubkey, updater: Pubkey, mid: u64) -> Instruction {
    let mut data = vec![FAST_TAG];
    data.extend_from_slice(&mid.to_le_bytes());
    Instruction {
        program_id: propamm::ID,
        accounts: vec![
            AccountMeta::new_readonly(updater, true),
            AccountMeta::new(market, false),
        ],
        data,
    }
}

/// From the same snapshot, the Anchor `update_price` and the raw fast path give the same result
/// and byte-identical market accounts, for successes and every failure mode.
#[test]
fn fast_path_matches_update_price_bytes() {
    let mut env = fast_env();
    let (market, updater) = (env.market, env.updater.insecure_clone());
    let compare = |env: &mut Env, slot: u64, mid: u64, expected: Option<u32>| {
        let snapshot = env.svm.get_account(&market).unwrap();
        env.svm.warp_to_slot(slot);
        let ix = env.update_price_ix(&updater.pubkey(), mid);
        let normal = send(&mut env.svm, ix, &updater);
        let normal_data = env.svm.get_account(&market).unwrap().data;
        env.svm.set_account(market, snapshot.clone()).unwrap();
        let fast = send(
            &mut env.svm,
            fast_ix(market, updater.pubkey(), mid),
            &updater,
        );
        let fast_data = env.svm.get_account(&market).unwrap().data;
        assert_eq!(outcome(&normal), outcome(&fast), "slot {slot} mid {mid}");
        assert_eq!(
            outcome(&fast),
            expected.map_or(Ok(()), custom),
            "slot {slot} mid {mid}"
        );
        assert_eq!(normal_data, fast_data, "slot {slot} mid {mid}");
        if expected.is_some() {
            assert_eq!(fast_data, snapshot.data);
        }
        env.svm.set_account(market, snapshot).unwrap();
    };
    let e = |x: PropAmmError| Some(code(x));
    compare(&mut env, 11, MID + 1, None);
    compare(&mut env, 11, MID + BAND, None);
    compare(&mut env, 11, MID - BAND, None);
    compare(
        &mut env,
        11,
        MID + BAND + 1,
        e(PropAmmError::PriceMoveTooLarge),
    );
    compare(&mut env, 10, MID, e(PropAmmError::AlreadyUpdatedThisSlot));
    compare(&mut env, 9, MID, e(PropAmmError::SlotRegression));
    // Window roll: consumed resets, window_start_mid snapshots the old mid.
    env.edit_state(|s| {
        s.ask_consumed_base_atoms = 3;
        s.bid_consumed_base_atoms = u64::MAX;
    });
    compare(&mut env, 12, MID + 7, None);
    env.edit_state(|s| s.paused = 1);
    compare(&mut env, 12, MID, e(PropAmmError::Paused));
    env.edit_state(|s| {
        s.paused = 0;
        s.mid_price = 0;
    });
    compare(&mut env, 12, MID, e(PropAmmError::NotInitializedPrice));
    let max_ok = max_ok_mid();
    env.edit_state(|s| {
        s.mid_price = max_ok;
        s.window_start_mid = max_ok;
    });
    compare(&mut env, 12, max_ok + 1, e(PropAmmError::InvalidParams));
}

#[test]
fn fast_path_rejects_bad_accounts() {
    let mut env = fast_env();
    env.svm.warp_to_slot(11);
    let (market, updater) = (env.market, env.updater.insecure_clone());
    let (admin, other) = (env.admin.insecure_clone(), env.other.insecure_clone());
    // Copies of the market: owner override, data edit.
    let foreign = copy_account(&mut env.svm, &market, Some(Pubkey::new_unique()), &|_| {});
    let bad_disc = copy_account(&mut env.svm, &market, None, &|d| d[0] ^= 1);
    let short = copy_account(&mut env.svm, &market, None, &|d| d.truncate(100));
    let before = env.state();

    let mut cases: Vec<(&str, Instruction, &Keypair, u32)> = vec![
        (
            "admin signs",
            fast_ix(market, admin.pubkey(), MID),
            &admin,
            code(PropAmmError::Unauthorized),
        ),
        (
            "stranger signs",
            fast_ix(market, other.pubkey(), MID),
            &other,
            code(PropAmmError::Unauthorized),
        ),
        (
            "market owned elsewhere",
            fast_ix(foreign, updater.pubkey(), MID),
            &updater,
            OWNED_BY_WRONG_PROGRAM,
        ),
        (
            "wrong discriminator",
            fast_ix(bad_disc, updater.pubkey(), MID),
            &updater,
            DISCRIMINATOR_MISMATCH,
        ),
        (
            "wrong size",
            fast_ix(short, updater.pubkey(), MID),
            &updater,
            DID_NOT_DESERIALIZE,
        ),
    ];
    let mut ix = fast_ix(market, updater.pubkey(), MID);
    ix.accounts[0].is_signer = false;
    cases.push(("updater not signing", ix, &other, ACCOUNT_NOT_SIGNER));
    let mut ix = fast_ix(market, updater.pubkey(), MID);
    ix.accounts[1].is_writable = false;
    cases.push(("read-only market", ix, &updater, CONSTRAINT_MUT));
    let mut ix = fast_ix(market, updater.pubkey(), MID);
    ix.accounts.truncate(1);
    cases.push(("missing market", ix, &updater, NOT_ENOUGH_KEYS));
    let mut ix = fast_ix(market, updater.pubkey(), MID);
    ix.data.truncate(8);
    cases.push(("short data", ix, &updater, INSTRUCTION_DID_NOT_DESERIALIZE));
    for (name, ix, payer, expected) in cases {
        assert_eq!(
            outcome(&send(&mut env.svm, ix, payer)),
            custom(expected),
            "{name}"
        );
    }
    assert_eq!(env.state(), before);
    send(
        &mut env.svm,
        fast_ix(market, updater.pubkey(), MID + 1),
        &updater,
    )
    .unwrap();
    assert_eq!(env.state().mid_price, MID + 1);
}

/// Without the feature the fallback answers exactly like a program without one.
#[test]
fn default_build_has_no_fast_path() {
    let mut env = Env::initialized();
    env.svm.warp_to_slot(10);
    env.force(MID).unwrap();
    env.svm.warp_to_slot(11);
    let updater = env.updater.insecure_clone();
    env.svm.airdrop(&updater.pubkey(), 1_000_000_000).unwrap();
    let before = env.state();
    let res = send(
        &mut env.svm,
        fast_ix(env.market, updater.pubkey(), MID + 1),
        &updater,
    );
    assert_eq!(outcome(&res), custom(INSTRUCTION_FALLBACK_NOT_FOUND));
    assert_eq!(env.state(), before);
}

/// Prints the CU ladder (run with `--nocapture`) and asserts the Pinocchio and fast-path budgets.
/// Scenarios match the Anchor CU tests: deposit 1,000 / withdraw 500, force at slot 10, update
/// to `MID + 1` at slot 11, swaps of `BUY` / `SELL` on `PARAMS`, worst case on 32 levels.
#[test]
fn cu_ladder() {
    let cu = |r: &TxResult| r.as_ref().unwrap().compute_units_consumed;
    let pair = |(a, p): (TxResult, TxResult)| (cu(&a), cu(&p));
    let mut rows: Vec<(&str, (u64, u64))> = vec![];

    let mut d = Dual::bare();
    let admin_kp = d.env.admin.insecure_clone();
    let admin = admin_kp.pubkey();
    let ix = d.env.initialize_ix(init_params(d.env.updater.pubkey()));
    rows.push(("initialize", pair(d.both_results(ix, &admin_kp))));
    let e = &d.env;
    let ix = e.deposit_ix(&admin, e.base_mint, e.base_vault, e.admin_base, 1_000);
    rows.push(("deposit", pair(d.both_results(ix, &admin_kp))));
    let e = &d.env;
    let ix = e.withdraw_ix(&admin, e.base_mint, e.base_vault, e.admin_base, 500);
    rows.push(("withdraw", pair(d.both_results(ix, &admin_kp))));
    let ix = d.env.set_params_ix(&admin, PARAMS);
    rows.push(("set_params", pair(d.both_results(ix, &admin_kp))));
    let ix = d.env.set_updater_ix(&admin, d.env.updater.pubkey());
    rows.push(("set_updater", pair(d.both_results(ix, &admin_kp))));
    let ix = d.env.set_paused_ix(&admin, false);
    rows.push(("set_paused", pair(d.both_results(ix, &admin_kp))));
    d.env.svm.warp_to_slot(10);
    let ix = d.env.force_price_ix(&admin, MID);
    rows.push(("force_price", pair(d.both_results(ix, &admin_kp))));
    d.env.svm.warp_to_slot(11);
    let updater = d.env.updater.insecure_clone();
    let ix = d.env.update_price_ix(&updater.pubkey(), MID + 1);
    let update = pair(d.both_results(ix, &updater));
    rows.push(("update_price", update));

    let mut d = Dual::priced();
    let other = d.env.other.insecure_clone();
    let ix = d.swap_ix(SwapSide::BuyBase, BUY, 0);
    rows.push(("swap BuyBase", pair(d.both_results(ix, &other))));
    let ix = d.swap_ix(SwapSide::SellBase, SELL, 0);
    rows.push(("swap SellBase", pair(d.both_results(ix, &other))));
    let mut d = deep();
    let other = d.env.other.insecure_clone();
    let depth = d.state().ladder().depth_base_atoms().unwrap();
    let ix = d.swap_ix(SwapSide::BuyBase, full_depth_buy_cost(&d.state()), 0);
    rows.push(("swap BuyBase 32 lv", pair(d.both_results(ix, &other))));
    let ix = d.swap_ix(SwapSide::SellBase, depth, 0);
    rows.push(("swap SellBase 32 lv", pair(d.both_results(ix, &other))));

    let mut env = fast_env();
    env.svm.warp_to_slot(11);
    let updater = env.updater.insecure_clone();
    let fast = cu(&send(
        &mut env.svm,
        fast_ix(env.market, updater.pubkey(), MID + 1),
        &updater,
    ));

    println!("| Instruction | Anchor | Anchor fast path | Pinocchio |");
    println!("|---|---|---|---|");
    for (name, (anchor, pino)) in &rows {
        let fast_col = if *name == "update_price" {
            fast.to_string()
        } else {
            "-".to_string()
        };
        println!("| {name} | {anchor} | {fast_col} | {pino} |");
    }

    let get = |name: &str| rows.iter().find(|(n, _)| *n == name).unwrap().1.1;
    for (name, budget) in [
        ("update_price", PINO_UPDATE_CU_BUDGET),
        ("force_price", PINO_FORCE_CU_BUDGET),
        ("deposit", PINO_DEPOSIT_CU_BUDGET),
        ("withdraw", PINO_WITHDRAW_CU_BUDGET),
        ("swap BuyBase", PINO_BUY_CU_BUDGET),
        ("swap SellBase", PINO_SELL_CU_BUDGET),
        ("swap BuyBase 32 lv", PINO_BUY_WORST_CU_BUDGET),
        ("swap SellBase 32 lv", PINO_SELL_WORST_CU_BUDGET),
    ] {
        assert!(
            get(name) <= budget,
            "Pinocchio {name} {} CU > {budget}",
            get(name)
        );
    }
    assert!(
        fast <= FAST_UPDATE_CU_BUDGET,
        "fast path update_price {fast} CU"
    );
    assert!(
        fast < update.0,
        "the fast path must beat the Anchor handler"
    );
}

// ---------------------------------------------------------------------------------------------
// Security-review additions.
// ---------------------------------------------------------------------------------------------

/// Sends `ix` to both programs without asserting equal outcomes: `(anchor, pinocchio)`.
fn each(d: &mut Dual, ix: Instruction, payer: &Keypair) -> (TxResult, TxResult) {
    let p_ix = d.pino_ix(&ix);
    let a = send(&mut d.env.svm, ix, payer);
    let p = send(&mut d.env.svm, p_ix, payer);
    (a, p)
}

/// A market copy per program (same owner override and data edit), registered as a twin pair.
/// Returns the Anchor-side key.
fn fake_market(d: &mut Dual, owner: Option<Pubkey>, edit: &dyn Fn(&mut Vec<u8>)) -> Pubkey {
    let (a_market, p_market) = (d.env.market, d.p_market());
    let pair = (
        copy_account(&mut d.env.svm, &a_market, owner, edit),
        copy_account(&mut d.env.svm, &p_market, owner, edit),
    );
    d.twins.push(pair);
    pair.0
}

/// The Rent sysvar shape does not change what either program creates: the market and both
/// vaults get exactly `solana-rent`'s minimum, for the legacy `{3480, 2.0, 50}` shape (which
/// Pinocchio's own `Rent` would undercount by half) and the SIMD-0194 `{6960, 1.0, 50}` shape.
#[test]
#[allow(deprecated)] // `burn_percent` is a vestigial field of the sysvar.
fn initialize_matches_under_both_rent_shapes() {
    let shapes = [
        (3480, 2.0f64), // legacy
        (6960, 1.0),    // SIMD-0194
    ];
    for (lamports_per_byte, threshold) in shapes {
        let mut d = Dual::bare();
        let rent = solana_rent::Rent {
            lamports_per_byte,
            exemption_threshold: threshold.to_le_bytes(),
            burn_percent: 50,
        };
        d.env.svm.set_sysvar(&rent);
        let ix = d.env.initialize_ix(init_params(d.env.updater.pubkey()));
        d.admin(ix).unwrap();
        let (market, base_vault) = (d.p_market(), d.twin(d.env.base_vault));
        for (account, len) in [(market, 8 + MARKET_STATE_SIZE), (base_vault, 165)] {
            assert_eq!(
                d.env.svm.get_account(&account).unwrap().lamports,
                rent.minimum_balance(len),
                "shape {lamports_per_byte} x {threshold}"
            );
        }
        assert_eq!(
            d.env.svm.get_account(&market).unwrap().lamports,
            (128 + 8 + MARKET_STATE_SIZE as u64) * 6960
        );
    }
}

/// Faults in several accounts at once report the same (first) error on both programs.
#[test]
fn multi_fault_ordering_matches() {
    let mut d = Dual::priced();
    let (k, trader) = (d.env.admin.insecure_clone(), d.env.other.insecure_clone());
    let (bm, qm) = (d.env.base_mint, d.env.quote_mint);
    // Wrong owner (admin's) and wrong mint at once, in each trader token role.
    let wrong_in_base = token_account(&mut d.env.svm, &k, &qm, &k);
    let wrong_in_quote = token_account(&mut d.env.svm, &k, &bm, &k);
    for (slot, account) in [(TRADER_BASE, wrong_in_base), (TRADER_QUOTE, wrong_in_quote)] {
        for side in [SwapSide::BuyBase, SwapSide::SellBase] {
            let mut ix = d.swap_ix(side, BUY, 0);
            ix.accounts[slot].pubkey = account;
            let (a, p) = each(&mut d, ix, &trader);
            assert_eq!(outcome(&a), outcome(&p), "slot {slot}");
            // Anchor checks `token::authority` before `token::mint`.
            assert_eq!(outcome(&a), custom(CONSTRAINT_TOKEN_OWNER), "slot {slot}");
        }
    }
    // Non-signing trader and a read-only market: the signer check comes first.
    let mut ix = d.swap_ix(SwapSide::BuyBase, BUY, 0);
    ix.accounts[0].is_signer = false;
    ix.accounts[1].is_writable = false;
    assert_eq!(outcome(&d.admin(ix)), custom(ACCOUNT_NOT_SIGNER));
}

/// A frozen trader token account passes validation (state 2 unpacks) on both programs, and the
/// SPL transfer then fails with the same error.
#[test]
fn frozen_trader_account_matches() {
    for (side, frozen_role) in [
        (SwapSide::BuyBase, TRADER_QUOTE),
        (SwapSide::SellBase, TRADER_BASE),
    ] {
        let mut d = Dual::priced();
        let trader = d.env.other.insecure_clone();
        let key = d.swap_ix(side, BUY, 0).accounts[frozen_role].pubkey;
        for account in [key, d.twin(key)] {
            let mut acc = d.env.svm.get_account(&account).unwrap();
            acc.data[108] = 2;
            d.env.svm.set_account(account, acc).unwrap();
        }
        let ix = d.swap_ix(side, BUY, 0);
        let (a, p) = each(&mut d, ix, &trader);
        assert_eq!(outcome(&a), outcome(&p), "{side:?}");
        // SPL Token `AccountFrozen`.
        assert_eq!(outcome(&p), custom(17), "{side:?}");
    }
}

/// Market accounts shorter than the discriminator, or larger than `MARKET_SPACE`.
///
/// Documented deviation: Anchor's `AccountLoader` accepts any size `>= 8 + MARKET_STATE_SIZE`
/// (272); the Pinocchio port requires exactly 272 because `initialize` only ever creates that
/// size (`checks::load`). So an oversized market works on Anchor and fails with
/// `AccountDidNotDeserialize` (3003) on Pinocchio. Real markets are always exactly 272 bytes,
/// and no instruction can grow one, so the difference is unreachable for program-created
/// accounts. Under-size accounts behave the same on both.
#[test]
fn market_size_bounds() {
    let mut d = Dual::priced();
    d.env.svm.warp_to_slot(11);
    let (admin, updater, trader) = (
        d.env.admin.insecure_clone(),
        d.env.updater.insecure_clone(),
        d.env.other.insecure_clone(),
    );
    let tiny = fake_market(&mut d, None, &|data| data.truncate(4));
    let eight = fake_market(&mut d, None, &|data| data.truncate(8));
    let big = fake_market(&mut d, None, &|data| data.push(0));
    let (ok, no_disc, no_deser) = (
        Ok(()),
        custom(DISCRIMINATOR_NOT_FOUND),
        custom(DID_NOT_DESERIALIZE),
    );
    let update = |d: &Dual, m: Pubkey| {
        let mut ix = d.env.update_price_ix(&d.env.updater.pubkey(), MID + 1);
        ix.accounts[1].pubkey = m;
        ix
    };
    let swap = |d: &Dual, m: Pubkey| {
        let mut ix = d.swap_ix(SwapSide::BuyBase, BUY, 0);
        ix.accounts[1].pubkey = m;
        ix
    };
    let deposit = |d: &Dual, m: Pubkey| {
        let e = &d.env;
        let mut ix = e.deposit_ix(
            &e.admin.pubkey(),
            e.base_mint,
            e.base_vault,
            e.admin_base,
            1,
        );
        ix.accounts[1].pubkey = m;
        ix
    };
    // < 8 bytes: no discriminator, both programs.
    for ix in [update(&d, tiny), swap(&d, tiny), deposit(&d, tiny)] {
        let payer = if ix.accounts[0].pubkey == updater.pubkey() {
            &updater
        } else if ix.accounts[0].pubkey == admin.pubkey() {
            &admin
        } else {
            &trader
        };
        let (a, p) = each(&mut d, ix, payer);
        assert_eq!(outcome(&a), no_disc);
        assert_eq!(outcome(&p), no_disc);
    }
    // 8 bytes: discriminator only, the body is missing.
    for ix in [update(&d, eight), deposit(&d, eight)] {
        let payer = if ix.accounts[0].pubkey == updater.pubkey() {
            &updater
        } else {
            &admin
        };
        let (a, p) = each(&mut d, ix, payer);
        assert_eq!(outcome(&a), no_deser);
        assert_eq!(outcome(&p), no_deser);
    }
    // > 272 bytes: Anchor accepts, Pinocchio rejects (see above).
    for (ix, payer) in [(update(&d, big), &updater), (deposit(&d, big), &admin)] {
        let (a, p) = each(&mut d, ix, payer);
        assert_eq!(outcome(&a), ok, "anchor accepts an oversized market");
        assert_eq!(outcome(&p), no_deser, "pinocchio requires exactly 272");
    }
    // Swap: Anchor loads the oversized copy too, then fails to sign the vault transfer as the
    // copy (its address is not the PDA the stored seeds derive).
    let ix = swap(&d, big);
    let (a, p) = each(&mut d, ix, &trader);
    assert_eq!(
        outcome(&a),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::PrivilegeEscalation
        ))
    );
    assert_eq!(outcome(&p), no_deser);
}

/// Market substitutes on swap, deposit and withdraw: owned elsewhere, wrong discriminator.
#[test]
fn market_owner_and_discriminator_match_on_swap_and_funds() {
    let mut d = Dual::priced();
    let admin = d.env.admin.insecure_clone();
    let foreign = fake_market(&mut d, Some(Pubkey::new_unique()), &|_| {});
    let bad_disc = fake_market(&mut d, None, &|data| data[0] ^= 1);
    for (market, expected) in [
        (foreign, OWNED_BY_WRONG_PROGRAM),
        (bad_disc, DISCRIMINATOR_MISMATCH),
    ] {
        for side in [SwapSide::BuyBase, SwapSide::SellBase] {
            let mut ix = d.swap_ix(side, BUY, 0);
            ix.accounts[1].pubkey = market;
            assert_eq!(outcome(&d.other(ix)), custom(expected), "swap {side:?}");
        }
        let e = &d.env;
        let ixs = [
            e.deposit_ix(&admin.pubkey(), e.base_mint, e.base_vault, e.admin_base, 1),
            e.withdraw_ix(&admin.pubkey(), e.base_mint, e.base_vault, e.admin_base, 1),
        ];
        for mut ix in ixs {
            ix.accounts[1].pubkey = market;
            assert_eq!(outcome(&d.admin(ix)), custom(expected), "funds");
        }
    }
}

/// Every `mut` account that is passed read-only gives `ConstraintMut` on both programs.
#[test]
fn read_only_accounts_match() {
    let mut d = Dual::priced();
    for slot in [1, QUOTE_VAULT, TRADER_BASE, TRADER_QUOTE] {
        for side in [SwapSide::BuyBase, SwapSide::SellBase] {
            let mut ix = d.swap_ix(side, BUY, 0);
            ix.accounts[slot].is_writable = false;
            assert_eq!(
                outcome(&d.other(ix)),
                custom(CONSTRAINT_MUT),
                "swap slot {slot} {side:?}"
            );
        }
    }
    // initialize with a read-only admin. A fee payer is always writable, so `other` pays and the
    // admin only signs. The `create_account` CPI paying from a read-only admin fails in the
    // runtime before either program's own `mut` check could run.
    let mut u = Dual::bare();
    let (admin, other) = (u.env.admin.insecure_clone(), u.env.other.insecure_clone());
    let mut ix = u.env.initialize_ix(init_params(u.env.updater.pubkey()));
    ix.accounts[0].is_writable = false;
    let p_ix = u.pino_ix(&ix);
    let mut results = vec![];
    for ix in [ix, p_ix] {
        u.env.svm.expire_blockhash();
        let msg = solana_message::Message::new(&[ix], Some(&other.pubkey()));
        let tx = solana_transaction::Transaction::new(
            &[&other, &admin],
            msg,
            u.env.svm.latest_blockhash(),
        );
        results.push(outcome(&u.env.svm.send_transaction(tx)));
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(
        results[0],
        Err(TransactionError::InstructionError(
            0,
            InstructionError::PrivilegeEscalation
        ))
    );
}

/// Accounts after the declared ones are ignored by both programs (Anchor remaining accounts).
#[test]
fn extra_trailing_accounts_are_ignored() {
    let extra = || AccountMeta::new_readonly(Pubkey::new_unique(), false);
    let mut u = Dual::bare();
    let mut ix = u.env.initialize_ix(init_params(u.env.updater.pubkey()));
    ix.accounts.push(extra());
    u.admin(ix).unwrap();

    let mut d = Dual::priced();
    let (admin, updater) = (d.env.admin.pubkey(), d.env.updater.pubkey());
    d.env.svm.warp_to_slot(11);
    // Order matters for state: update at slot 11 (forced at 10), swaps on the fresh price, then the
    // instructions that invalidate it.
    let mut ix = d.env.update_price_ix(&updater, MID + 1);
    ix.accounts.push(extra());
    d.updater(ix).unwrap();
    for side in [SwapSide::BuyBase, SwapSide::SellBase] {
        let mut ix = d.swap_ix(side, BUY, 0);
        ix.accounts.extend([extra(), extra()]);
        d.other(ix).unwrap();
    }
    let e = &d.env;
    let admin_ixs = vec![
        e.deposit_ix(&admin, e.base_mint, e.base_vault, e.admin_base, 1),
        e.withdraw_ix(&admin, e.base_mint, e.base_vault, e.admin_base, 1),
        e.set_updater_ix(&admin, updater),
        e.set_paused_ix(&admin, false),
        e.set_params_ix(&admin, PARAMS),
        e.force_price_ix(&admin, MID),
    ];
    for mut ix in admin_ixs {
        ix.accounts.push(extra());
        d.admin(ix).unwrap();
    }
}

/// The fast path and the normal `update_price` (same program, same snapshot) return the same
/// error for inputs with several faults, so the check order is identical.
#[test]
#[allow(clippy::too_many_lines)] // a case table
fn fast_path_error_parity() {
    let mut env = fast_env();
    env.svm.warp_to_slot(11);
    let (market, updater) = (env.market, env.updater.insecure_clone());
    let (admin, other) = (env.admin.insecure_clone(), env.other.insecure_clone());
    let foreign = copy_account(&mut env.svm, &market, Some(Pubkey::new_unique()), &|_| {});
    let tiny = copy_account(&mut env.svm, &market, None, &|d| d.truncate(4));
    let bad_disc = copy_account(&mut env.svm, &market, None, &|d| d[0] ^= 1);
    let short = copy_account(&mut env.svm, &market, None, &|d| d.truncate(100));
    let big = copy_account(&mut env.svm, &market, None, &|d| d.push(0));

    // (name, market, updater key, signs, market writable, expected code or None for success)
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Pubkey, &Keypair, bool, bool, Option<u32>)> = vec![
        ("ok", market, &updater, true, true, None),
        ("oversized market", big, &updater, true, true, None),
        (
            "non-signer and foreign market",
            foreign,
            &updater,
            false,
            true,
            Some(ACCOUNT_NOT_SIGNER),
        ),
        (
            "foreign market",
            foreign,
            &updater,
            true,
            true,
            Some(OWNED_BY_WRONG_PROGRAM),
        ),
        (
            "market under 8 bytes",
            tiny,
            &updater,
            true,
            true,
            Some(DISCRIMINATOR_NOT_FOUND),
        ),
        (
            "market under 8 bytes, read-only",
            tiny,
            &updater,
            true,
            false,
            Some(DISCRIMINATOR_NOT_FOUND),
        ),
        (
            "bad discriminator, read-only",
            bad_disc,
            &updater,
            true,
            false,
            Some(DISCRIMINATOR_MISMATCH),
        ),
        (
            "read-only market with wrong size",
            short,
            &updater,
            true,
            false,
            Some(CONSTRAINT_MUT),
        ),
        (
            "writable market with wrong size",
            short,
            &updater,
            true,
            true,
            Some(DID_NOT_DESERIALIZE),
        ),
        (
            "read-only market, wrong updater",
            market,
            &other,
            true,
            false,
            Some(CONSTRAINT_MUT),
        ),
        (
            "wrong size, wrong updater",
            short,
            &admin,
            true,
            true,
            Some(DID_NOT_DESERIALIZE),
        ),
        (
            "wrong updater",
            market,
            &admin,
            true,
            true,
            Some(code(PropAmmError::Unauthorized)),
        ),
    ];
    for (name, m, signer, signs, writable, expected) in cases {
        let snapshot = env.svm.get_account(&market).unwrap();
        let mut normal = env.update_price_ix(&signer.pubkey(), MID + 1);
        let mut fast = fast_ix(market, signer.pubkey(), MID + 1);
        for ix in [&mut normal, &mut fast] {
            ix.accounts[0].is_signer = signs;
            ix.accounts[1].pubkey = m;
            ix.accounts[1].is_writable = writable;
        }
        // A non-signing updater needs another fee payer.
        let payer = if signs { signer } else { &other };
        let m_snapshot = env.svm.get_account(&m).unwrap();
        let n = send(&mut env.svm, normal, payer);
        env.svm.set_account(m, m_snapshot).unwrap();
        let f = send(&mut env.svm, fast, payer);
        assert_eq!(outcome(&n), outcome(&f), "{name}");
        assert_eq!(outcome(&f), expected.map_or(Ok(()), custom), "{name}");
        env.svm.set_account(market, snapshot).unwrap();
    }
}
