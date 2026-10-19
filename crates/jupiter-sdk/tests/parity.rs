//! Quote parity: the adapter's `quote` must equal the real program's token deltas exactly.
//!
//! This replaces `jupiter-amm-test-kit`, which pins litesvm 0.14 and cannot share this
//! workspace's lockfile (litesvm 0.17). Run `make build-sbf` first.

use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas, system_program};
use jupiter_amm_interface::{
    Amm, AmmContext, ClockRef, FeeMode, KeyedAccount, QuoteParams, Swap, SwapMode, SwapParams,
};
use litesvm::LiteSVM;
use litesvm_token::spl_token::state::Account as TokenAccountState;
use litesvm_token::{CreateAccount, CreateMint, MintTo, TOKEN_ID, get_spl_account};
use propamm::instruction as ix;
use propamm::instructions::InitParams;
use propamm_jupiter::{
    MARKET_DISCRIMINATOR, PROGRAM_ID, PropAmm, SWAP_DISCRIMINATOR, swap_instruction_data,
};
use propamm_pricing::Side;
use sha2::{Digest, Sha256};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

type Pubkey = anchor_lang::prelude::Pubkey;

const MID: u64 = 200_000_000_000;
const FUND: u64 = 5_000_000_000;
const LEVEL: u64 = 1_000_000_000;
const MAX_AGE: u64 = 150;
const START_SLOT: u64 = 10;

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    trader: Keypair,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    market: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    trader_base: Pubkey,
    trader_quote: Pubkey,
}

fn send(svm: &mut LiteSVM, ix: Instruction, signer: &Keypair) {
    svm.expire_blockhash();
    let msg = Message::new(&[ix], Some(&signer.pubkey()));
    let tx = Transaction::new(&[signer], msg, svm.latest_blockhash());
    svm.send_transaction(tx).expect("transaction succeeds");
}

fn balance(svm: &LiteSVM, account: &Pubkey) -> u64 {
    get_spl_account::<TokenAccountState>(svm, account)
        .unwrap()
        .amount
}

fn load_program() -> LiteSVM {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/propamm.so"
    );
    assert!(
        std::path::Path::new(path).exists(),
        "missing {path}: run `make build-sbf` first"
    );
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(propamm::ID, path).unwrap();
    svm
}

/// Token account of `mint` owned by `owner`, holding `FUND * 2` atoms.
fn funded_account(svm: &mut LiteSVM, admin: &Keypair, mint: &Pubkey, owner: &Keypair) -> Pubkey {
    let acc = CreateAccount::new(svm, admin, mint)
        .owner(&owner.pubkey())
        .send()
        .unwrap();
    MintTo::new(svm, admin, mint, &acc, FUND * 2)
        .send()
        .unwrap();
    acc
}

