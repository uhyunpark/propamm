use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use propamm_bot::eval::{self, EvalOptions};
use propamm_bot::run::{self, RunOptions};
use propamm_bot::serve;
use propamm_bot::tools::{self, Cluster, SetupOptions, SideArg, SwapOptions};
use propamm_bot::{RecordOptions, record};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record raw market-data feeds to hourly zstd-compressed JSONL files.
    Record {
        /// Output directory.
        #[arg(long, default_value = "data/raw")]
        out: PathBuf,
        /// Comma-separated feed names. Default: all.
        #[arg(long, value_delimiter = ',')]
        feeds: Vec<String>,
        /// Stop after this many seconds (smoke tests).
        #[arg(long)]
        duration: Option<u64>,
    },
    /// Replay recordings and print the fair-value evaluation as Markdown.
    Eval {
        /// Recording directory (`data/raw` layout).
        #[arg(long, default_value = "data/raw")]
        data: PathBuf,
        /// Score samples from this unix time (seconds, UTC).
        #[arg(long)]
        from: Option<u64>,
        /// Score samples up to this unix time (seconds, UTC).
        #[arg(long)]
        to: Option<u64>,
        /// Publish latency added to t before the reference lookup.
        #[arg(long, default_value_t = 150)]
        latency_ms: u64,
        /// Skip scoring this long after the first record (EMA warm-up).
        #[arg(long, default_value_t = 90)]
        warmup_s: u64,
        /// Half spread of the simulated markout quotes.
        #[arg(long, default_value_t = 2.0)]
        half_spread_bps: f64,
    },
    /// Publish `update_price` transactions from the live fair value until Ctrl-C.
    Run {
        /// Default: `rpc_url` from the config.
        #[arg(long)]
        rpc: Option<String>,
        /// Default: `ws_url` from the config.
        #[arg(long)]
        ws: Option<String>,
        /// Config written by `setup-local`.
        #[arg(long, default_value = "data/localnet.json")]
        config: PathBuf,
        /// Send when the target is at least this far from the on-chain mid.
        #[arg(long, default_value_t = 1.0)]
        threshold_bps: f64,
        /// Send at least every N slots. Default: half of the market's `max_age_slots`.
        #[arg(long)]
        heartbeat_slots: Option<u64>,
        /// Largest inventory skew applied to the mid.
        #[arg(long, default_value_t = 3.0)]
        skew_max_bps: f64,
        /// Halt when |fair - Pyth| exceeds this.
        #[arg(long, default_value_t = 50.0)]
        pyth_max_dev_bps: f64,
        /// Hermes base URL for the circuit breaker.
        #[arg(long, default_value = propamm_bot::pyth::DEFAULT_HERMES_URL)]
        pyth_url: String,
        /// Priority fee in micro-lamports per compute unit.
        #[arg(long, default_value_t = 0)]
        cu_price: u64,
        /// Compute and log targets without sending.
        #[arg(long)]
        dry_run: bool,
        /// Signature-status polling interval in ms. Default: 200 on localhost, else 1000.
        #[arg(long)]
        status_poll_ms: Option<u64>,
        /// Market/vault RPC refresh interval in ms. Default: 2000 on localhost, else 3000.
        #[arg(long)]
        refresh_ms: Option<u64>,
        /// Start the demo server (config, stream, controls) on this address; 127.0.0.1 only (demo: 127.0.0.1:8788).
        #[arg(long, value_name = "127.0.0.1:PORT")]
        serve: Option<String>,
        /// With --serve: enable `POST /faucet` (mints test tokens with the admin key; localhost RPC only).
        #[arg(long, requires = "serve")]
        faucet: bool,
    },
    /// Create keypairs, mints and a funded, priced market (local validator or devnet).
    #[command(alias = "setup-local")]
    Setup {
        #[arg(long, value_enum, default_value = "local")]
        cluster: Cluster,
        /// Default: the cluster's RPC (localhost:8899, or api.devnet.solana.com).
        #[arg(long)]
        rpc: Option<String>,
        /// Default: derived from the RPC URL.
        #[arg(long)]
        ws: Option<String>,
        /// Default: data/localnet.json (pass data/devnet.json for devnet).
        #[arg(long, default_value = "data/localnet.json")]
        out: PathBuf,
        /// Admin (mint authority, market admin). Required on devnet; it must hold SOL.
        #[arg(long)]
        admin_keypair: Option<PathBuf>,
        /// Keypair directory. Default: ~/.config/solana/propamm-local (devnet: propamm-devnet).
        #[arg(long)]
        keys_dir: Option<PathBuf>,
        /// Force this price instead of looking one up.
        #[arg(long)]
        fair_usd: Option<f64>,
        #[arg(long, default_value = propamm_bot::pyth::DEFAULT_HERMES_URL)]
        pyth_url: String,
        /// Resume with an existing base mint (needs --quote-mint): existing accounts are kept.
        #[arg(long, requires = "quote_mint")]
        base_mint: Option<solana_pubkey::Pubkey>,
        /// Resume with an existing quote mint (needs --base-mint).
        #[arg(long, requires = "base_mint")]
        quote_mint: Option<solana_pubkey::Pubkey>,
    },
    /// Admin: set the mid directly (no move limit).
    ForcePrice {
        #[arg(long, default_value = "data/localnet.json")]
        config: PathBuf,
        /// USD per SOL.
        #[arg(long)]
        mid: f64,
    },
    /// Admin: pause the market.
    Pause {
        #[arg(long, default_value = "data/localnet.json")]
        config: PathBuf,
    },
    /// Admin: unpause the market.
    Unpause {
        #[arg(long, default_value = "data/localnet.json")]
        config: PathBuf,
    },
    /// Trader: quote with the pricing crate, swap, and compare with the real output.
    Swap {
        #[arg(long, default_value = "data/localnet.json")]
        config: PathBuf,
        #[arg(long, value_enum)]
        side: SideArg,
        /// Input amount in whole tokens: USDC for `buy`, SOL for `sell`.
        #[arg(long)]
        amount: f64,
        /// Minimum output in whole tokens (default 0).
        #[arg(long)]
        min_out: Option<f64>,
    },
}

