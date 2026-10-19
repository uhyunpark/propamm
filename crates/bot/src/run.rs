//! `propamm-bot run`: live feeds -> fair value -> publish policy -> `update_price` transactions.
//! Design and defaults: `docs/design-oracle.md`, "Publishing".

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use propamm_client::{PROGRAM_ID, decode_market, error_name, token_amount, update_price};
use propamm_pricing::MarketState;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval};
use tracing::{info, warn};

use crate::BPS;
use crate::chain::{Rpc, build_tx, custom_code_in, set_compute_unit_limit, set_compute_unit_price};
use crate::config::{NetConfig, read_keypair};
use crate::connector::now_unix_ns;
use crate::fair::FairValue;
use crate::live::{Feeds, LiveFair};
use crate::publish::{
    Decimals, Decision, Hold, Inputs, PolicyParams, Publisher, atoms_to_ui, base_value_share,
    gap_bps, price_to_usd, skew_bps, skewed_fair, usd_to_price,
};
use crate::pyth::{self, Breaker, PythSnapshot};
use crate::serve::{
    self, AppState, ChainView, ConfigView, Events, FairView, Faucet, InventoryView, ServerMsg,
    SharedControl, Tick, UpdateEvent, UpdateStatus, Venue,
};
use crate::subscribe::{self, WsEvent};

/// Compute unit limit of an `update_price` transaction (about 2,000 CU measured, plus the two
/// compute budget instructions). The slot is added modulo `CU_NONCE` so that two sends of the
/// same mid with the same cached blockhash are different transactions.
const CU_LIMIT: u32 = 4_000;
const CU_NONCE: u64 = 64;
const EVAL_EVERY: Duration = Duration::from_millis(25);
/// Polling defaults on a local validator; a remote RPC host rate-limits, so it polls slower.
const LOCAL_STATUS_POLL_MS: u64 = 200;
const LOCAL_REFRESH_MS: u64 = 2_000;
const REMOTE_STATUS_POLL_MS: u64 = 1_000;
const REMOTE_REFRESH_MS: u64 = 3_000;
const BLOCKHASH_EVERY: Duration = Duration::from_secs(4);
const REPORT_EVERY: Duration = Duration::from_secs(10);
/// A sent transaction without any status after this long is counted as dropped.
const DROP_AFTER: Duration = Duration::from_mins(1);
/// No update while the market state was last confirmed longer ago than this (ws or RPC poll).
const MAX_STATE_AGE: Duration = Duration::from_secs(6);
const GAP_SAMPLE_EVERY: Duration = Duration::from_millis(100);
const PYTH_MAX_STALE_S: u64 = 30;
/// Warn about a long clamp streak after this many slots.
const CLAMP_WARN_SLOTS: u64 = 20;
const TICK_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Default: `rpc_url` from the config.
    pub rpc: Option<String>,
    /// Default: `ws_url` from the config.
    pub ws: Option<String>,
    pub config: PathBuf,
    pub threshold_bps: f64,
    /// Default: half of the market's `max_age_slots`.
    pub heartbeat_slots: Option<u64>,
    pub skew_max_bps: f64,
    pub pyth_max_dev_bps: f64,
    pub pyth_url: String,
    pub cu_price: u64,
    pub dry_run: bool,
    /// Signature-status polling interval. Default: 200 ms on localhost, else 1000 ms.
    pub status_poll_ms: Option<u64>,
    /// Market/vault RPC refresh interval. Default: 2000 ms on localhost, else 3000 ms.
    pub refresh_ms: Option<u64>,
    /// Demo server (`--serve`).
    pub serve: Option<ServeOptions>,
}

#[derive(Debug, Clone, Copy)]
pub struct ServeOptions {
    /// Already validated by `serve::parse_bind`.
    pub addr: SocketAddr,
    /// Enables `POST /faucet` (still refused unless the RPC is local).
    pub faucet: bool,
}

