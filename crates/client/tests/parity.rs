//! Builder output must equal the Anchor-generated instruction data and account metas.

use anchor_lang::{InstructionData, ToAccountMetas, system_program};
use propamm::instruction as ix;
use propamm::instructions::{InitParams as AnchorInit, SwapSide};
use propamm::state::MarketParams as AnchorParams;
use propamm_client as c;
use propamm_pricing::{MARKET_STATE_SIZE, MarketState, Side};
use sha2::{Digest, Sha256};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;

fn key(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}

fn disc(preimage: &str) -> [u8; 8] {
    Sha256::digest(preimage.as_bytes())[..8].try_into().unwrap()
}

/// (key, signer, writable) triples, independent of the `AccountMeta` type's crate version.
fn metas(accounts: &[solana_instruction::AccountMeta]) -> Vec<(Pubkey, bool, bool)> {
    accounts
        .iter()
        .map(|m| (m.pubkey, m.is_signer, m.is_writable))
        .collect()
}

fn assert_same(
    ours: &Instruction,
    data: &[u8],
    anchor_metas: &[anchor_lang::prelude::AccountMeta],
) {
    assert_eq!(ours.program_id, propamm::ID);
    assert_eq!(ours.data, data);
    let theirs: Vec<_> = anchor_metas
        .iter()
        .map(|m| (m.pubkey, m.is_signer, m.is_writable))
        .collect();
    assert_eq!(metas(&ours.accounts), theirs);
}

fn keys() -> c::MarketKeys {
    c::MarketKeys {
        admin: key(1),
        base_mint: key(2),
        quote_mint: key(3),
    }
}

#[test]
fn constants_match_sha256() {
    assert_eq!(c::PROGRAM_ID, propamm::ID);
    assert_eq!(c::MARKET_DISCRIMINATOR, disc("account:Market"));
    for (got, name) in [
        (c::INITIALIZE_DISCRIMINATOR, "initialize"),
        (c::DEPOSIT_DISCRIMINATOR, "deposit"),
        (c::WITHDRAW_DISCRIMINATOR, "withdraw"),
        (c::SET_PARAMS_DISCRIMINATOR, "set_params"),
        (c::SET_UPDATER_DISCRIMINATOR, "set_updater"),
        (c::SET_PAUSED_DISCRIMINATOR, "set_paused"),
        (c::FORCE_PRICE_DISCRIMINATOR, "force_price"),
        (c::UPDATE_PRICE_DISCRIMINATOR, "update_price"),
        (c::SWAP_DISCRIMINATOR, "swap"),
    ] {
        assert_eq!(got, disc(&format!("global:{name}")), "{name}");
    }
}

#[test]
fn pdas_match_the_program_seeds() {
    let k = keys();
    let market = k.market();
    let (expected, _) = Pubkey::find_program_address(
        &[
            b"market",
            k.admin.as_ref(),
            k.base_mint.as_ref(),
            k.quote_mint.as_ref(),
        ],
        &propamm::ID,
    );
    assert_eq!(market, expected);
    let (vault, _) = Pubkey::find_program_address(
        &[b"vault", market.as_ref(), k.base_mint.as_ref()],
        &propamm::ID,
    );
    assert_eq!(k.base_vault(), vault);
    assert_ne!(k.base_vault(), k.quote_vault());
}

#[test]
fn initialize_matches() {
    let k = keys();
    let ours = c::initialize(
        &k,
        &c::InitParams {
            updater: key(9),
            level_size_base_atoms: 10_000_000_000,
            num_levels: 8,
            half_spread_bps: 5,
            level_step_bps: 1,
            max_age_slots: 20,
            max_move_bps: 100,
        },
    );
    let data = ix::Initialize {
        params: AnchorInit {
            updater: key(9),
            level_size_base_atoms: 10_000_000_000,
            num_levels: 8,
            half_spread_bps: 5,
            level_step_bps: 1,
            max_age_slots: 20,
            max_move_bps: 100,
        },
    }
    .data();
    let accounts = propamm::accounts::Initialize {
        admin: k.admin,
        base_mint: k.base_mint,
        quote_mint: k.quote_mint,
        market: k.market(),
        base_vault: k.base_vault(),
        quote_vault: k.quote_vault(),
        token_program: c::TOKEN_PROGRAM_ID,
        system_program: system_program::ID,
    }
    .to_account_metas(None);
    assert_same(&ours, &data, &accounts);
}

#[test]
fn deposit_and_withdraw_match() {
    let k = keys();
    let admin_token = key(7);
    let accounts = propamm::accounts::Funds {
        admin: k.admin,
        market: k.market(),
        mint: k.quote_mint,
        vault: k.quote_vault(),
        admin_token,
        token_program: c::TOKEN_PROGRAM_ID,
    }
    .to_account_metas(None);
    assert_same(
        &c::deposit(&k, &k.quote_mint, &admin_token, 123),
        &ix::Deposit { amount: 123 }.data(),
        &accounts,
    );
    assert_same(
        &c::withdraw(&k, &k.quote_mint, &admin_token, 456),
        &ix::Withdraw { amount: 456 }.data(),
        &accounts,
    );
}

