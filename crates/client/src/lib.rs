//! Shared client helpers for the `PropAMM` program: program id, PDAs, instruction builders and
//! account decoding. Dependency-light on purpose: the bot, the Jupiter adapter and the web/WASM
//! client share this one encoding of the program's wire format.
//!
//! Instruction data is `discriminator ++ Borsh(args)`, written by hand here and checked against
//! the Anchor-generated types in `tests/parity.rs`.
#![deny(clippy::unwrap_used, clippy::expect_used)]

use std::fmt;

use propamm_pricing::{MARKET_STATE_SIZE, MarketState, Side};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::{Pubkey, pubkey};

/// Program id from `declare_id!` in `programs/propamm`.
pub const PROGRAM_ID: Pubkey = pubkey!("2PuFKsYDoYaVWFnSxVGgbD2Xvsz3nymLu2WuctvF6V3F");
/// SPL Token program (the only token program the market supports).
pub const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// System program.
pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");

pub const MARKET_SEED: &[u8] = b"market";
pub const VAULT_SEED: &[u8] = b"vault";

/// `sha256("account:Market")[..8]`.
pub const MARKET_DISCRIMINATOR: [u8; 8] = [0xdb, 0xbe, 0xd5, 0x37, 0x00, 0xe3, 0xc6, 0x9a];
/// `sha256("global:initialize")[..8]`.
pub const INITIALIZE_DISCRIMINATOR: [u8; 8] = [0xaf, 0xaf, 0x6d, 0x1f, 0x0d, 0x98, 0x9b, 0xed];
/// `sha256("global:deposit")[..8]`.
pub const DEPOSIT_DISCRIMINATOR: [u8; 8] = [0xf2, 0x23, 0xc6, 0x89, 0x52, 0xe1, 0xf2, 0xb6];
/// `sha256("global:withdraw")[..8]`.
pub const WITHDRAW_DISCRIMINATOR: [u8; 8] = [0xb7, 0x12, 0x46, 0x9c, 0x94, 0x6d, 0xa1, 0x22];
/// `sha256("global:set_params")[..8]`.
pub const SET_PARAMS_DISCRIMINATOR: [u8; 8] = [0x1b, 0xea, 0xb2, 0x34, 0x93, 0x02, 0xbb, 0x8d];
/// `sha256("global:set_updater")[..8]`.
pub const SET_UPDATER_DISCRIMINATOR: [u8; 8] = [0x95, 0xe5, 0x6c, 0xbf, 0x00, 0xd5, 0xfe, 0xd2];
/// `sha256("global:set_paused")[..8]`.
pub const SET_PAUSED_DISCRIMINATOR: [u8; 8] = [0x5b, 0x3c, 0x7d, 0xc0, 0xb0, 0xe1, 0xa6, 0xda];
/// `sha256("global:force_price")[..8]`.
pub const FORCE_PRICE_DISCRIMINATOR: [u8; 8] = [0x0f, 0x56, 0x4c, 0x82, 0x7e, 0xb7, 0xfe, 0x40];
/// `sha256("global:update_price")[..8]`.
pub const UPDATE_PRICE_DISCRIMINATOR: [u8; 8] = [0x3d, 0x22, 0x75, 0x9b, 0x4b, 0x22, 0x7b, 0xd0];
/// `sha256("global:swap")[..8]`.
pub const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

/// Account size: 8-byte discriminator + `MarketState`.
pub const MARKET_ACCOUNT_SIZE: usize = 8 + MARKET_STATE_SIZE;