impl Env {
    /// Funded market; priced at `MID` only when `priced`.
    fn new(priced: bool) -> Self {
        let mut svm = load_program();
        let (admin, trader, updater) = (Keypair::new(), Keypair::new(), Keypair::new());
        for k in [&admin, &trader] {
            svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
        }
        let mut mint = || {
            CreateMint::new(&mut svm, &admin)
                .decimals(6)
                .send()
                .unwrap()
        };
        let (base_mint, quote_mint) = (mint(), mint());
        let market = Pubkey::find_program_address(
            &[
                b"market",
                admin.pubkey().as_ref(),
                base_mint.as_ref(),
                quote_mint.as_ref(),
            ],
            &propamm::ID,
        )
        .0;
        let vault = |mint: &Pubkey| {
            Pubkey::find_program_address(&[b"vault", market.as_ref(), mint.as_ref()], &propamm::ID)
                .0
        };
        let (base_vault, quote_vault) = (vault(&base_mint), vault(&quote_mint));
        let admin_base = funded_account(&mut svm, &admin, &base_mint, &admin);
        let admin_quote = funded_account(&mut svm, &admin, &quote_mint, &admin);
        let trader_base = funded_account(&mut svm, &admin, &base_mint, &trader);
        let trader_quote = funded_account(&mut svm, &admin, &quote_mint, &trader);

        let init = Instruction {
            program_id: propamm::ID,
            accounts: propamm::accounts::Initialize {
                admin: admin.pubkey(),
                base_mint,
                quote_mint,
                market,
                base_vault,
                quote_vault,
                token_program: TOKEN_ID,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
            data: ix::Initialize {
                params: InitParams {
                    updater: updater.pubkey(),
                    level_size_base_atoms: LEVEL,
                    num_levels: 4,
                    half_spread_bps: 2,
                    level_step_bps: 1,
                    max_age_slots: MAX_AGE,
                    max_move_bps: 100,
                },
            }
            .data(),
        };
        send(&mut svm, init, &admin);
        let mut env = Self {
            svm,
            admin,
            trader,
            base_mint,
            quote_mint,
            market,
            base_vault,
            quote_vault,
            trader_base,
            trader_quote,
        };
        env.funds_ix(
            base_mint,
            base_vault,
            admin_base,
            ix::Deposit { amount: FUND }.data(),
        );
        env.funds_ix(
            quote_mint,
            quote_vault,
            admin_quote,
            ix::Deposit { amount: FUND }.data(),
        );
        env.svm.warp_to_slot(START_SLOT);
        if priced {
            env.admin_ix(ix::ForcePrice { mid: MID }.data());
        }
        env
    }

    /// Sends an admin deposit/withdraw (`Funds` accounts) for one side of the market.
    fn funds_ix(&mut self, mint: Pubkey, vault: Pubkey, admin_token: Pubkey, data: Vec<u8>) {
        let ix = Instruction {
            program_id: propamm::ID,
            accounts: propamm::accounts::Funds {
                admin: self.admin.pubkey(),
                market: self.market,
                mint,
                vault,
                admin_token,
                token_program: TOKEN_ID,
            }
            .to_account_metas(None),
            data,
        };
        self.send_admin(ix);
    }

    fn send_admin(&mut self, ix: Instruction) {
        let admin = self.admin.insecure_clone();
        send(&mut self.svm, ix, &admin);
    }

    fn admin_ix(&mut self, data: Vec<u8>) {
        let ix = Instruction {
            program_id: propamm::ID,
            accounts: propamm::accounts::AdminOnly {
                admin: self.admin.pubkey(),
                market: self.market,
            }
            .to_account_metas(None),
            data,
        };
        self.send_admin(ix);
    }

    /// Fresh adapter built from the current `LiteSVM` accounts, with the clock at `slot`.
    fn snapshot_at(&self, slot: u64) -> PropAmm {
        let ctx = AmmContext {
            clock_ref: ClockRef::default(),
        };
        ctx.clock_ref.slot.store(slot, Ordering::Relaxed);
        let keyed = KeyedAccount {
            key: self.market,
            account: self.svm.get_account(&self.market).unwrap(),
            params: None,
        };
        let mut amm = PropAmm::from_keyed_account(&keyed, &ctx).unwrap();
        let accounts: HashMap<Pubkey, Box<_>> = amm
            .get_accounts_to_update()
            .into_iter()
            .map(|k| (k, Box::new(self.svm.get_account(&k).unwrap())))
            .collect();
        amm.update(&accounts).unwrap();
        amm
    }

    fn snapshot(&self) -> PropAmm {
        self.snapshot_at(START_SLOT)
    }
}

fn params(input: Pubkey, output: Pubkey, amount: u64) -> QuoteParams {
    QuoteParams {
        amount,
        input_mint: input,
        output_mint: output,
        swap_mode: SwapMode::ExactIn,
        fee_mode: FeeMode::Normal,
    }
}

/// Quotes, executes the adapter's instruction as a top-level tx, and asserts exact token deltas.
fn assert_parity(env: &mut Env, side: Side, amount: u64) -> u64 {
    let (src_mint, dst_mint, src_acc, dst_acc) = match side {
        Side::BuyBase => (
            env.quote_mint,
            env.base_mint,
            env.trader_quote,
            env.trader_base,
        ),
        Side::SellBase => (
            env.base_mint,
            env.quote_mint,
            env.trader_base,
            env.trader_quote,
        ),
    };
    let amm = env.snapshot();
    let quote = amm.quote(&params(src_mint, dst_mint, amount)).unwrap();
    assert_eq!(quote.in_amount, amount);
    assert_eq!(quote.fee_amount, 0);

    let jupiter = Pubkey::new_unique();
    let swap_params = SwapParams {
        swap_mode: SwapMode::ExactIn,
        in_amount: amount,
        out_amount: quote.out_amount,
        source_mint: src_mint,
        destination_mint: dst_mint,
        source_token_account: src_acc,
        destination_token_account: dst_acc,
        token_transfer_authority: env.trader.pubkey(),
        user: env.trader.pubkey(),
        payer: env.trader.pubkey(),
        quote_mint_to_referrer: None,
        jupiter_program_id: &jupiter,
        missing_dynamic_accounts_as_default: false,
    };
    let metas = amm.get_swap_and_account_metas(&swap_params).unwrap();
    let Swap::Placeholder { data: Some(tag) } = metas.swap else {
        panic!("expected Swap::Placeholder");
    };
    assert_eq!(tag.len(), 1);

    let (in_before, out_before) = (balance(&env.svm, &src_acc), balance(&env.svm, &dst_acc));
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: metas.account_metas,
        data: swap_instruction_data(side, amount, quote.out_amount),
    };
    let trader = env.trader.insecure_clone();
    send(&mut env.svm, ix, &trader);
    assert_eq!(in_before - balance(&env.svm, &src_acc), amount);
    assert_eq!(
        balance(&env.svm, &dst_acc) - out_before,
        quote.out_amount,
        "{side:?} amount {amount}"
    );
    quote.out_amount
}

