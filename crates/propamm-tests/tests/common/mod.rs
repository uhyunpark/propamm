//! Shared `LiteSVM` environment for the integration tests.

// dead_code: each test binary uses a subset of these helpers.
// result_large_err: `TxResult` holds LiteSVM's large error type.
#![allow(dead_code, clippy::result_large_err)]

use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas, system_program};
use litesvm::LiteSVM;
use litesvm::types::{FailedTransactionMetadata, TransactionMetadata};
use litesvm_token::spl_token::state::Account as TokenAccountState;
use litesvm_token::{CreateAccount, CreateMint, MintTo, TOKEN_ID, get_spl_account};
use propamm::error::PropAmmError;
use propamm::instruction as ix;
use propamm::instructions::InitParams;
use propamm::state::MarketParams;
use propamm_pricing::MarketState;
use solana_instruction_error::InstructionError;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;
use solana_transaction_error::TransactionError;

pub type Pubkey = anchor_lang::prelude::Pubkey;
pub type TxResult = Result<TransactionMetadata, FailedTransactionMetadata>;

pub const DECIMALS: u8 = 6;
pub const MINTED: u64 = 1_000_000_000;
/// $200 for SOL/USDC.
pub const MID: u64 = 200_000_000_000;

pub const PARAMS: MarketParams = MarketParams {
    level_size_base_atoms: 1_000_000_000,
    num_levels: 4,
    half_spread_bps: 2,
    level_step_bps: 1,
    max_age_slots: 150,
    max_move_bps: 100,
};

pub fn init_params(updater: Pubkey) -> InitParams {
    init_params_from(updater, PARAMS)
}

pub struct Env {
    pub svm: LiteSVM,
    pub admin: Keypair,
    pub updater: Keypair,
    pub other: Keypair,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub market: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    /// Admin token accounts, funded with `MINTED` each.
    pub admin_base: Pubkey,
    pub admin_quote: Pubkey,
    /// Non-admin base token account, funded with `MINTED`.
    pub other_base: Pubkey,
}

pub fn market_pda(admin: &Pubkey, base: &Pubkey, quote: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"market", admin.as_ref(), base.as_ref(), quote.as_ref()],
        &propamm::ID,
    )
    .0
}

pub fn vault_pda(market: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"vault", market.as_ref(), mint.as_ref()], &propamm::ID).0
}

pub fn load_svm() -> LiteSVM {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/propamm.so"
    );
    assert!(
        std::path::Path::new(path).exists(),
        "missing {path}: run `make build-sbf` first"
    );
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(propamm::ID, path)
        .expect("load propamm.so");
    svm
}

pub fn send(svm: &mut LiteSVM, ix: Instruction, payer: &Keypair) -> TxResult {
    svm.expire_blockhash();
    let msg = Message::new(&[ix], Some(&payer.pubkey()));
    let tx = Transaction::new(&[payer], msg, svm.latest_blockhash());
    svm.send_transaction(tx)
}

pub fn token_account(svm: &mut LiteSVM, payer: &Keypair, mint: &Pubkey, owner: &Keypair) -> Pubkey {
    let account = CreateAccount::new(svm, payer, mint)
        .owner(&owner.pubkey())
        .send()
        .unwrap();
    MintTo::new(svm, payer, mint, &account, MINTED)
        .send()
        .unwrap();
    account
}

impl Env {
    /// Program loaded, mints and funded token accounts created, market NOT initialized.
    pub fn bare() -> Self {
        Self::bare_with_decimals(DECIMALS)
    }

    /// Like `bare`, with a custom base mint decimals (the quote mint keeps `DECIMALS`).
    pub fn bare_with_decimals(base_decimals: u8) -> Self {
        let mut svm = load_svm();
        let [admin, updater, other] = [(); 3].map(|()| Keypair::new());
        for k in [&admin, &other] {
            svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
        }
        let base_mint = CreateMint::new(&mut svm, &admin)
            .decimals(base_decimals)
            .send()
            .unwrap();
        let quote_mint = CreateMint::new(&mut svm, &admin)
            .decimals(DECIMALS)
            .send()
            .unwrap();
        let admin_base = token_account(&mut svm, &admin, &base_mint, &admin);
        let admin_quote = token_account(&mut svm, &admin, &quote_mint, &admin);
        let other_base = token_account(&mut svm, &admin, &base_mint, &other);
        let market = market_pda(&admin.pubkey(), &base_mint, &quote_mint);
        Self {
            svm,
            admin,
            updater,
            other,
            base_mint,
            quote_mint,
            market,
            base_vault: vault_pda(&market, &base_mint),
            quote_vault: vault_pda(&market, &quote_mint),
            admin_base,
            admin_quote,
            other_base,
        }
    }

    /// Like `bare`, plus a successful `initialize`.
    pub fn initialized() -> Self {
        Self::initialized_with(Self::bare(), PARAMS)
    }

    /// Successful `initialize` of `env` with `params`.
    pub fn initialized_with(mut env: Self, params: MarketParams) -> Self {
        let ix = env.initialize_ix(init_params_from(env.updater.pubkey(), params));
        let admin = env.admin.insecure_clone();
        send(&mut env.svm, ix, &admin).unwrap();
        env
    }