/// Why a Market account could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    WrongSize { got: usize },
    WrongDiscriminator,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSize { got } => {
                write!(
                    f,
                    "market account has {got} bytes, expected {MARKET_ACCOUNT_SIZE}"
                )
            }
            Self::WrongDiscriminator => f.write_str("not a Market account"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decodes a Market account: size and discriminator checks, then the pod body.
pub fn decode_market(data: &[u8]) -> Result<MarketState, DecodeError> {
    if data.len() != MARKET_ACCOUNT_SIZE {
        return Err(DecodeError::WrongSize { got: data.len() });
    }
    let (disc, body) = data.split_at(8);
    if disc != MARKET_DISCRIMINATOR {
        return Err(DecodeError::WrongDiscriminator);
    }
    Ok(bytemuck::pod_read_unaligned(body))
}

/// SPL token account: `amount` is a little-endian u64 at bytes 64..72.
pub fn token_amount(data: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = data.get(64..72)?.try_into().ok()?;
    Some(u64::from_le_bytes(bytes))
}

/// Market PDA for `(admin, base_mint, quote_mint)`.
pub fn market_pda(admin: &Pubkey, base_mint: &Pubkey, quote_mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(
        &[
            MARKET_SEED,
            admin.as_ref(),
            base_mint.as_ref(),
            quote_mint.as_ref(),
        ],
        &PROGRAM_ID,
    )
}

/// Vault PDA holding `mint` for `market`.
pub fn vault_pda(market: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[VAULT_SEED, market.as_ref(), mint.as_ref()], &PROGRAM_ID)
}

/// Borsh index of `SwapSide` in the program: `BuyBase` = 0, `SellBase` = 1.
pub const fn side_byte(side: Side) -> u8 {
    match side {
        Side::BuyBase => 0,
        Side::SellBase => 1,
    }
}

/// `InitParams` of the `initialize` instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitParams {
    pub updater: Pubkey,
    pub level_size_base_atoms: u64,
    pub num_levels: u8,
    pub half_spread_bps: u16,
    pub level_step_bps: u16,
    pub max_age_slots: u64,
    pub max_move_bps: u16,
}

/// `MarketParams` of the `set_params` instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketParams {
    pub level_size_base_atoms: u64,
    pub num_levels: u8,
    pub half_spread_bps: u16,
    pub level_step_bps: u16,
    pub max_age_slots: u64,
    pub max_move_bps: u16,
}

impl InitParams {
    fn market_params(&self) -> MarketParams {
        MarketParams {
            level_size_base_atoms: self.level_size_base_atoms,
            num_levels: self.num_levels,
            half_spread_bps: self.half_spread_bps,
            level_step_bps: self.level_step_bps,
            max_age_slots: self.max_age_slots,
            max_move_bps: self.max_move_bps,
        }
    }
}

impl MarketParams {
    /// Borsh bytes of the struct (23 bytes).
    fn to_borsh(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(23);
        out.extend_from_slice(&self.level_size_base_atoms.to_le_bytes());
        out.push(self.num_levels);
        out.extend_from_slice(&self.half_spread_bps.to_le_bytes());
        out.extend_from_slice(&self.level_step_bps.to_le_bytes());
        out.extend_from_slice(&self.max_age_slots.to_le_bytes());
        out.extend_from_slice(&self.max_move_bps.to_le_bytes());
        out
    }
}

/// The three keys that identify a market and everything derived from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketKeys {
    pub admin: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
}

impl MarketKeys {
    pub fn market(&self) -> Pubkey {
        market_pda(&self.admin, &self.base_mint, &self.quote_mint).0
    }

    pub fn base_vault(&self) -> Pubkey {
        vault_pda(&self.market(), &self.base_mint).0
    }

    pub fn quote_vault(&self) -> Pubkey {
        vault_pda(&self.market(), &self.quote_mint).0
    }
}

fn ix(accounts: Vec<AccountMeta>, disc: [u8; 8], args: &[u8]) -> Instruction {
    let mut data = Vec::with_capacity(8 + args.len());
    data.extend_from_slice(&disc);
    data.extend_from_slice(args);
    Instruction {
        program_id: PROGRAM_ID,
        accounts,
        data,
    }
}

