//! `PropAMM`: full Pinocchio port of the Anchor program (CU ladder rung 3).
//!
//! Same account bytes (8-byte Anchor `Market` discriminator + `MarketState`), PDAs, instruction
//! semantics, error codes and check order as `programs/propamm`. Every Anchor constraint has an
//! explicit equivalent here; the mapping is in `docs/design-onchain.md` (CU ladder).
//!
//! Instruction data: a 1-byte tag ([`tag`]) followed by the same fixed little-endian fields
//! that Borsh writes for the Anchor arguments.
#![no_std]
#![deny(
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::expect_used
)]

mod checks;
pub mod error;
mod instructions;
mod rent;
pub mod state;

use pinocchio::{AccountView, Address, ProgramResult};

use error::Error;

solana_address::declare_id!("HTsHg7iAkb5YtrV9in5537j8iijh1VHrk1t2dFMpB287");

/// Most accounts any instruction takes (swap). Extra accounts are ignored, like Anchor's
/// remaining accounts.
const MAX_ACCOUNTS: usize = 9;

pinocchio::program_entrypoint!(process_instruction, MAX_ACCOUNTS);
pinocchio::no_allocator!();
pinocchio::nostd_panic_handler!();

/// Instruction tags (first data byte).
pub mod tag {
    pub const INITIALIZE: u8 = 0;
    pub const DEPOSIT: u8 = 1;
    pub const WITHDRAW: u8 = 2;
    pub const SET_PARAMS: u8 = 3;
    pub const SET_UPDATER: u8 = 4;
    pub const SET_PAUSED: u8 = 5;
    pub const UPDATE_PRICE: u8 = 6;
    pub const SWAP: u8 = 7;
    pub const FORCE_PRICE: u8 = 8;
}

/// Dispatch on the tag. Unknown or missing tags fail like Anchor's dispatch without a fallback.
pub fn process_instruction(
    program_id: &Address,
    accounts: &mut [AccountView],
    data: &[u8],
) -> ProgramResult {
    if program_id != &ID {
        return Err(Error::DeclaredProgramIdMismatch.into());
    }
    let Some((&tag, args)) = data.split_first() else {
        return Err(Error::InstructionFallbackNotFound.into());
    };
    match tag {
        tag::INITIALIZE => instructions::initialize::process(accounts, args),
        tag::DEPOSIT => instructions::funds::deposit(accounts, args),
        tag::WITHDRAW => instructions::funds::withdraw(accounts, args),
        tag::SET_PARAMS => instructions::admin::set_params(accounts, args),
        tag::SET_UPDATER => instructions::admin::set_updater(accounts, args),
        tag::SET_PAUSED => instructions::admin::set_paused(accounts, args),
        tag::UPDATE_PRICE => instructions::price::update_price(accounts, args),
        tag::SWAP => instructions::swap::swap(accounts, args),
        tag::FORCE_PRICE => instructions::admin::force_price(accounts, args),
        _ => Err(Error::InstructionFallbackNotFound.into()),
    }
}