    pub fn initialize_ix(&self, params: InitParams) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: propamm::accounts::Initialize {
                admin: self.admin.pubkey(),
                base_mint: self.base_mint,
                quote_mint: self.quote_mint,
                market: self.market,
                base_vault: self.base_vault,
                quote_vault: self.quote_vault,
                token_program: TOKEN_ID,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
            data: ix::Initialize { params }.data(),
        }
    }

    pub fn funds_accounts(
        &self,
        signer: &Pubkey,
        mint: Pubkey,
        vault: Pubkey,
        token: Pubkey,
    ) -> Vec<anchor_lang::prelude::AccountMeta> {
        propamm::accounts::Funds {
            admin: *signer,
            market: self.market,
            mint,
            vault,
            admin_token: token,
            token_program: TOKEN_ID,
        }
        .to_account_metas(None)
    }

    pub fn deposit_ix(
        &self,
        signer: &Pubkey,
        mint: Pubkey,
        vault: Pubkey,
        token: Pubkey,
        amount: u64,
    ) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: self.funds_accounts(signer, mint, vault, token),
            data: ix::Deposit { amount }.data(),
        }
    }

    pub fn withdraw_ix(
        &self,
        signer: &Pubkey,
        mint: Pubkey,
        vault: Pubkey,
        token: Pubkey,
        amount: u64,
    ) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: self.funds_accounts(signer, mint, vault, token),
            data: ix::Withdraw { amount }.data(),
        }
    }

    pub fn admin_only_accounts(&self, signer: &Pubkey) -> Vec<anchor_lang::prelude::AccountMeta> {
        propamm::accounts::AdminOnly {
            admin: *signer,
            market: self.market,
        }
        .to_account_metas(None)
    }

    pub fn set_params_ix(&self, signer: &Pubkey, params: MarketParams) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: self.admin_only_accounts(signer),
            data: ix::SetParams { params }.data(),
        }
    }

    pub fn set_updater_ix(&self, signer: &Pubkey, new_updater: Pubkey) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: self.admin_only_accounts(signer),
            data: ix::SetUpdater { new_updater }.data(),
        }
    }

    pub fn set_paused_ix(&self, signer: &Pubkey, paused: bool) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: self.admin_only_accounts(signer),
            data: ix::SetPaused { paused }.data(),
        }
    }

    /// Admin deposits `amount` of base into the base vault.
    pub fn fund_base(&mut self, amount: u64) {
        let ix = self.deposit_ix(
            &self.admin.pubkey(),
            self.base_mint,
            self.base_vault,
            self.admin_base,
            amount,
        );
        self.send_admin(ix).unwrap();
    }

    pub fn update_price_ix(&self, signer: &Pubkey, mid: u64) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: propamm::accounts::UpdatePrice {
                updater: *signer,
                market: self.market,
            }
            .to_account_metas(None),
            data: ix::UpdatePrice { mid }.data(),
        }
    }

    pub fn force_price_ix(&self, signer: &Pubkey, mid: u64) -> Instruction {
        Instruction {
            program_id: propamm::ID,
            accounts: self.admin_only_accounts(signer),
            data: ix::ForcePrice { mid }.data(),
        }
    }

    pub fn send_updater(&mut self, ix: Instruction) -> TxResult {
        let updater = self.updater.insecure_clone();
        self.svm.airdrop(&updater.pubkey(), 1_000_000_000).unwrap();
        send(&mut self.svm, ix, &updater)
    }

    /// Admin `force_price` to `mid`.
    pub fn force(&mut self, mid: u64) -> TxResult {
        let ix = self.force_price_ix(&self.admin.pubkey(), mid);
        self.send_admin(ix)
    }

    pub fn update(&mut self, mid: u64) -> TxResult {
        let ix = self.update_price_ix(&self.updater.pubkey(), mid);
        self.send_updater(ix)
    }

    /// Overwrites the market state directly (e.g. to set `consumed`, which only swap moves).
    pub fn edit_state(&mut self, f: impl FnOnce(&mut MarketState)) {
        let mut acc = self.svm.get_account(&self.market).unwrap();
        let mut state = self.state();
        f(&mut state);
        acc.data[8..].copy_from_slice(bytemuck::bytes_of(&state));
        self.svm.set_account(self.market, acc).unwrap();
    }

    pub fn state(&self) -> MarketState {
        let data = self
            .svm
            .get_account(&self.market)
            .expect("market account")
            .data;
        bytemuck::pod_read_unaligned(&data[8..])
    }

    /// `(ask, bid)` consumed base atoms.
    pub fn consumed(&self) -> (u64, u64) {
        let s = self.state();
        (s.ask_consumed_base_atoms, s.bid_consumed_base_atoms)
    }

    pub fn balance(&self, account: &Pubkey) -> u64 {
        get_spl_account::<TokenAccountState>(&self.svm, account)
            .unwrap()
            .amount
    }

    pub fn send_admin(&mut self, ix: Instruction) -> TxResult {
        let admin = self.admin.insecure_clone();
        send(&mut self.svm, ix, &admin)
    }

    pub fn send_other(&mut self, ix: Instruction) -> TxResult {
        let other = self.other.insecure_clone();
        send(&mut self.svm, ix, &other)
    }
}

/// Asserts the transaction failed with the given program error.
#[track_caller]
pub fn assert_program_err(res: &TxResult, expected: PropAmmError) {
    assert_custom_code(res, u32::from(expected));
}

/// Asserts the transaction failed in instruction 0 with a raw custom code
/// (also used for errors that come from the SPL Token or System programs).
#[track_caller]
pub fn assert_custom_code(res: &TxResult, code: u32) {
    let err = res.as_ref().expect_err("transaction should fail");
    assert_eq!(
        err.err,
        TransactionError::InstructionError(0, InstructionError::Custom(code)),
        "logs: {:?}",
        err.meta.logs
    );
}

pub fn init_params_from(updater: Pubkey, p: MarketParams) -> InitParams {
    InitParams {
        updater,
        level_size_base_atoms: p.level_size_base_atoms,
        num_levels: p.num_levels,
        half_spread_bps: p.half_spread_bps,
        level_step_bps: p.level_step_bps,
        max_age_slots: p.max_age_slots,
        max_move_bps: p.max_move_bps,
    }
}