/// `initialize`: the admin pays, becomes admin, and the market and both vaults are created.
pub fn initialize(keys: &MarketKeys, params: &InitParams) -> Instruction {
    let mut args = Vec::with_capacity(55);
    args.extend_from_slice(params.updater.as_ref());
    args.extend_from_slice(&params.market_params().to_borsh());
    ix(
        vec![
            AccountMeta::new(keys.admin, true),
            AccountMeta::new_readonly(keys.base_mint, false),
            AccountMeta::new_readonly(keys.quote_mint, false),
            AccountMeta::new(keys.market(), false),
            AccountMeta::new(keys.base_vault(), false),
            AccountMeta::new(keys.quote_vault(), false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        INITIALIZE_DISCRIMINATOR,
        &args,
    )
}

fn funds(
    disc: [u8; 8],
    keys: &MarketKeys,
    mint: &Pubkey,
    admin_token: &Pubkey,
    amount: u64,
) -> Instruction {
    let market = keys.market();
    ix(
        vec![
            AccountMeta::new_readonly(keys.admin, true),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(vault_pda(&market, mint).0, false),
            AccountMeta::new(*admin_token, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        disc,
        &amount.to_le_bytes(),
    )
}

/// `deposit` of `amount` atoms of `mint` (base or quote) from the admin's token account.
pub fn deposit(keys: &MarketKeys, mint: &Pubkey, admin_token: &Pubkey, amount: u64) -> Instruction {
    funds(DEPOSIT_DISCRIMINATOR, keys, mint, admin_token, amount)
}

/// `withdraw` of `amount` atoms of `mint` (base or quote) to the admin's token account.
pub fn withdraw(
    keys: &MarketKeys,
    mint: &Pubkey,
    admin_token: &Pubkey,
    amount: u64,
) -> Instruction {
    funds(WITHDRAW_DISCRIMINATOR, keys, mint, admin_token, amount)
}

fn admin_only(disc: [u8; 8], admin: &Pubkey, market: &Pubkey, args: &[u8]) -> Instruction {
    ix(
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*market, false),
        ],
        disc,
        args,
    )
}

/// `set_params` (admin).
pub fn set_params(admin: &Pubkey, market: &Pubkey, params: &MarketParams) -> Instruction {
    admin_only(SET_PARAMS_DISCRIMINATOR, admin, market, &params.to_borsh())
}

/// `set_updater` (admin).
pub fn set_updater(admin: &Pubkey, market: &Pubkey, new_updater: &Pubkey) -> Instruction {
    admin_only(
        SET_UPDATER_DISCRIMINATOR,
        admin,
        market,
        new_updater.as_ref(),
    )
}

/// `set_paused` (admin).
pub fn set_paused(admin: &Pubkey, market: &Pubkey, paused: bool) -> Instruction {
    admin_only(SET_PAUSED_DISCRIMINATOR, admin, market, &[u8::from(paused)])
}

/// `force_price` (admin): sets the mid with no move limit. Scaled by `PRICE_SCALE`.
pub fn force_price(admin: &Pubkey, market: &Pubkey, mid: u64) -> Instruction {
    admin_only(FORCE_PRICE_DISCRIMINATOR, admin, market, &mid.to_le_bytes())
}

/// `update_price` (updater): the only instruction the hot key may send.
pub fn update_price(updater: &Pubkey, market: &Pubkey, mid: u64) -> Instruction {
    ix(
        vec![
            AccountMeta::new_readonly(*updater, true),
            AccountMeta::new(*market, false),
        ],
        UPDATE_PRICE_DISCRIMINATOR,
        &mid.to_le_bytes(),
    )
}

/// Swap instruction data: discriminator + Borsh(side, `amount_in`, `min_out`).
pub fn swap_data(side: Side, amount_in: u64, min_out: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&SWAP_DISCRIMINATOR);
    data.push(side_byte(side));
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    data
}

/// The nine swap accounts in program order. Also used by the Jupiter adapter.
pub fn swap_accounts(
    trader: &Pubkey,
    market: &Pubkey,
    state: &MarketState,
    trader_base: &Pubkey,
    trader_quote: &Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(*trader, true),
        AccountMeta::new(*market, false),
        AccountMeta::new_readonly(Pubkey::new_from_array(state.base_mint), false),
        AccountMeta::new_readonly(Pubkey::new_from_array(state.quote_mint), false),
        AccountMeta::new(Pubkey::new_from_array(state.base_vault), false),
        AccountMeta::new(Pubkey::new_from_array(state.quote_vault), false),
        AccountMeta::new(*trader_base, false),
        AccountMeta::new(*trader_quote, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
    ]
}

/// Arguments of the `swap` instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapArgs {
    pub side: Side,
    pub amount_in: u64,
    pub min_out: u64,
}

