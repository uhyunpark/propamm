//! Manual tools: local setup and the admin / trader commands used around the bot.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use propamm_client::{
    InitParams, MarketKeys, PROGRAM_ID, SwapArgs, TOKEN_PROGRAM_ID, decode_market,
    decode_swap_event, deposit, error_name, force_price, initialize, set_paused, swap,
    token_amount,
};
use propamm_pricing::{Side, is_fresh};
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use tracing::info;

use crate::chain::{
    MINT_SIZE, Rpc, RpcError, ata, create_account, create_ata_idempotent, initialize_mint2,
    mint_to, transfer,
};
use crate::config::{NetConfig, read_keypair, write_keypair};
use crate::connector::now_unix_ns;
use crate::live::{Feeds, LiveFair};
use crate::publish::{Decimals, atoms_to_ui, price_to_usd, ui_to_atoms, usd_to_price};
use crate::pyth;

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
pub const BASE_DECIMALS: u8 = 9;
pub const QUOTE_DECIMALS: u8 = 6;
/// Atoms per whole token: `10^BASE_DECIMALS` and `10^QUOTE_DECIMALS`.
const BASE_UNIT: u64 = 1_000_000_000;
const QUOTE_UNIT: u64 = 1_000_000;
const DECIMALS: Decimals = Decimals {
    base: BASE_DECIMALS,
    quote: QUOTE_DECIMALS,
};
/// Demo market parameters (half spread 5 bps: the markout evaluation showed 2 bps loses).
const LEVEL_SIZE_SOL: u64 = 10;
const NUM_LEVELS: u8 = 8;
const HALF_SPREAD_BPS: u16 = 5;
const LEVEL_STEP_BPS: u16 = 1;
const MAX_AGE_SLOTS: u64 = 20;
const MAX_MOVE_BPS: u16 = 100;
/// Inventory minted per owner: (SOL, USDC) whole tokens.
const ADMIN_INVENTORY: (u64, u64) = (1_000, 200_000);
const TRADER_INVENTORY: (u64, u64) = (100, 20_000);
/// Smaller devnet inventory (the mints are our own; only SOL for rent and fees matters).
const DEVNET_ADMIN_INVENTORY: (u64, u64) = (200, 40_000);
const DEVNET_TRADER_INVENTORY: (u64, u64) = (20, 4_000);
const FAIR_TIMEOUT: Duration = Duration::from_mins(1);

/// Where `setup` creates the market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Cluster {
    /// A local test validator; keys are created and airdropped.
    Local,
    /// Public devnet; the admin key must already hold SOL (no airdrops).
    Devnet,
}

impl Cluster {
    fn default_rpc(self) -> &'static str {
        match self {
            Self::Local => "http://127.0.0.1:8899",
            Self::Devnet => "https://api.devnet.solana.com",
        }
    }

    fn keys_subdir(self) -> &'static str {
        match self {
            Self::Local => "propamm-local",
            Self::Devnet => "propamm-devnet",
        }
    }
}

/// Token account size (SPL Token).
const TOKEN_ACCOUNT_SIZE: u64 = 165;
/// Devnet SOL kept on the updater (fees for `update_price`, about 5,000 lamports each).
const DEVNET_UPDATER_SOL: u64 = LAMPORTS_PER_SOL / 2;
/// Devnet SOL for the CLI trader (swap fees).
const DEVNET_TRADER_SOL: u64 = LAMPORTS_PER_SOL / 20;
/// Margin for the setup transactions' own fees.
const DEVNET_FEE_BUFFER: u64 = LAMPORTS_PER_SOL / 100;
pub const DEVNET_FAUCET_URL: &str = "https://faucet.solana.com";

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub cluster: Cluster,
    /// Default: the cluster's public/local RPC.
    pub rpc: Option<String>,
    pub ws: Option<String>,
    pub out: PathBuf,
    /// Required on devnet; locally the admin key is created in `keys_dir` when this is unset.
    pub admin_keypair: Option<PathBuf>,
    pub keys_dir: Option<PathBuf>,
    /// Skip the price lookup and force this price.
    pub fair_usd: Option<f64>,
    pub pyth_url: String,
    /// Resume: reuse these mints (both or neither) instead of creating new ones. Existing
    /// accounts are detected and skipped; inventory and deposits are topped up to the target.
    pub base_mint: Option<Pubkey>,
    pub quote_mint: Option<Pubkey>,
}