#[test]
fn discriminators_match_anchor() {
    let hash = |s: &str| <[u8; 8]>::try_from(&Sha256::digest(s)[..8]).unwrap();
    assert_eq!(MARKET_DISCRIMINATOR, hash("account:Market"));
    assert_eq!(SWAP_DISCRIMINATOR, hash("global:swap"));
    assert_eq!(PROGRAM_ID, propamm::ID);
    assert_eq!(
        swap_instruction_data(Side::SellBase, 7, 3),
        ix::Swap {
            side: propamm::instructions::SwapSide::SellBase,
            amount_in: 7,
            min_out: 3
        }
        .data()
    );
}

#[test]
fn quote_matches_program_buy_and_sell() {
    let mut env = Env::new(true);
    // Single level, then multi-level (4e8 quote atoms buys ~2e9 base atoms = 2 levels).
    assert_parity(&mut env, Side::BuyBase, 100_000_000);
    // Starts mid-level 0 after the prior swap and crosses into level 1 and 2.
    assert_parity(&mut env, Side::BuyBase, 400_000_000);
    assert_parity(&mut env, Side::SellBase, 500_000_000);
    // Multi-level sell starting mid-level: 2.4e9 base atoms spans levels 0..=2 from offset 0.5e9.
    assert_parity(&mut env, Side::SellBase, 1_700_000_000);
}

#[test]
fn accessors_and_account_list() {
    let env = Env::new(true);
    let amm = env.snapshot();
    assert_eq!(amm.key(), env.market);
    assert_eq!(amm.program_id(), PROGRAM_ID);
    assert_eq!(amm.get_reserve_mints(), vec![env.base_mint, env.quote_mint]);
    assert_eq!(
        amm.get_accounts_to_update(),
        vec![env.market, env.base_vault, env.quote_vault]
    );
    assert!(amm.is_active());
    assert!(!amm.supports_exact_out());
}

#[test]
fn quote_errors() {
    let mut env = Env::new(true);
    let (b, q) = (env.base_mint, env.quote_mint);

    // Staleness boundary: age == max_age is fresh, +1 is stale.
    let amm = env.snapshot_at(START_SLOT + MAX_AGE);
    assert!(amm.quote(&params(q, b, 100_000_000)).is_ok());
    let amm = env.snapshot_at(START_SLOT + MAX_AGE + 1);
    assert!(amm.quote(&params(q, b, 100_000_000)).is_err());

    // Unknown mints and same-mint pairs.
    let amm = env.snapshot();
    let other = Pubkey::new_unique();
    for (i, o) in [(q, other), (other, b), (b, b), (q, q)] {
        assert!(amm.quote(&params(i, o, 100_000_000)).is_err());
    }

    // ExactOut and zero amount.
    let mut p = params(q, b, 100_000_000);
    p.swap_mode = SwapMode::ExactOut;
    assert!(amm.quote(&p).is_err());
    assert!(amm.quote(&params(q, b, 0)).is_err());

    // Insufficient depth (4 levels of 1e9 base atoms; far more than that).
    assert!(amm.quote(&params(q, b, 2_000_000_000)).is_err());

    // Paused.
    env.admin_ix(ix::SetPaused { paused: true }.data());
    let amm = env.snapshot();
    assert!(!amm.is_active());
    assert!(amm.quote(&params(q, b, 100_000_000)).is_err());
    env.admin_ix(ix::SetPaused { paused: false }.data());
    assert!(env.snapshot().is_active());
}

#[test]
fn quote_errors_when_unpriced() {
    let env = Env::new(false);
    let amm = env.snapshot();
    let (b, q) = (env.base_mint, env.quote_mint);
    assert!(amm.quote(&params(q, b, 100_000_000)).is_err());
}

#[test]
fn quote_errors_on_empty_out_vault() {
    let mut env = Env::new(true);
    // Withdraw all base from the vault: BuyBase has nothing to pay out.
    let admin_base = CreateAccount::new(&mut env.svm, &env.admin.insecure_clone(), &env.base_mint)
        .owner(&env.admin.pubkey())
        .send()
        .unwrap();
    env.funds_ix(
        env.base_mint,
        env.base_vault,
        admin_base,
        ix::Withdraw { amount: FUND }.data(),
    );
    let amm = env.snapshot();
    assert!(
        amm.quote(&params(env.quote_mint, env.base_mint, 100_000_000))
            .is_err()
    );
    // The other side still quotes.
    assert!(
        amm.quote(&params(env.base_mint, env.quote_mint, 100_000_000))
            .is_ok()
    );
}