/// `swap` (any trader). Vaults and mints come from the decoded market `state`.
pub fn swap(
    trader: &Pubkey,
    market: &Pubkey,
    state: &MarketState,
    trader_base: &Pubkey,
    trader_quote: &Pubkey,
    args: SwapArgs,
) -> Instruction {
    Instruction {
        program_id: PROGRAM_ID,
        accounts: swap_accounts(trader, market, state, trader_base, trader_quote),
        data: swap_data(args.side, args.amount_in, args.min_out),
    }
}

/// Names of the program's custom errors, in declaration order. Anchor numbers them from 6000.
const ERROR_NAMES: [&str; 17] = [
    "Unauthorized",
    "InvalidParams",
    "InvalidMint",
    "InvalidVault",
    "MathOverflow",
    "ZeroInput",
    "ZeroOutput",
    "InsufficientDepth",
    "InvalidUpdater",
    "NotInitializedPrice",
    "PriceMoveTooLarge",
    "Paused",
    "SlotRegression",
    "AlreadyUpdatedThisSlot",
    "StalePrice",
    "SlippageExceeded",
    "InsufficientVaultBalance",
];

/// Name of a custom program error code (6000 and up), if it is one of the program's.
pub fn error_name(code: u32) -> Option<&'static str> {
    let index = usize::try_from(code.checked_sub(6000)?).ok()?;
    ERROR_NAMES.get(index).copied()
}

/// `sha256("event:SwapEvent")[..8]`.
pub const SWAP_EVENT_DISCRIMINATOR: [u8; 8] = [0x40, 0xc6, 0xcd, 0xe8, 0x26, 0x08, 0x71, 0xe2];

/// The program's `SwapEvent`, as emitted in the `Program data:` log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapEvent {
    pub market: Pubkey,
    pub trader: Pubkey,
    pub side: Side,
    pub amount_in: u64,
    pub amount_out: u64,
    pub consumed_after: u64,
    pub mid_price: u64,
    pub slot: u64,
}

/// Decodes the bytes of a `Program data:` log line (discriminator + Borsh `SwapEvent`).
pub fn decode_swap_event(data: &[u8]) -> Option<SwapEvent> {
    let body = data.strip_prefix(&SWAP_EVENT_DISCRIMINATOR)?;
    if body.len() != 32 + 32 + 1 + 5 * 8 {
        return None;
    }
    let key = |at: usize| -> Option<Pubkey> {
        Some(Pubkey::new_from_array(
            body.get(at..at + 32)?.try_into().ok()?,
        ))
    };
    let num = |at: usize| -> Option<u64> {
        Some(u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?))
    };
    let side = match body.get(64)? {
        0 => Side::BuyBase,
        1 => Side::SellBase,
        _ => return None,
    };
    Some(SwapEvent {
        market: key(0)?,
        trader: key(32)?,
        side,
        amount_in: num(65)?,
        amount_out: num(73)?,
        consumed_after: num(81)?,
        mid_price: num(89)?,
        slot: num(97)?,
    })
}