/// What the bot loop shares with the demo server.
struct ServerHandle {
    events: Events,
    control: SharedControl,
}

/// One sent `update_price`, from send to the moment the market account shows it.
struct Pending {
    sig: String,
    sent_slot: u64,
    sent_at: Instant,
    fair: f64,
    target: u64,
    clamped: bool,
    mid_before: u64,
    landed_slot: Option<u64>,
}

#[derive(Default)]
struct Counters {
    sent: u64,
    landed: u64,
    failed: u64,
    delays: Vec<u64>,
    /// |target - on-chain mid| in bps, sampled at most every 100 ms while publishing.
    gaps: Vec<f64>,
}

struct Bot {
    opts: RunOptions,
    market: Pubkey,
    /// Market, base vault, quote vault: the accounts polled over RPC.
    keys: [Pubkey; 3],
    decimals: Decimals,
    updater: Option<Keypair>,
    rpc: Rpc,
    publisher: Publisher,
    live: LiveFair,
    pyth: watch::Receiver<PythSnapshot>,
    fair: Option<FairValue>,
    state: Option<MarketState>,
    state_slot: u64,
    /// When `state` was last confirmed by the ws subscription or an RPC poll.
    state_seen_at: Option<Instant>,
    vaults: Option<(u64, u64)>,
    slot: u64,
    blockhash: Option<Hash>,
    pending: Vec<Pending>,
    counters: Counters,
    send_errors: mpsc::UnboundedSender<(String, String)>,
    halt_reason: Option<String>,
    last_gap_sample: Option<Instant>,
    last_hold: Option<Hold>,
    // Latest policy inputs, for the status line.
    skew: f64,
    share: Option<f64>,
    breaker: Option<Breaker>,
    target_gap: Option<f64>,
    server: Option<ServerHandle>,
    /// Demo offset applied in the last `evaluate` (0 without `--serve`).
    offset_bps: f64,
}