fn default_keys_dir(cluster: Cluster) -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set; pass --keys-dir")?;
    Ok(PathBuf::from(home)
        .join(".config/solana")
        .join(cluster.keys_subdir()))
}

/// Lamports to top up so that `balance` ends at `target`; nothing while it is above half.
fn topup(balance: u64, target: u64) -> u64 {
    if balance >= target / 2 {
        0
    } else {
        target.saturating_sub(balance)
    }
}

/// Lamports the admin needs on devnet: rent for 2 mints, 4 ATAs + 2 vaults, the market, the
/// top-ups for updater and trader, and a fee margin.
fn devnet_requirement(mint_rent: u64, token_rent: u64, market_rent: u64, topups: u64) -> u64 {
    2 * mint_rent + 6 * token_rent + market_rent + topups + DEVNET_FEE_BUFFER
}

fn sol(lamports: u64) -> String {
    // Display only: f64 is exact enough for a balance shown with 4 decimals.
    #[allow(clippy::cast_precision_loss)]
    let v = lamports as f64 / LAMPORTS_PER_SOL as f64;
    format!("{v:.4} SOL")
}

/// Devnet has no airdrop here: check the admin balance against what setup needs, then top up
/// the updater and trader from the admin.
async fn fund_devnet(
    rpc: &Rpc,
    admin: &Keypair,
    updater: &Pubkey,
    trader: &Pubkey,
    creating_mints: bool,
    creating_market: bool,
) -> anyhow::Result<()> {
    let mint_rent = if creating_mints {
        rpc.rent_exempt_minimum(MINT_SIZE).await?
    } else {
        0
    };
    let token_rent = rpc.rent_exempt_minimum(TOKEN_ACCOUNT_SIZE).await?;
    let market_rent = if creating_market {
        rpc.rent_exempt_minimum(propamm_client::MARKET_ACCOUNT_SIZE as u64)
            .await?
    } else {
        0
    };
    let updater_topup = topup(rpc.balance(updater).await?, DEVNET_UPDATER_SOL);
    let trader_topup = topup(rpc.balance(trader).await?, DEVNET_TRADER_SOL);
    let needed = devnet_requirement(
        mint_rent,
        token_rent,
        market_rent,
        updater_topup + trader_topup,
    );
    let have = rpc.balance(&admin.pubkey()).await?;
    info!(
        admin = %admin.pubkey(),
        have = sol(have),
        needed = sol(needed),
        "devnet funding check"
    );
    if have < needed {
        bail!(
            "admin {} has {} on devnet but setup needs about {} (short by {}).\n\
             Get devnet SOL at {DEVNET_FAUCET_URL} (paste the admin address), then run this again.",
            admin.pubkey(),
            sol(have),
            sol(needed),
            sol(needed - have)
        );
    }
    let mut ixs = Vec::new();
    if updater_topup > 0 {
        ixs.push(transfer(&admin.pubkey(), updater, updater_topup));
    }
    if trader_topup > 0 {
        ixs.push(transfer(&admin.pubkey(), trader, trader_topup));
    }
    if !ixs.is_empty() {
        send(rpc, &ixs, admin, &[]).await?;
        info!(
            updater = sol(updater_topup),
            trader = sol(trader_topup),
            "funded updater and trader from the admin"
        );
    }
    Ok(())
}