#[test]
fn admin_instructions_match() {
    let k = keys();
    let market = k.market();
    let accounts = propamm::accounts::AdminOnly {
        admin: k.admin,
        market,
    }
    .to_account_metas(None);
    assert_same(
        &c::set_params(
            &k.admin,
            &market,
            &c::MarketParams {
                level_size_base_atoms: 5,
                num_levels: 3,
                half_spread_bps: 7,
                level_step_bps: 2,
                max_age_slots: 40,
                max_move_bps: 50,
            },
        ),
        &ix::SetParams {
            params: AnchorParams {
                level_size_base_atoms: 5,
                num_levels: 3,
                half_spread_bps: 7,
                level_step_bps: 2,
                max_age_slots: 40,
                max_move_bps: 50,
            },
        }
        .data(),
        &accounts,
    );
    assert_same(
        &c::set_updater(&k.admin, &market, &key(8)),
        &ix::SetUpdater {
            new_updater: key(8),
        }
        .data(),
        &accounts,
    );
    for paused in [true, false] {
        assert_same(
            &c::set_paused(&k.admin, &market, paused),
            &ix::SetPaused { paused }.data(),
            &accounts,
        );
    }
    assert_same(
        &c::force_price(&k.admin, &market, 200_000_000_000),
        &ix::ForcePrice {
            mid: 200_000_000_000,
        }
        .data(),
        &accounts,
    );
}

#[test]
fn update_price_matches() {
    let k = keys();
    let accounts = propamm::accounts::UpdatePrice {
        updater: key(9),
        market: k.market(),
    }
    .to_account_metas(None);
    assert_same(
        &c::update_price(&key(9), &k.market(), 201_000_000_000),
        &ix::UpdatePrice {
            mid: 201_000_000_000,
        }
        .data(),
        &accounts,
    );
}

#[test]
fn swap_matches() {
    let k = keys();
    let mut state: MarketState = bytemuck::Zeroable::zeroed();
    state.base_mint = k.base_mint.to_bytes();
    state.quote_mint = k.quote_mint.to_bytes();
    state.base_vault = k.base_vault().to_bytes();
    state.quote_vault = k.quote_vault().to_bytes();
    let accounts = propamm::accounts::Swap {
        trader: key(6),
        market: k.market(),
        base_mint: k.base_mint,
        quote_mint: k.quote_mint,
        base_vault: k.base_vault(),
        quote_vault: k.quote_vault(),
        trader_base: key(4),
        trader_quote: key(5),
        token_program: c::TOKEN_PROGRAM_ID,
    }
    .to_account_metas(None);
    for (side, anchor_side) in [
        (Side::BuyBase, SwapSide::BuyBase),
        (Side::SellBase, SwapSide::SellBase),
    ] {
        let ours = c::swap(
            &key(6),
            &k.market(),
            &state,
            &key(4),
            &key(5),
            c::SwapArgs {
                side,
                amount_in: 1_000,
                min_out: 900,
            },
        );
        let data = ix::Swap {
            side: anchor_side,
            amount_in: 1_000,
            min_out: 900,
        }
        .data();
        assert_same(&ours, &data, &accounts);
    }
}

#[test]
fn decode_market_checks_size_and_discriminator() {
    let mut data = vec![0_u8; c::MARKET_ACCOUNT_SIZE];
    assert_eq!(
        c::decode_market(&data),
        Err(c::DecodeError::WrongDiscriminator)
    );
    data[..8].copy_from_slice(&c::MARKET_DISCRIMINATOR);
    data[8 + 192..8 + 200].copy_from_slice(&42_u64.to_le_bytes());
    assert_eq!(c::decode_market(&data).unwrap().mid_price, 42);
    assert_eq!(
        c::decode_market(&data[..data.len() - 1]),
        Err(c::DecodeError::WrongSize {
            got: 8 + MARKET_STATE_SIZE - 1
        })
    );
}

#[test]
fn token_amount_reads_offset_64() {
    let mut data = vec![0_u8; 165];
    data[64..72].copy_from_slice(&777_u64.to_le_bytes());
    assert_eq!(c::token_amount(&data), Some(777));
    assert_eq!(c::token_amount(&data[..70]), None);
}

#[test]
fn error_names_match_the_program_enum() {
    use propamm::error::PropAmmError as E;
    for (e, name) in [
        (E::Unauthorized, "Unauthorized"),
        (E::PriceMoveTooLarge, "PriceMoveTooLarge"),
        (E::AlreadyUpdatedThisSlot, "AlreadyUpdatedThisSlot"),
        (E::StalePrice, "StalePrice"),
        (E::InsufficientVaultBalance, "InsufficientVaultBalance"),
    ] {
        assert_eq!(c::error_name(e.into()), Some(name));
    }
    assert_eq!(c::error_name(5999), None);
    assert_eq!(c::error_name(6017), None);
}

#[test]
fn swap_event_decodes_the_anchor_encoding() {
    use anchor_lang::Discriminator;
    use propamm::instructions::SwapEvent as AnchorEvent;
    assert_eq!(c::SWAP_EVENT_DISCRIMINATOR, AnchorEvent::DISCRIMINATOR);
    assert_eq!(c::SWAP_EVENT_DISCRIMINATOR, disc("event:SwapEvent"));
    let event = AnchorEvent {
        market: key(1),
        trader: key(2),
        side: SwapSide::SellBase,
        amount_in: 11,
        amount_out: 22,
        consumed_after: 33,
        mid_price: 44,
        slot: 55,
    };
    let decoded = c::decode_swap_event(&anchor_lang::Event::data(&event)).unwrap();
    assert_eq!(decoded.side, Side::SellBase);
    assert_eq!((decoded.market, decoded.trader), (key(1), key(2)));
    assert_eq!(
        [
            decoded.amount_in,
            decoded.amount_out,
            decoded.consumed_after,
            decoded.mid_price,
            decoded.slot
        ],
        [11, 22, 33, 44, 55]
    );
    assert_eq!(c::decode_swap_event(&[0; 5]), None);
}