// One linear startup script followed by the event loop.
#[allow(clippy::too_many_lines)]
pub async fn run(opts: RunOptions) -> anyhow::Result<()> {
    let cfg = NetConfig::load(&opts.config)?;
    let market = cfg.market;
    let keys = [market, cfg.base_vault, cfg.quote_vault];
    let rpc_url = opts.rpc.clone().unwrap_or_else(|| cfg.rpc_url.clone());
    let ws_url = opts.ws.clone().unwrap_or_else(|| cfg.ws_url.clone());
    let rpc = Rpc::new(&rpc_url)?;
    let updater = if opts.dry_run {
        None
    } else {
        Some(read_keypair(&cfg.updater_keypair)?)
    };

    let (_, accounts) = rpc
        .get_accounts(&keys)
        .await
        .context("reading the market (is the validator up?)")?;
    let state = accounts
        .first()
        .cloned()
        .flatten()
        .context("market account not found; run `setup-local` against this validator")?;
    let state = decode_market(&state.data)?;
    if let Some(kp) = &updater
        && state.updater != kp.pubkey().to_bytes()
    {
        bail!(
            "{} is not the market's updater ({})",
            kp.pubkey(),
            Pubkey::new_from_array(state.updater)
        );
    }
    let params = PolicyParams {
        threshold_bps: opts.threshold_bps,
        heartbeat_slots: opts
            .heartbeat_slots
            .unwrap_or((state.max_age_slots / 2).max(1)),
        clamp_warn_slots: CLAMP_WARN_SLOTS,
        max_state_age_ms: u64::try_from(MAX_STATE_AGE.as_millis()).unwrap_or(u64::MAX),
    };
    params
        .validate(state.max_age_slots)
        .map_err(anyhow::Error::msg)?;
    info!(
        market = %market,
        max_age_slots = state.max_age_slots,
        max_move_bps = state.max_move_bps,
        heartbeat_slots = params.heartbeat_slots,
        threshold_bps = opts.threshold_bps,
        skew_max_bps = opts.skew_max_bps,
        pyth_max_dev_bps = opts.pyth_max_dev_bps,
        dry_run = opts.dry_run,
        "starting"
    );

    let serving = opts.serve.is_some();
    let mut feeds = Feeds::spawn_with(serving)?;
    let (ws_tx, mut ws_rx) = mpsc::channel(1_024);
    let ws_task = tokio::spawn(subscribe::run(
        ws_url.clone(),
        market,
        serving.then_some(PROGRAM_ID),
        ws_tx,
    ));
    let (server, server_task) = match opts.serve {
        Some(serve_opts) => {
            let (handle, task) = start_server(serve_opts, &cfg, &rpc_url, &ws_url).await?;
            (Some(handle), Some(task))
        }
        None => (None, None),
    };
    let (pyth_tx, pyth_rx) = watch::channel(Err("not polled yet".to_owned()));
    let pyth_task = tokio::spawn(pyth::poll(opts.pyth_url.clone(), pyth_tx));
    let (err_tx, mut err_rx) = mpsc::unbounded_channel();

    let local = serve::is_local_rpc(&rpc_url);
    let status_ms = opts.status_poll_ms.unwrap_or(if local {
        LOCAL_STATUS_POLL_MS
    } else {
        REMOTE_STATUS_POLL_MS
    });
    let refresh_ms = opts.refresh_ms.unwrap_or(if local {
        LOCAL_REFRESH_MS
    } else {
        REMOTE_REFRESH_MS
    });
    info!(status_poll_ms = status_ms, refresh_ms, "polling intervals");
    let mut bot = Bot {
        publisher: Publisher::new(params),
        decimals: cfg.decimals(),
        market,
        keys,
        updater,
        rpc,
        live: if serving {
            LiveFair::with_venues()
        } else {
            LiveFair::new()
        },
        pyth: pyth_rx,
        fair: None,
        state: None,
        state_slot: 0,
        state_seen_at: None,
        vaults: None,
        slot: 0,
        blockhash: None,
        pending: Vec::new(),
        counters: Counters::default(),
        send_errors: err_tx,
        halt_reason: None,
        last_gap_sample: None,
        last_hold: None,
        skew: 0.0,
        share: None,
        breaker: None,
        target_gap: None,
        server,
        offset_bps: 0.0,
        opts,
    };
    bot.refresh().await;
    bot.refresh_blockhash().await;

    let mut eval_timer = interval(EVAL_EVERY);
    eval_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut status_timer = interval(Duration::from_millis(status_ms.max(1)));
    let mut refresh_timer = interval(Duration::from_millis(refresh_ms.max(1)));
    let mut blockhash_timer = interval(BLOCKHASH_EVERY);
    let mut report_timer = interval(REPORT_EVERY);
    report_timer.tick().await;
    let mut tick_timer = interval(TICK_EVERY);
    tick_timer.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            () = crate::shutdown_signal() => {
                info!("signal received, shutting down");
                break;
            }
            msg = feeds.rx.recv() => match msg {
                Some(m) => bot.live.ingest(&m),
                None => bail!("all feed tasks ended"),
            },
            ev = ws_rx.recv() => match ev {
                Some(WsEvent::Slot(s)) => bot.slot = bot.slot.max(s),
                Some(WsEvent::Market { data, slot }) => bot.apply_market(&data, slot),
                Some(WsEvent::Logs { signature, failed, logs }) => {
                    bot.handle_logs(&signature, failed, &logs);
                }
                None => bail!("ws task ended"),
            },
            Some((sig, error)) = err_rx.recv() => bot.send_failed(&sig, &error),
            _ = eval_timer.tick() => bot.evaluate(),
            _ = status_timer.tick() => bot.poll_statuses().await,
            _ = refresh_timer.tick() => bot.refresh().await,
            _ = blockhash_timer.tick() => bot.refresh_blockhash().await,
            _ = report_timer.tick() => bot.report(),
            _ = tick_timer.tick(), if serving => bot.broadcast_tick(),
        }
    }

    if let Some(t) = server_task {
        t.abort();
    }
    ws_task.abort();
    pyth_task.abort();
    feeds.shutdown().await;
    bot.summary();
    Ok(())
}