/// Loads `dir/<name>.json`, creating the keypair when it is missing.
fn load_or_create(dir: &Path, name: &str) -> anyhow::Result<(Keypair, PathBuf)> {
    let path = dir.join(format!("{name}.json"));
    if !path.exists() {
        write_keypair(&path, &Keypair::new())?;
        info!(path = %path.display(), "created keypair");
    }
    Ok((read_keypair(&path)?, path))
}

/// `http://host:8899` -> `ws://host:8900` (the validator's default pairing).
fn default_ws(rpc: &str) -> String {
    let ws = rpc
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    match ws
        .rsplit_once(':')
        .map(|(h, p)| (h, p.trim_end_matches('/').parse::<u16>()))
    {
        Some((host, Ok(port))) if host.contains("//") => {
            format!("{host}:{}", port.saturating_add(1))
        }
        _ => ws,
    }
}

async fn airdrop_to(rpc: &Rpc, key: &Pubkey, target_sol: u64) -> anyhow::Result<()> {
    if rpc.balance(key).await? >= target_sol * LAMPORTS_PER_SOL / 2 {
        return Ok(());
    }
    rpc.request_airdrop(key, target_sol * LAMPORTS_PER_SOL)
        .await?;
    for _ in 0..60 {
        if rpc.balance(key).await? >= target_sol * LAMPORTS_PER_SOL / 2 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("airdrop to {key} did not arrive")
}

/// First fair price: Hermes if reachable, else a short live-feed run.
async fn first_price(opts: &SetupOptions) -> anyhow::Result<f64> {
    if let Some(p) = opts.fair_usd {
        return Ok(p);
    }
    match pyth::fetch_once(&opts.pyth_url).await {
        Ok(p) => {
            info!(price = p.price, "first price from Hermes");
            return Ok(p.price);
        }
        Err(e) => info!(error = %e, "Hermes unavailable, running the live feeds for a first price"),
    }
    let mut feeds = Feeds::spawn()?;
    let mut live = LiveFair::new();
    let deadline = tokio::time::Instant::now() + FAIR_TIMEOUT;
    let mut warm_until = None;
    let result = loop {
        let Ok(msg) = tokio::time::timeout_at(deadline, feeds.rx.recv()).await else {
            break Err(anyhow::anyhow!(
                "no fair value within {FAIR_TIMEOUT:?}; pass --fair-usd"
            ));
        };
        let Some(msg) = msg else {
            break Err(anyhow::anyhow!("feeds ended"));
        };
        live.ingest(&msg);
        if let Some(f) = live.fair(now_unix_ns()) {
            // Give the EMAs a few seconds of data before trusting the first value.
            let warm =
                *warm_until.get_or_insert(tokio::time::Instant::now() + Duration::from_secs(5));
            if tokio::time::Instant::now() >= warm {
                info!(
                    price = f.fair,
                    sources = f.live_sources,
                    "first price from the live feeds"
                );
                break Ok(f.fair);
            }
        }
    };
    feeds.shutdown().await;
    result
}

// A linear script; splitting it would only scatter the steps.
#[allow(clippy::too_many_lines)]
pub async fn setup(opts: SetupOptions) -> anyhow::Result<()> {
    let rpc_url = opts
        .rpc
        .clone()
        .unwrap_or_else(|| opts.cluster.default_rpc().to_owned());
    let keys_dir = match &opts.keys_dir {
        Some(d) => d.clone(),
        None => default_keys_dir(opts.cluster)?,
    };
    let (admin, admin_path) = match (&opts.admin_keypair, opts.cluster) {
        (Some(path), _) => (read_keypair(path)?, path.clone()),
        (None, Cluster::Local) => load_or_create(&keys_dir, "admin")?,
        (None, Cluster::Devnet) => bail!("--admin-keypair is required on devnet"),
    };
    let (updater, updater_path) = load_or_create(&keys_dir, "updater")?;
    let (trader, trader_path) = load_or_create(&keys_dir, "trader")?;
    let rpc = Rpc::new(&rpc_url)?;

    rpc.get_account(&PROGRAM_ID)
        .await
        .context("RPC unreachable")?
        .context(
            "propamm program is not deployed on this cluster \
             (scripts/localnet.sh or scripts/devnet-deploy.sh)",
        )?;
    let resume = match (opts.base_mint, opts.quote_mint) {
        (Some(b), Some(q)) => Some((b, q)),
        (None, None) => None,
        _ => bail!("pass both --base-mint and --quote-mint, or neither"),
    };
    let creating_mints = resume.is_none();
    let new_mints = [Keypair::new(), Keypair::new()];
    let (bm, qm) = resume.unwrap_or_else(|| (new_mints[0].pubkey(), new_mints[1].pubkey()));
    let keys = MarketKeys {
        admin: admin.pubkey(),
        base_mint: bm,
        quote_mint: qm,
    };
    let market_exists = !creating_mints && rpc.get_account(&keys.market()).await?.is_some();
    match opts.cluster {
        Cluster::Local => {
            airdrop_to(&rpc, &admin.pubkey(), 100).await?;
            airdrop_to(&rpc, &updater.pubkey(), 10).await?;
            airdrop_to(&rpc, &trader.pubkey(), 10).await?;
            info!("airdrops done");
        }
        Cluster::Devnet => {
            fund_devnet(
                &rpc,
                &admin,
                &updater.pubkey(),
                &trader.pubkey(),
                creating_mints,
                !market_exists,
            )
            .await?;
        }
    }

    // Mints.
    if creating_mints {
        let rent = rpc.rent_exempt_minimum(MINT_SIZE).await?;
        let mut ixs = Vec::new();
        for (mint, decimals) in new_mints.iter().zip([BASE_DECIMALS, QUOTE_DECIMALS]) {
            ixs.push(create_account(
                &admin.pubkey(),
                &mint.pubkey(),
                rent,
                MINT_SIZE,
                &TOKEN_PROGRAM_ID,
            ));
            ixs.push(initialize_mint2(&mint.pubkey(), decimals, &admin.pubkey()));
        }
        send(&rpc, &ixs, &admin, &[&new_mints[0], &new_mints[1]]).await?;
        info!(base_mint = %bm, quote_mint = %qm, "mints created (SOL 9 decimals, USDC 6 decimals)");
    } else {
        let (_, found) = rpc.get_accounts(&[bm, qm]).await?;
        if found.iter().any(Option::is_none) {
            bail!("--base-mint/--quote-mint do not both exist on this cluster");
        }
        info!(base_mint = %bm, quote_mint = %qm, "reusing existing mints");
    }

    // Token accounts and inventory (minted only up to the target).
    let (admin_inv, trader_inv) = match opts.cluster {
        Cluster::Local => (ADMIN_INVENTORY, TRADER_INVENTORY),
        Cluster::Devnet => (DEVNET_ADMIN_INVENTORY, DEVNET_TRADER_INVENTORY),
    };
    let mut ixs = Vec::new();
    for (owner, (sol, usdc)) in [(admin.pubkey(), admin_inv), (trader.pubkey(), trader_inv)] {
        ixs.push(create_ata_idempotent(&admin.pubkey(), &owner, &bm));
        ixs.push(create_ata_idempotent(&admin.pubkey(), &owner, &qm));
        for (mint, units, unit, vault) in [
            (bm, sol, BASE_UNIT, keys.base_vault()),
            (qm, usdc, QUOTE_UNIT, keys.quote_vault()),
        ] {
            let mut have = token_balance(&rpc, &ata(&owner, &mint)).await?;
            if owner == admin.pubkey() {
                // Admin inventory already deposited counts toward the target.
                have += token_balance(&rpc, &vault).await?;
            }
            let missing = (units * unit).saturating_sub(have);
            if missing > 0 {
                ixs.push(mint_to(
                    &mint,
                    &ata(&owner, &mint),
                    &admin.pubkey(),
                    missing,
                ));
            }
        }
    }
    send(&rpc, &ixs, &admin, &[]).await?;

    // Market.
    if market_exists {
        info!(market = %keys.market(), "market exists, skipping initialize");
    } else {
        let init = initialize(
            &keys,
            &InitParams {
                updater: updater.pubkey(),
                level_size_base_atoms: LEVEL_SIZE_SOL * BASE_UNIT,
                num_levels: NUM_LEVELS,
                half_spread_bps: HALF_SPREAD_BPS,
                level_step_bps: LEVEL_STEP_BPS,
                max_age_slots: MAX_AGE_SLOTS,
                max_move_bps: MAX_MOVE_BPS,
            },
        );
        send(&rpc, &[init], &admin, &[]).await?;
        info!(market = %keys.market(), "market initialized");
    }

    let (admin_base, admin_quote) = (ata(&admin.pubkey(), &bm), ata(&admin.pubkey(), &qm));
    let mut deposits = Vec::new();
    for (mint, admin_ata, vault, target) in [
        (bm, admin_base, keys.base_vault(), admin_inv.0 * BASE_UNIT),
        (
            qm,
            admin_quote,
            keys.quote_vault(),
            admin_inv.1 * QUOTE_UNIT,
        ),
    ] {
        let missing = target.saturating_sub(token_balance(&rpc, &vault).await?);
        if missing > 0 {
            deposits.push(deposit(&keys, &mint, &admin_ata, missing));
        }
    }
    if deposits.is_empty() {
        info!("vaults already hold the target inventory");
    } else {
        send(&rpc, &deposits, &admin, &[]).await?;
        info!("admin inventory deposited into the vaults");
    }

    let account = rpc
        .get_account(&keys.market())
        .await?
        .context("market account missing after initialize")?;
    if decode_market(&account.data)?.mid_price == 0 {
        let price = first_price(&opts).await?;
        let mid = usd_to_price(price, DECIMALS).context("first price is out of range")?;
        send(
            &rpc,
            &[force_price(&admin.pubkey(), &keys.market(), mid)],
            &admin,
            &[],
        )
        .await?;
        info!(price = format_args!("{price:.4}"), "initial price forced");
    } else {
        info!("market already has a price, skipping force_price");
    }

    let cfg = NetConfig {
        market: keys.market(),
        base_mint: bm,
        quote_mint: qm,
        base_vault: keys.base_vault(),
        quote_vault: keys.quote_vault(),
        base_decimals: BASE_DECIMALS,
        quote_decimals: QUOTE_DECIMALS,
        admin_keypair: admin_path,
        updater_keypair: updater_path,
        trader_keypair: trader_path,
        ws_url: opts.ws.clone().unwrap_or_else(|| default_ws(&rpc_url)),
        rpc_url,
    };
    cfg.save(&opts.out)?;
    info!(config = %opts.out.display(), "wrote config");
    Ok(())
}

/// Token amount of an SPL token account; 0 when the account does not exist yet.
async fn token_balance(rpc: &Rpc, key: &Pubkey) -> anyhow::Result<u64> {
    Ok(rpc
        .get_account(key)
        .await?
        .and_then(|a| token_amount(&a.data))
        .unwrap_or(0))
}

/// Human-readable program error for a failed transaction.
fn describe(e: &RpcError) -> String {
    match e.custom_code() {
        Some(code) => format!(
            "{} ({code}, {e})",
            error_name(code).unwrap_or("unknown program error")
        ),
        None => e.to_string(),
    }
}

/// Signs, sends and confirms `ixs`; program errors are named.
async fn send(
    rpc: &Rpc,
    ixs: &[Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
) -> anyhow::Result<String> {
    rpc.send_and_confirm(ixs, payer, extra_signers)
        .await
        .map_err(|e| anyhow::anyhow!(describe(&e)))
}

pub async fn admin_force_price(config: &Path, mid_usd: f64) -> anyhow::Result<()> {
    let cfg = NetConfig::load(config)?;
    let admin = read_keypair(&cfg.admin_keypair)?;
    let mid = usd_to_price(mid_usd, cfg.decimals()).context("price is out of range")?;
    let rpc = Rpc::new(&cfg.rpc_url)?;
    let sig = send(
        &rpc,
        &[force_price(&admin.pubkey(), &cfg.market, mid)],
        &admin,
        &[],
    )
    .await?;
    println!("force_price {mid_usd:.4} (mid {mid}) ok, signature {sig}");
    Ok(())
}

pub async fn admin_set_paused(config: &Path, paused: bool) -> anyhow::Result<()> {
    let cfg = NetConfig::load(config)?;
    let admin = read_keypair(&cfg.admin_keypair)?;
    let rpc = Rpc::new(&cfg.rpc_url)?;
    let sig = send(
        &rpc,
        &[set_paused(&admin.pubkey(), &cfg.market, paused)],
        &admin,
        &[],
    )
    .await?;
    println!(
        "{} ok, signature {sig}",
        if paused { "pause" } else { "unpause" }
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SideArg {
    /// Pay quote (USDC), receive base (SOL).
    Buy,
    /// Pay base (SOL), receive quote (USDC).
    Sell,
}

#[derive(Debug, Clone)]
pub struct SwapOptions {
    pub config: PathBuf,
    pub side: SideArg,
    /// Whole units of the input token (USDC for buy, SOL for sell).
    pub amount: f64,
    /// Whole units of the output token.
    pub min_out: Option<f64>,
}

async fn trader_balances(rpc: &Rpc, base: &Pubkey, quote: &Pubkey) -> anyhow::Result<(u64, u64)> {
    let (_, accounts) = rpc.get_accounts(&[*base, *quote]).await?;
    let amount = |i: usize| {
        accounts
            .get(i)
            .and_then(Option::as_ref)
            .and_then(|a| token_amount(&a.data))
            .context("trader token account missing")
    };
    Ok((amount(0)?, amount(1)?))
}

// A linear script; splitting it would only scatter the steps.
#[allow(clippy::too_many_lines)]
pub async fn trader_swap(opts: SwapOptions) -> anyhow::Result<()> {
    let cfg = NetConfig::load(&opts.config)?;
    let trader = read_keypair(&cfg.trader_keypair)?;
    let rpc = Rpc::new(&cfg.rpc_url)?;
    let market = cfg.market;
    let (base_mint, quote_mint) = (cfg.base_mint, cfg.quote_mint);
    let (trader_base, trader_quote) = (
        ata(&trader.pubkey(), &base_mint),
        ata(&trader.pubkey(), &quote_mint),
    );
    let (side, in_dec, out_dec) = match opts.side {
        SideArg::Buy => (Side::BuyBase, cfg.quote_decimals, cfg.base_decimals),
        SideArg::Sell => (Side::SellBase, cfg.base_decimals, cfg.quote_decimals),
    };
    let amount_in = ui_to_atoms(opts.amount, in_dec).context("bad --amount")?;
    let min_out = match opts.min_out {
        Some(m) => ui_to_atoms(m, out_dec).context("bad --min-out")?,
        None => 0,
    };

    // Quote with the pricing crate against the current on-chain state.
    let account = rpc
        .get_account(&market)
        .await?
        .context("market account not found")?;
    let state = decode_market(&account.data)?;
    let slot = rpc.get_slot().await?;
    let age = slot.saturating_sub(state.last_update_slot);
    let fresh = is_fresh(slot, state.last_update_slot, state.max_age_slots);
    let quote = state
        .ladder()
        .quote_exact_in(side, state.consumed(side), amount_in);
    println!(
        "market mid {:.4}, price age {age} slots (max {}){}{}",
        price_to_usd(state.mid_price, cfg.decimals()),
        state.max_age_slots,
        if fresh { "" } else { "  STALE" },
        if state.is_paused() { "  PAUSED" } else { "" },
    );
    let expected = match &quote {
        Ok(f) => {
            println!(
                "quote: in {} -> out {} (atoms: {amount_in} -> {})",
                opts.amount,
                atoms_to_ui(f.amount_out, out_dec),
                f.amount_out
            );
            Some(f.amount_out)
        }
        Err(e) => {
            println!("quote: pricing crate says {e:?}; sending anyway so the program decides");
            None
        }
    };

    let before = trader_balances(&rpc, &trader_base, &trader_quote).await?;
    let ix = swap(
        &trader.pubkey(),
        &market,
        &state,
        &trader_base,
        &trader_quote,
        SwapArgs {
            side,
            amount_in,
            min_out,
        },
    );
    let sig = send(&rpc, &[ix], &trader, &[])
        .await
        .context("swap failed")?;
    let after = trader_balances(&rpc, &trader_base, &trader_quote).await?;
    let actual = match side {
        Side::BuyBase => after.0.checked_sub(before.0),
        Side::SellBase => after.1.checked_sub(before.1),
    }
    .context("output balance went down")?;
    println!("swap ok, signature {sig}");
    println!(
        "actual out: {} (atoms {actual})",
        atoms_to_ui(actual, out_dec)
    );
    if let Some(e) = expected {
        println!("pre-trade quote == actual: {}", e == actual);
    }

    // The bot may update the price between the quote and the swap, so also replay the quote
    // with the mid and window the program really used (from its SwapEvent).
    let logs = rpc.transaction_logs(&sig).await.unwrap_or_default();
    let event = logs
        .iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .filter_map(|b| B64.decode(b).ok())
        .find_map(|d| decode_swap_event(&d));
    if let Some(ev) = event {
        let base_moved = match side {
            Side::BuyBase => ev.amount_out,
            Side::SellBase => ev.amount_in,
        };
        let mut at_exec = state.ladder();
        at_exec.mid_price = ev.mid_price;
        let replay = ev
            .consumed_after
            .checked_sub(base_moved)
            .and_then(|consumed| at_exec.quote_exact_in(side, consumed, amount_in).ok());
        println!(
            "executed in slot {} at mid {:.4}; replayed quote (pricing crate at that mid/window) {}; match: {}",
            ev.slot,
            price_to_usd(ev.mid_price, cfg.decimals()),
            replay.map_or_else(|| "n/a".to_owned(), |f| f.amount_out.to_string()),
            replay.is_some_and(|f| f.amount_out == actual && f.amount_out == ev.amount_out),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ws_follows_the_rpc_url() {
        assert_eq!(default_ws("http://127.0.0.1:8899"), "ws://127.0.0.1:8900");
        assert_eq!(
            default_ws("https://api.devnet.solana.com"),
            "wss://api.devnet.solana.com"
        );
    }

    #[test]
    fn topups_only_when_below_half() {
        assert_eq!(topup(0, 100), 100);
        assert_eq!(topup(49, 100), 51);
        assert_eq!(topup(50, 100), 0);
        assert_eq!(topup(1_000, 100), 0);
    }

    #[test]
    fn devnet_requirement_adds_up() {
        assert_eq!(
            devnet_requirement(10, 20, 30, 5),
            2 * 10 + 6 * 20 + 30 + 5 + DEVNET_FEE_BUFFER
        );
    }

    #[tokio::test]
    async fn devnet_needs_an_admin_keypair() {
        let opts = SetupOptions {
            cluster: Cluster::Devnet,
            rpc: Some("http://127.0.0.1:1".into()),
            ws: None,
            out: PathBuf::from("/nonexistent/out.json"),
            admin_keypair: None,
            keys_dir: Some(PathBuf::from("/nonexistent")),
            fair_usd: None,
            pyth_url: String::new(),
            base_mint: None,
            quote_mint: None,
        };
        let err = setup(opts).await.unwrap_err();
        assert!(err.to_string().contains("--admin-keypair"));
    }
}
