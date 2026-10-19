//! Jupiter `Amm` adapter for `PropAMM`.
//!
//! Not submission-ready: which `jupiter-amm-interface` version Jupiter integrates is
//! unconfirmed, and a real listing needs a Jupiter-assigned `Swap` variant (this adapter
//! uses `Swap::Placeholder`, with the side as the single data byte).
//!
//! Fees: `Quote::fee_amount` is 0 and `fee_pct` is 0. The spread is inside the ladder
//! price, so there is no separate fee to report.
#![deny(clippy::unwrap_used, clippy::expect_used)]

use jupiter_amm_interface::{
    AccountProvider, Amm, AmmContext, AmmError, AmmLabel, ClockRef, KeyedAccount, Quote,
    QuoteParams, SingleProgramAmm, Swap, SwapAndAccountMetas, SwapMode, SwapParams,
    single_program_amm,
};
use propamm_client::{decode_market as decode_market_account, swap_accounts, swap_data};
use propamm_pricing::{MarketState, Side, is_fresh};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use std::sync::atomic::Ordering;

pub use propamm_client::{MARKET_DISCRIMINATOR, PROGRAM_ID, SWAP_DISCRIMINATOR, TOKEN_PROGRAM_ID};

/// `PropAMM` market snapshot for quoting.
#[derive(Clone)]
pub struct PropAmm {
    key: Pubkey,
    state: MarketState,
    base_vault_balance: u64,
    quote_vault_balance: u64,
    clock_ref: ClockRef,
}

single_program_amm!(PropAmm, PROGRAM_ID, "PropAMM");

fn decode_market(data: &[u8]) -> Result<MarketState, AmmError> {
    decode_market_account(data).map_err(|e| AmmError::Custom(e.to_string()))
}

fn token_amount(data: &[u8]) -> Result<u64, AmmError> {
    propamm_client::token_amount(data).ok_or_else(|| "token account is too short".into())
}

impl PropAmm {
    fn base_mint(&self) -> Pubkey {
        Pubkey::new_from_array(self.state.base_mint)
    }

    fn quote_mint(&self) -> Pubkey {
        Pubkey::new_from_array(self.state.quote_mint)
    }

    fn base_vault(&self) -> Pubkey {
        Pubkey::new_from_array(self.state.base_vault)
    }

    fn quote_vault(&self) -> Pubkey {
        Pubkey::new_from_array(self.state.quote_vault)
    }

    /// `BuyBase` when paying quote for base, `SellBase` for the reverse; `None` for any other pair.
    fn side_for(&self, input: &Pubkey, output: &Pubkey) -> Option<Side> {
        if *input == self.quote_mint() && *output == self.base_mint() {
            Some(Side::BuyBase)
        } else if *input == self.base_mint() && *output == self.quote_mint() {
            Some(Side::SellBase)
        } else {
            None
        }
    }
}

impl Amm for PropAmm {
    fn from_keyed_account(
        keyed_account: &KeyedAccount,
        amm_context: &AmmContext,
    ) -> Result<Self, AmmError> {
        if keyed_account.account.owner != PROGRAM_ID {
            return Err("account is not owned by the PropAMM program".into());
        }
        Ok(Self {
            key: keyed_account.key,
            state: decode_market(&keyed_account.account.data)?,
            base_vault_balance: 0,
            quote_vault_balance: 0,
            clock_ref: amm_context.clock_ref.clone(),
        })
    }

    fn label(&self) -> AmmLabel {
        Self::LABEL
    }

    fn program_id(&self) -> Pubkey {
        PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.base_mint(), self.quote_mint()]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![self.key, self.base_vault(), self.quote_vault()]
    }

    fn update(&mut self, account_provider: impl AccountProvider) -> Result<(), AmmError> {
        let state = decode_market(account_provider.try_get(&self.key)?.data())?;
        let base = token_amount(account_provider.try_get(&self.base_vault())?.data())?;
        let quote = token_amount(account_provider.try_get(&self.quote_vault())?.data())?;
        self.state = state;
        self.base_vault_balance = base;
        self.quote_vault_balance = quote;
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote, AmmError> {
        if quote_params.swap_mode != SwapMode::ExactIn {
            return Err("ExactOut is not supported".into());
        }
        let side = self
            .side_for(&quote_params.input_mint, &quote_params.output_mint)
            .ok_or("mint pair is not this market's pair")?;
        let s = &self.state;
        if s.is_paused() {
            return Err("market is paused".into());
        }
        if s.mid_price == 0 {
            return Err("market has no price".into());
        }
        let slot = self.clock_ref.slot.load(Ordering::Relaxed);
        if !is_fresh(slot, s.last_update_slot, s.max_age_slots) {
            return Err("price is stale".into());
        }
        let fill = s
            .ladder()
            .quote_exact_in(side, s.consumed(side), quote_params.amount)
            .map_err(|e| AmmError::Custom(format!("{e:?}")))?;
        let out_balance = match side {
            Side::BuyBase => self.base_vault_balance,
            Side::SellBase => self.quote_vault_balance,
        };
        if out_balance < fill.amount_out {
            return Err("insufficient vault balance".into());
        }
        Ok(Quote {
            in_amount: quote_params.amount,
            out_amount: fill.amount_out,
            fee_mint: quote_params.input_mint,
            ..Quote::default()
        })
    }

    fn get_swap_and_account_metas(&self, p: &SwapParams) -> Result<SwapAndAccountMetas, AmmError> {
        if p.swap_mode != SwapMode::ExactIn {
            return Err("ExactOut is not supported".into());
        }
        let side = self
            .side_for(&p.source_mint, &p.destination_mint)
            .ok_or("mint pair is not this market's pair")?;
        let (trader_base, trader_quote) = match side {
            Side::BuyBase => (p.destination_token_account, p.source_token_account),
            Side::SellBase => (p.source_token_account, p.destination_token_account),
        };
        let account_metas = swap_accounts(
            &p.token_transfer_authority,
            &self.key,
            &self.state,
            &trader_base,
            &trader_quote,
        );
        Ok(SwapAndAccountMetas {
            swap: Swap::Placeholder {
                data: Some(vec![propamm_client::side_byte(side)]),
            },
            account_metas,
        })
    }

    fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
        vec![(PROGRAM_ID, "propamm".into())]
    }

    fn get_accounts_len(&self) -> usize {
        // 9 swap accounts + the program itself.
        10
    }

    fn is_active(&self) -> bool {
        !self.state.is_paused()
    }
}

/// Builds the program's swap instruction data: discriminator + Borsh(side, `amount_in`, `min_out`).
pub fn swap_instruction_data(side: Side, amount_in: u64, min_out: u64) -> Vec<u8> {
    swap_data(side, amount_in, min_out)
}