/// Binds the demo server and spawns it. Fails early on a bad address or a busy port.
async fn start_server(
    opts: ServeOptions,
    cfg: &NetConfig,
    rpc_url: &str,
    ws_url: &str,
) -> anyhow::Result<(ServerHandle, tokio::task::JoinHandle<()>)> {
    let faucet = if serve::faucet_allowed(opts.faucet, rpc_url) {
        warn!(
            "faucet enabled: the admin key is loaded and mints test tokens for anyone on 127.0.0.1"
        );
        let admin = read_keypair(&cfg.admin_keypair)?;
        Some(Arc::new(Faucet::new(Rpc::new(rpc_url)?, admin, cfg)?))
    } else {
        if opts.faucet {
            warn!(
                rpc = rpc_url,
                "--faucet ignored: the RPC is not localhost; /faucet answers 403"
            );
        }
        None
    };
    let events = Events::new();
    let control = SharedControl::default();
    let state = AppState {
        config: Arc::new(ConfigView::new(cfg, rpc_url, ws_url)),
        events: events.clone(),
        control: control.clone(),
        faucet,
    };
    let listener = tokio::net::TcpListener::bind(opts.addr)
        .await
        .with_context(|| format!("binding the demo server to {}", opts.addr))?;
    info!(addr = %opts.addr, "demo server listening (GET /config, WS /stream, POST /control, POST /faucet)");
    let task = tokio::spawn(async move {
        if let Err(e) = serve::serve(listener, state).await {
            warn!(error = %e, "demo server stopped");
        }
    });
    Ok((ServerHandle { events, control }, task))
}

impl Bot {
    fn usd(&self, mid: u64) -> f64 {
        price_to_usd(mid, self.decimals)
    }

    fn apply_market(&mut self, data: &[u8], slot: u64) {
        match decode_market(data) {
            Ok(state) => {
                self.state_seen_at = Some(Instant::now());
                if slot >= self.state_slot {
                    self.state_slot = slot;
                    self.slot = self.slot.max(slot);
                    self.state = Some(state);
                    self.resolve_landed();
                }
            }
            Err(e) => warn!(error = %e, "market account did not decode"),
        }
    }

    /// Polls market and vaults over RPC (the ws subscription covers the market between polls).
    async fn refresh(&mut self) {
        match self.rpc.get_accounts(&self.keys).await {
            Ok((slot, accounts)) => {
                if let Some(Some(a)) = accounts.first() {
                    self.apply_market(&a.data, slot);
                }
                let amount = |i: usize| {
                    accounts
                        .get(i)
                        .and_then(Option::as_ref)
                        .and_then(|a| token_amount(&a.data))
                };
                if let (Some(b), Some(q)) = (amount(1), amount(2)) {
                    self.vaults = Some((b, q));
                }
            }
            Err(e) => warn!(error = %e, "refreshing market and vaults failed"),
        }
        match self.rpc.get_slot().await {
            Ok(s) => self.slot = self.slot.max(s),
            Err(e) => warn!(error = %e, "getSlot failed"),
        }
    }

    async fn refresh_blockhash(&mut self) {
        match self.rpc.latest_blockhash().await {
            Ok(h) => self.blockhash = Some(h),
            Err(e) => warn!(error = %e, "blockhash refresh failed"),
        }
    }