// One match arm per subcommand.
#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    // One process-wide TLS provider for feeds (wss) and reqwest (Hermes, https RPC).
    // Err means one is already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    match Cli::parse().command {
        Command::Record {
            out,
            feeds,
            duration,
        } => {
            record(RecordOptions {
                out,
                feeds,
                duration: duration.map(Duration::from_secs),
            })
            .await
        }
        Command::Eval {
            data,
            from,
            to,
            latency_ms,
            warmup_s,
            half_spread_bps,
        } => {
            let report = eval::run(&EvalOptions {
                data,
                from_s: from,
                to_s: to,
                latency_ms,
                warmup_s,
                half_spread_bps,
            })?;
            println!("{report}");
            Ok(())
        }
        Command::Run {
            rpc,
            ws,
            config,
            threshold_bps,
            heartbeat_slots,
            skew_max_bps,
            pyth_max_dev_bps,
            pyth_url,
            cu_price,
            dry_run,
            status_poll_ms,
            refresh_ms,
            serve,
            faucet,
        } => {
            let serve = serve
                .map(|addr| serve::parse_bind(&addr).map(|addr| run::ServeOptions { addr, faucet }))
                .transpose()?;
            run::run(RunOptions {
                rpc,
                ws,
                config,
                threshold_bps,
                heartbeat_slots,
                skew_max_bps,
                pyth_max_dev_bps,
                pyth_url,
                cu_price,
                dry_run,
                status_poll_ms,
                refresh_ms,
                serve,
            })
            .await
        }
        Command::Setup {
            cluster,
            rpc,
            ws,
            out,
            admin_keypair,
            keys_dir,
            fair_usd,
            pyth_url,
            base_mint,
            quote_mint,
        } => {
            tools::setup(SetupOptions {
                cluster,
                rpc,
                ws,
                out,
                admin_keypair,
                keys_dir,
                fair_usd,
                pyth_url,
                base_mint,
                quote_mint,
            })
            .await
        }
        Command::ForcePrice { config, mid } => tools::admin_force_price(&config, mid).await,
        Command::Pause { config } => tools::admin_set_paused(&config, true).await,
        Command::Unpause { config } => tools::admin_set_paused(&config, false).await,
        Command::Swap {
            config,
            side,
            amount,
            min_out,
        } => {
            tools::trader_swap(SwapOptions {
                config,
                side,
                amount,
                min_out,
            })
            .await
        }
    }
}