    /// Computes the target and applies the publish policy. Called every 25 ms; the policy itself
    /// limits sends to one per slot.
    fn evaluate(&mut self) {
        let now_ns = now_unix_ns();
        self.fair = self.live.fair(now_ns);
        // Without a recent view of the market there is no valid band reference.
        let (Some(state), Some(seen_at), true) = (self.state, self.state_seen_at, self.slot > 0)
        else {
            return;
        };

        let snapshot = self.pyth.borrow().clone();
        self.breaker = self.fair.map(|f| {
            pyth::check(
                f.fair,
                &snapshot,
                now_ns / 1_000_000_000,
                self.opts.pyth_max_dev_bps,
                PYTH_MAX_STALE_S,
            )
        });
        let usable = self
            .fair
            .filter(|_| !self.breaker.as_ref().is_some_and(Breaker::trips));
        self.log_halt(usable.is_some());

        let (publishing, offset_bps) = self.demo_controls();
        self.offset_bps = offset_bps;
        let target = usable.and_then(|f| {
            let share = self.vaults.and_then(|(b, q)| {
                base_value_share(
                    atoms_to_ui(b, self.decimals.base),
                    atoms_to_ui(q, self.decimals.quote),
                    f.fair,
                )
            });
            self.share = share;
            self.skew = skew_bps(share, self.opts.skew_max_bps);
            // The demo offset moves the fair value before skew and band clamp.
            let fair = f.fair * (1.0 + offset_bps / BPS);
            usd_to_price(skewed_fair(fair, self.skew), self.decimals)
        });

        self.target_gap = target.map(|t| gap_bps(t, state.mid_price));
        if let Some(gap) = self.target_gap
            && state.mid_price != 0
            && !state.is_paused()
            && self
                .last_gap_sample
                .is_none_or(|t| t.elapsed() >= GAP_SAMPLE_EVERY)
        {
            self.last_gap_sample = Some(Instant::now());
            self.counters.gaps.push(gap.abs());
        }

        let eval = self.publisher.evaluate(&Inputs {
            slot: self.slot,
            target,
            onchain_mid: state.mid_price,
            last_update_slot: state.last_update_slot,
            state_age_ms: u64::try_from(seen_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            max_move_bps: state.max_move_bps,
            paused: state.is_paused(),
            pending: !self.pending.is_empty(),
        });
        if eval.clamp_warning {
            warn!(
                clamp_warn_slots = CLAMP_WARN_SLOTS,
                "target is outside the move band for many slots in a row; \
                 consider the admin `force-price` command"
            );
        }
        match eval.decision {
            Decision::Send { mid, clamped } => {
                self.last_hold = None;
                // Demo control: no sends while publishing is off, so the price goes stale.
                if publishing && let Some(f) = self.fair {
                    self.send(mid, clamped, f.fair, state.mid_price);
                }
            }
            Decision::Hold(hold @ (Hold::NotTradable | Hold::StaleState)) => {
                if self.last_hold != Some(hold) {
                    self.last_hold = Some(hold);
                    if hold == Hold::NotTradable {
                        warn!(
                            "market has no price or is paused: nothing to update \
                             (admin `force-price` / `unpause`)"
                        );
                    } else {
                        warn!("market state not confirmed recently: sending no updates");
                    }
                }
            }
            Decision::Hold(_) => self.last_hold = None,
        }
    }

    /// `(publishing, offset_bps)` from the demo controls; defaults without `--serve`.
    fn demo_controls(&self) -> (bool, f64) {
        self.server.as_ref().map_or((true, 0.0), |s| {
            let c = s.control.lock();
            (c.publishing(), c.effective_offset(Instant::now()))
        })
    }

    fn emit_update(
        &self,
        signature: &str,
        target_slot: u64,
        landed_slot: Option<u64>,
        mid: u64,
        status: UpdateStatus,
    ) {
        let Some(server) = &self.server else { return };
        server.events.send(&ServerMsg::Update(UpdateEvent {
            signature: signature.to_owned(),
            target_slot,
            landed_slot,
            delay_slots: landed_slot.map(|l| l.saturating_sub(target_slot)),
            mid: self.usd(mid),
            status,
        }));
    }

    /// Swap events of this market from program logs, forwarded to the stream.
    fn handle_logs(&self, signature: &str, failed: bool, logs: &[String]) {
        let Some(server) = &self.server else { return };
        if failed || !server.events.has_clients() {
            return;
        }
        for swap in serve::swaps_in_logs(&self.market, signature, logs) {
            server.events.send(&ServerMsg::Swap(swap));
        }
    }

    /// One `tick` message (every 250 ms) when a client is connected.
    fn broadcast_tick(&self) {
        let Some(server) = &self.server else { return };
        if !server.events.has_clients() {
            return;
        }
        let now_ns = now_unix_ns();
        let venues = self
            .live
            .venues(now_ns)
            .into_iter()
            .map(|(key, b)| {
                (
                    key.to_owned(),
                    Venue {
                        bid: b.bid,
                        ask: b.ask,
                        mid: b.mid(),
                    },
                )
            })
            .collect();
        let (publishing, offset_bps) = self.demo_controls();
        server.events.send(&ServerMsg::Tick(Tick {
            ts_ms: now_ns / crate::NS_PER_MS,
            venues,
            fair: self.fair.map(|f| FairView {
                price: f.fair,
                conf_bps: f.conf_bps,
                live: f.live_sources,
            }),
            chain: self.state.map(|s| ChainView {
                slot: self.slot,
                mid: self.usd(s.mid_price),
                last_update_slot: s.last_update_slot,
                age_slots: self.slot.saturating_sub(s.last_update_slot),
                max_age_slots: s.max_age_slots,
                paused: s.is_paused(),
            }),
            inventory: self.vaults.map(|(b, q)| InventoryView {
                base: atoms_to_ui(b, self.decimals.base),
                quote: atoms_to_ui(q, self.decimals.quote),
                skew_bps: self.skew,
            }),
            pyth_dev_bps: self.breaker.as_ref().and_then(Breaker::dev_bps),
            publishing,
            offset_bps,
        }));
    }

    fn log_halt(&mut self, running: bool) {
        let reason = if running {
            None
        } else if self.breaker.as_ref().is_some_and(Breaker::trips) {
            Some(format!(
                "pyth circuit breaker tripped (dev {:.1} bps)",
                self.breaker
                    .as_ref()
                    .and_then(Breaker::dev_bps)
                    .unwrap_or(0.0)
            ))
        } else {
            Some("fair engine halted (fewer than 2 live sources or USDC off peg)".to_owned())
        };
        if reason != self.halt_reason {
            if let Some(r) = &reason {
                warn!(reason = %r, "HALT: sending no updates; the on-chain price will go stale");
            } else {
                info!("resumed: fair value is live again");
            }
            self.halt_reason = reason;
        }
    }

    fn send(&mut self, mid: u64, clamped: bool, fair: f64, mid_before: u64) {
        self.publisher.record_sent(self.slot);
        let target_usd = self.usd(mid);
        if self.opts.dry_run {
            info!(
                slot = self.slot,
                fair = format_args!("{fair:.4}"),
                target = format_args!("{target_usd:.4}"),
                onchain_mid = format_args!("{:.4}", self.usd(mid_before)),
                clamped,
                "dry-run: would send update_price"
            );
            return;
        }
        let (Some(updater), Some(hash)) = (&self.updater, self.blockhash) else {
            warn!("no blockhash yet, skipping this slot");
            return;
        };
        let ixs = [
            set_compute_unit_limit(CU_LIMIT + u32::try_from(self.slot % CU_NONCE).unwrap_or(0)),
            set_compute_unit_price(self.opts.cu_price),
            update_price(&updater.pubkey(), &self.market, mid),
        ];
        let (sig, wire) = match build_tx(&ixs, updater, &[], hash) {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "building the transaction failed");
                return;
            }
        };
        let sig = sig.to_string();
        if self.offset_bps != 0.0 {
            info!(
                offset_bps = self.offset_bps,
                fair = format_args!("{fair:.4}"),
                sent_mid = format_args!("{target_usd:.4}"),
                clamped,
                "demo offset active: this update is not the plain fair value"
            );
        }
        self.emit_update(&sig, self.slot, None, mid, UpdateStatus::Sent);
        self.counters.sent += 1;
        self.pending.push(Pending {
            sig: sig.clone(),
            sent_slot: self.slot,
            sent_at: Instant::now(),
            fair,
            target: mid,
            clamped,
            mid_before,
            landed_slot: None,
        });
        // Send without waiting: the result of the RPC call only matters when it fails.
        let rpc = self.rpc.clone();
        let errors = self.send_errors.clone();
        tokio::spawn(async move {
            if let Err(e) = rpc.send_raw(&wire, true).await {
                let _ = errors.send((sig, e.to_string()));
            }
        });
    }

    fn send_failed(&mut self, sig: &str, error: &str) {
        if let Some(i) = self.pending.iter().position(|p| p.sig == sig) {
            let p = self.pending.remove(i);
            self.counters.failed += 1;
            self.emit_update(&p.sig, p.sent_slot, None, p.target, UpdateStatus::Failed);
            warn!(sent_slot = p.sent_slot, %error, "sendTransaction failed");
        }
    }

    async fn poll_statuses(&mut self) {
        let open: Vec<String> = self
            .pending
            .iter()
            .filter(|p| p.landed_slot.is_none())
            .map(|p| p.sig.clone())
            .collect();
        if open.is_empty() {
            return;
        }
        let statuses = match self.rpc.signature_statuses(&open).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "status poll failed");
                return;
            }
        };
        for (sig, status) in open.iter().zip(statuses) {
            let Some(i) = self.pending.iter().position(|p| &p.sig == sig) else {
                continue;
            };
            match status {
                Some(s) => match s.err {
                    None => {
                        let p = &mut self.pending[i];
                        p.landed_slot = Some(s.slot);
                        let delay = s.slot.saturating_sub(p.sent_slot);
                        self.counters.landed += 1;
                        self.counters.delays.push(delay);
                        let (sig, sent_slot, target) = (p.sig.clone(), p.sent_slot, p.target);
                        self.emit_update(
                            &sig,
                            sent_slot,
                            Some(s.slot),
                            target,
                            UpdateStatus::Landed,
                        );
                    }
                    Some(err) => {
                        let p = self.pending.remove(i);
                        self.counters.failed += 1;
                        self.emit_update(
                            &p.sig,
                            p.sent_slot,
                            Some(s.slot),
                            p.target,
                            UpdateStatus::Failed,
                        );
                        let text = err.to_string();
                        let name = custom_code_in(&text).and_then(error_name);
                        warn!(
                            target_slot = p.sent_slot,
                            landed_slot = s.slot,
                            error = %text,
                            program_error = name.unwrap_or("-"),
                            "update failed on chain"
                        );
                    }
                },
                None if self.pending[i].sent_at.elapsed() > DROP_AFTER => {
                    let p = self.pending.remove(i);
                    self.counters.failed += 1;
                    self.emit_update(&p.sig, p.sent_slot, None, p.target, UpdateStatus::Failed);
                    warn!(target_slot = p.sent_slot, "update never landed (dropped)");
                }
                None => {}
            }
        }
        self.resolve_landed();
    }

    /// Logs and drops updates whose landing slot is now visible in the market account.
    fn resolve_landed(&mut self) {
        let Some(state) = self.state else { return };
        let (mid_after, visible) = (state.mid_price, state.last_update_slot);
        let mut i = 0;
        while i < self.pending.len() {
            match self.pending[i].landed_slot {
                Some(landed) if visible >= landed => {
                    let p = self.pending.remove(i);
                    info!(
                        target_slot = p.sent_slot,
                        landed_slot = landed,
                        delay_slots = landed.saturating_sub(p.sent_slot),
                        fair = format_args!("{:.4}", p.fair),
                        target = format_args!("{:.4}", self.usd(p.target)),
                        clamped = p.clamped,
                        mid_before = format_args!("{:.4}", self.usd(p.mid_before)),
                        mid_after = format_args!("{:.4}", self.usd(mid_after)),
                        "update landed"
                    );
                }
                _ => i += 1,
            }
        }
    }

    fn report(&self) {
        let (fair, gap) = match (self.fair, self.state) {
            (Some(f), Some(s)) => (
                format!("{:.4}", f.fair),
                format!(
                    "{:+.2}",
                    gap_bps(
                        usd_to_price(f.fair, self.decimals).unwrap_or(0),
                        s.mid_price
                    )
                ),
            ),
            _ => ("-".to_owned(), "-".to_owned()),
        };
        let mid = self.state.map_or_else(
            || "-".to_owned(),
            |s| format!("{:.4}", self.usd(s.mid_price)),
        );
        let inventory = self.vaults.map_or_else(
            || "-".to_owned(),
            |(b, q)| {
                format!(
                    "{:.2} base / {:.2} quote",
                    atoms_to_ui(b, self.decimals.base),
                    atoms_to_ui(q, self.decimals.quote)
                )
            },
        );
        let pyth = self
            .breaker
            .as_ref()
            .and_then(Breaker::dev_bps)
            .map_or_else(|| "n/a".to_owned(), |d| format!("{d:.1}"));
        info!(
            slot = self.slot,
            fair,
            onchain_mid = mid,
            gap_bps = gap,
            target_gap_bps = self
                .target_gap
                .map_or_else(|| "-".to_owned(), |g| format!("{g:+.2}")),
            sent = self.counters.sent,
            landed = self.counters.landed,
            failed = self.counters.failed,
            inventory,
            base_share = self
                .share
                .map_or_else(|| "-".to_owned(), |s| format!("{s:.3}")),
            skew_bps = format_args!("{:+.2}", self.skew),
            pyth_dev_bps = pyth,
            halted = self.halt_reason.is_some(),
            "status"
        );
    }

    // Counts are small, the share is for display.
    #[allow(clippy::cast_precision_loss)]
    fn summary(&self) {
        let c = &self.counters;
        let mut hist: BTreeMap<u64, u64> = BTreeMap::new();
        for d in &c.delays {
            *hist.entry(*d).or_default() += 1;
        }
        let mut sorted = c.delays.clone();
        sorted.sort_unstable();
        let pct = |p: usize| {
            sorted
                .get((sorted.len().saturating_sub(1)) * p / 100)
                .copied()
                .unwrap_or(0)
        };
        let mut gaps = c.gaps.clone();
        gaps.sort_by(f64::total_cmp);
        let gap_pct = |p: usize| {
            gaps.get((gaps.len().saturating_sub(1)) * p / 100)
                .copied()
                .unwrap_or(0.0)
        };
        let above = gaps
            .iter()
            .filter(|g| **g >= self.opts.threshold_bps)
            .count();
        info!(
            samples = gaps.len(),
            p50 = format_args!("{:.2}", gap_pct(50)),
            p90 = format_args!("{:.2}", gap_pct(90)),
            p99 = format_args!("{:.2}", gap_pct(99)),
            max = format_args!("{:.2}", gap_pct(100)),
            share_at_or_above_threshold = format_args!(
                "{:.3}",
                if gaps.is_empty() {
                    0.0
                } else {
                    above as f64 / gaps.len() as f64
                }
            ),
            "summary: |target - on-chain mid| in bps, sampled every 100 ms"
        );
        let hist: Vec<String> = hist.iter().map(|(d, n)| format!("{d}:{n}")).collect();
        info!(
            sent = c.sent,
            landed = c.landed,
            failed = c.failed,
            delay_p50 = pct(50),
            delay_p90 = pct(90),
            delay_max = sorted.last().copied().unwrap_or(0),
            delay_histogram = hist.join(" "),
            "summary (landing delay in slots)"
        );
        let (rate_limited, retries) = self.rpc.retry_stats();
        info!(
            rate_limited,
            retries, "summary (rpc http 429 responses and retries)"
        );
    }
}
