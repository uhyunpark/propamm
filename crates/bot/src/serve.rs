//! Demo server (`propamm-bot run --serve`): config, a JSON stream of ticks and events, demo
//! controls and an optional local faucet. Interface: `docs/demo-contract.md`.
//!
//! The server only reads shared state and sends messages; the publishing loop never waits for
//! it. Clients get a bounded broadcast channel and are dropped when they fall behind.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, bail};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State, rejection::JsonRejection};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use propamm_client::{PROGRAM_ID, SwapEvent, TOKEN_PROGRAM_ID, decode_swap_event};
use propamm_pricing::Side;
use serde::{Deserialize, Serialize};
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{info, warn};

use crate::chain::{Rpc, ata, create_ata_idempotent, mint_to};
use crate::config::NetConfig;
use crate::publish::ui_to_atoms;

/// Browser origins of the Vite dev server.
pub const ALLOWED_ORIGINS: [&str; 2] = ["http://localhost:5173", "http://127.0.0.1:5173"];
pub const DEFAULT_OFFSET_SECONDS: f64 = 10.0;
pub const MAX_OFFSET_SECONDS: f64 = 300.0;
pub const MAX_OFFSET_BPS: f64 = 1_000.0;
/// Messages a client may fall behind before it is dropped.
const BROADCAST_CAPACITY: usize = 256;
/// A client that does not take a message within this long is dropped.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const FAUCET_BASE_UI: f64 = 50.0;
const FAUCET_QUOTE_UI: f64 = 10_000.0;
const FAUCET_LAMPORTS: u64 = 1_000_000_000;

/// Accepts `127.0.0.1:<port>` only: the server has no authentication.
pub fn parse_bind(addr: &str) -> anyhow::Result<SocketAddr> {
    let addr: SocketAddr = addr
        .parse()
        .with_context(|| format!("invalid --serve address `{addr}` (expected 127.0.0.1:PORT)"))?;
    if addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        bail!("--serve must bind 127.0.0.1 (got {addr}): the demo server is unauthenticated");
    }
    Ok(addr)
}

/// Whether an RPC URL points at this machine: an http(s) URL whose parsed host is exactly
/// `127.0.0.1` or `localhost` (not a substring match, so `localhost.evil.com` fails).
pub fn is_local_rpc(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https")
            && matches!(u.host_str(), Some("127.0.0.1" | "localhost"))
    })
}

/// The faucet needs both the flag and a local RPC.
pub fn faucet_allowed(enabled: bool, rpc_url: &str) -> bool {
    enabled && is_local_rpc(rpc_url)
}

// ---------------------------------------------------------------------------------------------
// Control

/// Demo controls: publishing on/off and a temporary offset applied to the fair value.
#[derive(Debug, Clone, Copy)]
pub struct ControlState {
    publishing: bool,
    offset_bps: f64,
    offset_until: Option<Instant>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlRequest {
    pub publishing: Option<bool>,
    pub offset_bps: Option<f64>,
    pub offset_seconds: Option<f64>,
}

/// Response of `POST /control`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlView {
    pub publishing: bool,
    pub offset_bps: f64,
    pub offset_seconds_left: f64,
}

impl Default for ControlState {
    fn default() -> Self {
        Self {
            publishing: true,
            offset_bps: 0.0,
            offset_until: None,
        }
    }
}

impl ControlState {
    pub fn publishing(&self) -> bool {
        self.publishing
    }

    /// The offset in force at `now`; 0 once it has expired.
    pub fn effective_offset(&self, now: Instant) -> f64 {
        match self.offset_until {
            Some(until) if now < until => self.offset_bps,
            _ => 0.0,
        }
    }

    pub fn view(&self, now: Instant) -> ControlView {
        let left = self
            .offset_until
            .map_or(0.0, |u| u.saturating_duration_since(now).as_secs_f64());
        ControlView {
            publishing: self.publishing,
            offset_bps: self.effective_offset(now),
            offset_seconds_left: left,
        }
    }

    /// Applies a request atomically: nothing changes when it is invalid.
    pub fn apply(&mut self, req: &ControlRequest, now: Instant) -> Result<(), String> {
        let offset = match (req.offset_bps, req.offset_seconds) {
            (None, None) => None,
            (None, Some(_)) => return Err("offsetSeconds needs offsetBps".to_owned()),
            (Some(bps), secs) => {
                let secs = secs.unwrap_or(DEFAULT_OFFSET_SECONDS);
                if !bps.is_finite() || bps.abs() > MAX_OFFSET_BPS {
                    return Err(format!("offsetBps must be within +-{MAX_OFFSET_BPS}"));
                }
                if !secs.is_finite() || secs <= 0.0 || secs > MAX_OFFSET_SECONDS {
                    return Err(format!(
                        "offsetSeconds must be in (0, {MAX_OFFSET_SECONDS}]"
                    ));
                }
                Some((bps, secs))
            }
        };
        if let Some(p) = req.publishing {
            self.publishing = p;
        }
        if let Some((bps, secs)) = offset {
            if bps == 0.0 {
                self.offset_bps = 0.0;
                self.offset_until = None;
            } else {
                self.offset_bps = bps;
                self.offset_until = Some(now + Duration::from_secs_f64(secs));
            }
        }
        Ok(())
    }
}

/// Control state shared between the bot loop and the HTTP handlers.
#[derive(Debug, Clone, Default)]
pub struct SharedControl(Arc<Mutex<ControlState>>);

impl SharedControl {
    pub fn lock(&self) -> MutexGuard<'_, ControlState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

// ---------------------------------------------------------------------------------------------
// Messages

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Venue {
    pub bid: f64,
    pub ask: f64,
    pub mid: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FairView {
    pub price: f64,
    pub conf_bps: f64,
    pub live: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainView {
    pub slot: u64,
    pub mid: f64,
    pub last_update_slot: u64,
    pub age_slots: u64,
    pub max_age_slots: u64,
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryView {
    pub base: f64,
    pub quote: f64,
    pub skew_bps: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tick {
    pub ts_ms: u64,
    pub venues: BTreeMap<String, Venue>,
    pub fair: Option<FairView>,
    pub chain: Option<ChainView>,
    pub inventory: Option<InventoryView>,
    pub pyth_dev_bps: Option<f64>,
    pub publishing: bool,
    pub offset_bps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateStatus {
    Sent,
    Landed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEvent {
    pub signature: String,
    pub target_slot: u64,
    pub landed_slot: Option<u64>,
    pub delay_slots: Option<u64>,
    pub mid: f64,
    pub status: UpdateStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapMsg {
    pub signature: String,
    pub slot: u64,
    pub side: String,
    pub amount_in: String,
    pub amount_out: String,
    pub trader: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerMsg {
    Tick(Tick),
    Update(UpdateEvent),
    Swap(SwapMsg),
}

impl SwapMsg {
    fn new(signature: &str, ev: &SwapEvent) -> Self {
        Self {
            signature: signature.to_owned(),
            slot: ev.slot,
            side: match ev.side {
                Side::BuyBase => "buy",
                Side::SellBase => "sell",
            }
            .to_owned(),
            amount_in: ev.amount_in.to_string(),
            amount_out: ev.amount_out.to_string(),
            trader: ev.trader.to_string(),
        }
    }
}

/// Swap events of `market` in the logs of one successful transaction.
pub fn swaps_in_logs(market: &Pubkey, signature: &str, logs: &[String]) -> Vec<SwapMsg> {
    logs.iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .filter_map(|b64| B64.decode(b64.trim()).ok())
        .filter_map(|bytes| decode_swap_event(&bytes))
        .filter(|ev| ev.market == *market)
        .map(|ev| SwapMsg::new(signature, &ev))
        .collect()
}

/// Sending half of the stream. Cloning is cheap; sending never blocks.
#[derive(Debug, Clone)]
pub struct Events {
    tx: broadcast::Sender<Arc<str>>,
}

impl Events {
    pub fn new() -> Self {
        Self {
            tx: broadcast::channel(BROADCAST_CAPACITY).0,
        }
    }

    pub fn has_clients(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// Serializes and broadcasts `msg`; skipped while nobody listens.
    pub fn send(&self, msg: &ServerMsg) {
        if !self.has_clients() {
            return;
        }
        match serde_json::to_string(msg) {
            Ok(text) => {
                let _ = self.tx.send(text.into());
            }
            Err(e) => warn!(error = %e, "serializing a stream message failed"),
        }
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------------------------
// HTTP

/// `GET /config` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
    pub program_id: String,
    pub market: String,
    pub base_mint: String,
    pub quote_mint: String,
    pub base_vault: String,
    pub quote_vault: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub rpc_url: String,
    pub ws_url: String,
    pub token_program: String,
}

impl ConfigView {
    pub fn new(cfg: &NetConfig, rpc_url: &str, ws_url: &str) -> Self {
        Self {
            program_id: PROGRAM_ID.to_string(),
            market: cfg.market.to_string(),
            base_mint: cfg.base_mint.to_string(),
            quote_mint: cfg.quote_mint.to_string(),
            base_vault: cfg.base_vault.to_string(),
            quote_vault: cfg.quote_vault.to_string(),
            base_decimals: cfg.base_decimals,
            quote_decimals: cfg.quote_decimals,
            rpc_url: rpc_url.to_owned(),
            ws_url: ws_url.to_owned(),
            token_program: TOKEN_PROGRAM_ID.to_string(),
        }
    }
}

/// Mints test tokens with the admin key (the mints' authority). Local validators only.
pub struct Faucet {
    rpc: Rpc,
    admin: Keypair,
    base_mint: Pubkey,
    quote_mint: Pubkey,
    base_atoms: u64,
    quote_atoms: u64,
    /// One faucet transaction at a time (they share the admin as fee payer).
    lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FaucetResponse {
    signature: String,
    airdrop_signature: String,
}

impl Faucet {
    pub fn new(rpc: Rpc, admin: Keypair, cfg: &NetConfig) -> anyhow::Result<Self> {
        Ok(Self {
            rpc,
            admin,
            base_mint: cfg.base_mint,
            quote_mint: cfg.quote_mint,
            base_atoms: ui_to_atoms(FAUCET_BASE_UI, cfg.base_decimals).context("base amount")?,
            quote_atoms: ui_to_atoms(FAUCET_QUOTE_UI, cfg.quote_decimals)
                .context("quote amount")?,
            lock: tokio::sync::Mutex::new(()),
        })
    }

    async fn fund(&self, owner: &Pubkey) -> anyhow::Result<FaucetResponse> {
        let _one_at_a_time = self.lock.lock().await;
        let airdrop = self.rpc.request_airdrop(owner, FAUCET_LAMPORTS).await?;
        let payer = self.admin.pubkey();
        let ixs = [
            create_ata_idempotent(&payer, owner, &self.base_mint),
            create_ata_idempotent(&payer, owner, &self.quote_mint),
            mint_to(
                &self.base_mint,
                &ata(owner, &self.base_mint),
                &payer,
                self.base_atoms,
            ),
            mint_to(
                &self.quote_mint,
                &ata(owner, &self.quote_mint),
                &payer,
                self.quote_atoms,
            ),
        ];
        let signature = self.rpc.send_and_confirm(&ixs, &self.admin, &[]).await?;
        // The airdrop usually lands before the mint transaction; wait briefly for the rest.
        for _ in 0..40 {
            let status = self
                .rpc
                .signature_statuses(std::slice::from_ref(&airdrop))
                .await?;
            if matches!(status.first(), Some(Some(_))) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(FaucetResponse {
            signature,
            airdrop_signature: airdrop,
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ConfigView>,
    pub events: Events,
    pub control: SharedControl,
    /// `None` unless `--faucet` is set and the RPC is local.
    pub faucet: Option<Arc<Faucet>>,
}

/// `port` is the bound port; requests whose `Host` is not `127.0.0.1:port` or `localhost:port` get
/// 403 (DNS rebinding defense).
pub fn router(state: AppState, port: u16) -> Router {
    let origins: Vec<HeaderValue> = ALLOWED_ORIGINS
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE]);
    Router::new()
        .route("/config", get(config))
        .route("/stream", get(stream))
        .route("/control", post(control))
        .route("/faucet", post(faucet))
        .layer(cors)
        // Added last, so it runs first on every route (including CORS preflight).
        .layer(middleware::from_fn(move |req: Request, next: Next| async move {
            if host_allowed(req.headers(), port) {
                next.run(req).await
            } else {
                StatusCode::FORBIDDEN.into_response()
            }
        }))
        .with_state(state)
}

fn host_allowed(headers: &HeaderMap, port: u16) -> bool {
    let mut values = headers.get_all(header::HOST).iter();
    match (values.next().and_then(|h| h.to_str().ok()), values.next()) {
        (Some(host), None) => {
            host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
        }
        _ => false,
    }
}

/// Serves until the task is aborted. Bind first (with [`parse_bind`]) so errors surface early.
pub async fn serve(listener: TcpListener, state: AppState) -> anyhow::Result<()> {
    let port = listener.local_addr()?.port();
    axum::serve(listener, router(state, port)).await?;
    Ok(())
}

async fn config(State(s): State<AppState>) -> Json<ConfigView> {
    Json((*s.config).clone())
}

fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": message.into()})),
    )
        .into_response()
}

async fn control(
    State(s): State<AppState>,
    body: Result<Json<ControlRequest>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return bad_request(e.body_text()),
    };
    let now = Instant::now();
    let mut state = s.control.lock();
    if let Err(e) = state.apply(&req, now) {
        return bad_request(e);
    }
    let view = state.view(now);
    drop(state);
    info!(
        publishing = view.publishing,
        offset_bps = view.offset_bps,
        offset_seconds_left = format_args!("{:.1}", view.offset_seconds_left),
        "control: demo state changed (the band clamp still applies to the offset)"
    );
    Json(view).into_response()
}

#[derive(Debug, Deserialize)]
struct FaucetRequest {
    owner: String,
}

async fn faucet(
    State(s): State<AppState>,
    body: Result<Json<FaucetRequest>, JsonRejection>,
) -> Response {
    let Some(faucet) = &s.faucet else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "faucet is disabled (needs --faucet and a localhost RPC)"})),
        )
            .into_response();
    };
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return bad_request(e.body_text()),
    };
    let Ok(owner) = req.owner.parse::<Pubkey>() else {
        return bad_request("owner is not a valid base58 public key");
    };
    match faucet.fund(&owner).await {
        Ok(r) => {
            info!(%owner, "faucet: funded");
            Json(r).into_response()
        }
        Err(e) => {
            warn!(%owner, error = %e, "faucet failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    headers
        .get(header::ORIGIN)
        .is_none_or(|o| o.to_str().is_ok_and(|o| ALLOWED_ORIGINS.contains(&o)))
}

async fn stream(State(s): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    // Browsers do not apply CORS to WebSockets, so check the origin here.
    if !origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let rx = s.events.tx.subscribe();
    ws.on_upgrade(|socket| client(socket, rx))
}

/// Forwards broadcast messages until the client leaves, fails to keep up, or stalls.
async fn client(mut socket: WebSocket, mut rx: broadcast::Receiver<Arc<str>>) {
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Ok(text) => {
                    let send = socket.send(Message::text(String::from(&*text)));
                    if !matches!(tokio::time::timeout(SEND_TIMEOUT, send).await, Ok(Ok(()))) {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "dropping a slow stream client");
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.recv() => match incoming {
                None | Some(Err(_) | Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn state() -> AppState {
        let key = Pubkey::new_unique();
        let cfg = NetConfig {
            market: key,
            base_mint: key,
            quote_mint: key,
            base_vault: key,
            quote_vault: key,
            base_decimals: 9,
            quote_decimals: 6,
            admin_keypair: "/a".into(),
            updater_keypair: "/u".into(),
            trader_keypair: "/t".into(),
            rpc_url: "http://127.0.0.1:8899".into(),
            ws_url: "ws://127.0.0.1:8900".into(),
        };
        AppState {
            config: Arc::new(ConfigView::new(&cfg, &cfg.rpc_url, &cfg.ws_url)),
            events: Events::new(),
            control: SharedControl::default(),
            faucet: None,
        }
    }

    const PORT: u16 = 8788;

    async fn call(mut req: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
        if !req.headers().contains_key(header::HOST) {
            req.headers_mut()
                .insert(header::HOST, HeaderValue::from_static("127.0.0.1:8788"));
        }
        call_raw(req).await
    }

    async fn call_raw(req: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
        let resp = router(state(), PORT).oneshot(req).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or_default();
        (status, headers, body)
    }

    fn post_json(uri: &str, body: &str) -> Request<Body> {
        Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    #[test]
    fn control_transitions_and_offset_expiry() {
        let t0 = Instant::now();
        let mut c = ControlState::default();
        assert!(c.publishing());
        assert!((c.effective_offset(t0) - 0.0).abs() < 1e-12);

        let req = ControlRequest {
            publishing: Some(false),
            ..Default::default()
        };
        c.apply(&req, t0).unwrap();
        assert!(!c.publishing());

        // Default duration is 10 s; the offset lapses on its own.
        let req = ControlRequest {
            offset_bps: Some(50.0),
            ..Default::default()
        };
        c.apply(&req, t0).unwrap();
        assert!((c.effective_offset(t0 + Duration::from_secs(9)) - 50.0).abs() < 1e-12);
        assert!((c.effective_offset(t0 + Duration::from_secs(10)) - 0.0).abs() < 1e-12);
        assert!(!c.publishing(), "an offset request leaves publishing alone");

        // A new offset replaces the old one; 0 clears it.
        let req = ControlRequest {
            offset_bps: Some(-20.0),
            offset_seconds: Some(30.0),
            ..Default::default()
        };
        c.apply(&req, t0).unwrap();
        assert!((c.effective_offset(t0 + Duration::from_secs(29)) - -20.0).abs() < 1e-12);
        let view = c.view(t0 + Duration::from_secs(5));
        assert!((view.offset_seconds_left - 25.0).abs() < 1e-6);
        let req = ControlRequest {
            offset_bps: Some(0.0),
            ..Default::default()
        };
        c.apply(&req, t0).unwrap();
        assert!((c.effective_offset(t0) - 0.0).abs() < 1e-12);

        let req = ControlRequest {
            publishing: Some(true),
            ..Default::default()
        };
        c.apply(&req, t0).unwrap();
        assert!(c.publishing());
    }

    #[test]
    fn invalid_control_requests_change_nothing() {
        let t0 = Instant::now();
        let mut c = ControlState::default();
        for (bps, secs) in [
            (Some(f64::NAN), None),
            (Some(5_000.0), None),
            (Some(10.0), Some(0.0)),
            (Some(10.0), Some(1e6)),
            (None, Some(5.0)),
        ] {
            let req = ControlRequest {
                publishing: Some(false),
                offset_bps: bps,
                offset_seconds: secs,
            };
            assert!(c.apply(&req, t0).is_err());
            assert!(c.publishing(), "publishing must not change on error");
        }
    }

    #[test]
    fn bind_is_loopback_only() {
        assert!(parse_bind("127.0.0.1:8788").is_ok());
        for bad in [
            "0.0.0.0:8788",
            "192.168.1.5:8788",
            "[::1]:8788",
            "localhost:1",
            "8788",
        ] {
            assert!(parse_bind(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn faucet_needs_flag_and_local_rpc() {
        assert!(is_local_rpc("http://127.0.0.1:8899"));
        assert!(is_local_rpc("http://localhost:8899"));
        assert!(!is_local_rpc("https://api.devnet.solana.com"));
        assert!(!is_local_rpc("http://127.0.0.1.evil.com:8899"));
        assert!(!is_local_rpc("not a url"));
        for evil in [
            "http://localhost.evil.com:8899",
            "http://127.0.0.1.nip.io:8899",
            "http://127.0.0.1@evil.com:8899",
            "http://evil.com/?localhost",
            "http://evil.com/#127.0.0.1",
            "http://evil.com/127.0.0.1",
            "http://0.0.0.0:8899",
            "http://192.168.1.5:8899",
            "ftp://localhost:8899",
            "localhost:8899",
            "",
        ] {
            assert!(!is_local_rpc(evil), "{evil} must be rejected");
        }
        assert!(faucet_allowed(true, "http://localhost:8899"));
        assert!(!faucet_allowed(false, "http://localhost:8899"));
        assert!(!faucet_allowed(true, "https://api.devnet.solana.com"));
    }

    fn request_to(route: &str, host: Option<&str>) -> Request<Body> {
        let mut b = if route == "/config" || route == "/stream" {
            Request::get(route)
        } else {
            Request::post(route).header(header::CONTENT_TYPE, "application/json")
        };
        if let Some(h) = host {
            b = b.header(header::HOST, h);
        }
        let body = match route {
            "/control" => r#"{"publishing":true}"#,
            "/faucet" => "{}",
            _ => "",
        };
        b.body(Body::from(body)).unwrap()
    }

    #[tokio::test]
    async fn host_header_is_pinned_on_every_route() {
        for route in ["/config", "/stream", "/control", "/faucet"] {
            for host in ["127.0.0.1:8788", "localhost:8788"] {
                let (status, _, body) = call_raw(request_to(route, Some(host))).await;
                // The disabled faucet answers 403 with a JSON error; the host layer sends none.
                let host_denied = status == StatusCode::FORBIDDEN && body.get("error").is_none();
                assert!(!host_denied, "{host} on {route}");
            }
            for host in [
                Some("evil.com:8788"),
                Some("127.0.0.1:9999"),
                Some("127.0.0.1.nip.io:8788"),
                Some("127.0.0.1"),
                None,
            ] {
                let (status, _, _) = call_raw(request_to(route, host)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{host:?} on {route}");
            }
        }
        let (status, _, _) = call_raw(request_to("/config", Some("127.0.0.1:8788"))).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn faucet_is_forbidden_when_disabled() {
        let owner = Pubkey::new_unique();
        let (status, _, _) = call(post_json("/faucet", &format!(r#"{{"owner":"{owner}"}}"#))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn config_and_control_endpoints() {
        let (status, _, body) = call(Request::get("/config").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["programId"], PROGRAM_ID.to_string());
        assert_eq!(body["baseDecimals"], 9);
        assert_eq!(body["tokenProgram"], TOKEN_PROGRAM_ID.to_string());
        assert_eq!(body["rpcUrl"], "http://127.0.0.1:8899");

        let (status, _, body) = call(post_json(
            "/control",
            r#"{"publishing":false,"offsetBps":50,"offsetSeconds":10}"#,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["publishing"], false);
        assert_eq!(body["offsetBps"], 50.0);

        let (status, _, _) = call(post_json("/control", r#"{"offsetBps":1e9}"#)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = call(post_json("/control", r#"{"nope":1}"#)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn config_exposes_no_secrets() {
        let (_, _, body) = call(Request::get("/config").body(Body::empty()).unwrap()).await;
        let mut keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "baseDecimals",
                "baseMint",
                "baseVault",
                "market",
                "programId",
                "quoteDecimals",
                "quoteMint",
                "quoteVault",
                "rpcUrl",
                "tokenProgram",
                "wsUrl"
            ]
        );
        assert!(!body.to_string().contains("keypair"));
    }

    #[tokio::test]
    async fn malformed_or_out_of_range_input_is_rejected_without_panic() {
        for body in [
            "",
            "{",
            "null",
            "[]",
            "\"x\"",
            r#"{"publishing":"yes"}"#,
            r#"{"offsetBps":"5"}"#,
            r#"{"offsetBps":-1001}"#,
            r#"{"offsetBps":1001}"#,
            r#"{"offsetBps":5,"offsetSeconds":0}"#,
            r#"{"offsetBps":5,"offsetSeconds":-1}"#,
            r#"{"offsetBps":5,"offsetSeconds":301}"#,
            r#"{"offsetSeconds":5}"#,
            r#"{"offsetBps":1e999}"#,
        ] {
            let (status, _, _) = call(post_json("/control", body)).await;
            assert!(status.is_client_error(), "{body:?} gave {status}");
        }
        let req = Request::post("/control")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from(r#"{"publishing":false}"#))
            .unwrap();
        let (status, _, _) = call(req).await;
        assert!(status.is_client_error(), "{status}");
        // The faucet is checked for malformed input only when it is enabled; disabled it is 403.
        let (status, _, _) = call(post_json("/faucet", "{")).await;
        assert!(status.is_client_error());
    }

    #[tokio::test]
    async fn cors_allows_only_the_vite_origins() {
        for origin in ALLOWED_ORIGINS {
            let req = Request::options("/control")
                .header(header::ORIGIN, origin)
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
                .body(Body::empty())
                .unwrap();
            let (_, headers, _) = call(req).await;
            assert_eq!(
                headers
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .map(HeaderValue::as_bytes),
                Some(origin.as_bytes())
            );
        }
        let req = Request::options("/control")
            .header(header::ORIGIN, "http://evil.example")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
            .body(Body::empty())
            .unwrap();
        let (_, headers, _) = call(req).await;
        assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    }

    #[test]
    fn stream_origin_check() {
        let mut h = HeaderMap::new();
        assert!(origin_allowed(&h), "non-browser clients send no Origin");
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://localhost:5173"),
        );
        assert!(origin_allowed(&h));
        h.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://evil.example"),
        );
        assert!(!origin_allowed(&h));
        for bad in [
            "null",
            "http://localhost:5173/",
            "http://localhost:5173.evil.com",
            "https://localhost:5173",
            "http://localhost:5174",
        ] {
            h.insert(header::ORIGIN, HeaderValue::from_str(bad).unwrap());
            assert!(!origin_allowed(&h), "{bad} must be rejected");
        }
    }

    /// The literal from `docs/demo-contract.md`, filled with example values.
    const TICK_JSON: &str = r#"{"type":"tick","tsMs":1790000000000,"venues":{"binance_perp":{"bid":200.0,"ask":200.02,"mid":200.01},"coinbase":{"bid":199.9,"ask":200.1,"mid":200.0}},"fair":{"price":200.01,"confBps":0.5,"live":2},"chain":{"slot":1000,"mid":200.0,"lastUpdateSlot":998,"ageSlots":2,"maxAgeSlots":20,"paused":false},"inventory":{"base":1000.0,"quote":200000.0,"skewBps":-0.5},"pythDevBps":1.5,"publishing":true,"offsetBps":0.0}"#;

    #[test]
    fn tick_matches_the_contract_shape() {
        let msg: ServerMsg = serde_json::from_str(TICK_JSON).unwrap();
        let ServerMsg::Tick(tick) = &msg else {
            panic!("not a tick");
        };
        assert_eq!(tick.venues.len(), 2);
        assert_eq!(tick.chain.as_ref().map(|c| c.age_slots), Some(2));
        // Serializing gives back the same JSON value (field names, nesting, tag).
        let expected: serde_json::Value = serde_json::from_str(TICK_JSON).unwrap();
        assert_eq!(serde_json::to_value(&msg).unwrap(), expected);
    }

    #[test]
    fn empty_tick_keeps_null_fields() {
        let tick = ServerMsg::Tick(Tick {
            ts_ms: 1,
            venues: BTreeMap::new(),
            fair: None,
            chain: None,
            inventory: None,
            pyth_dev_bps: None,
            publishing: false,
            offset_bps: 0.0,
        });
        assert_eq!(
            serde_json::to_value(&tick).unwrap(),
            json!({"type":"tick","tsMs":1,"venues":{},"fair":null,"chain":null,
                "inventory":null,"pythDevBps":null,"publishing":false,"offsetBps":0.0})
        );
    }

    #[test]
    fn update_and_swap_shapes() {
        let update = ServerMsg::Update(UpdateEvent {
            signature: "s".into(),
            target_slot: 10,
            landed_slot: None,
            delay_slots: None,
            mid: 200.5,
            status: UpdateStatus::Sent,
        });
        assert_eq!(
            serde_json::to_value(&update).unwrap(),
            json!({"type":"update","signature":"s","targetSlot":10,"landedSlot":null,
                "delaySlots":null,"mid":200.5,"status":"sent"})
        );
        let swap = ServerMsg::Swap(SwapMsg {
            signature: "s".into(),
            slot: 11,
            side: "buy".into(),
            amount_in: "5".into(),
            amount_out: "6".into(),
            trader: "t".into(),
        });
        assert_eq!(
            serde_json::to_value(&swap).unwrap(),
            json!({"type":"swap","signature":"s","slot":11,"side":"buy","amountIn":"5",
                "amountOut":"6","trader":"t"})
        );
    }

    fn event_log(market: &Pubkey, trader: &Pubkey, side: u8) -> String {
        let mut bytes = propamm_client::SWAP_EVENT_DISCRIMINATOR.to_vec();
        bytes.extend_from_slice(market.as_ref());
        bytes.extend_from_slice(trader.as_ref());
        bytes.push(side);
        for n in [1_000_u64, 5, 5, 200, 77] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        format!("Program data: {}", B64.encode(bytes))
    }

    #[test]
    fn swap_events_are_decoded_from_logs() {
        let (market, trader) = (Pubkey::new_unique(), Pubkey::new_unique());
        let logs = vec![
            "Program x invoke [1]".to_owned(),
            event_log(&market, &trader, 1),
            event_log(&Pubkey::new_unique(), &trader, 0),
            "Program data: !!notbase64".to_owned(),
        ];
        let swaps = swaps_in_logs(&market, "sig", &logs);
        assert_eq!(
            swaps,
            vec![SwapMsg {
                signature: "sig".into(),
                slot: 77,
                side: "sell".into(),
                amount_in: "1000".into(),
                amount_out: "5".into(),
                trader: trader.to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn events_are_skipped_without_clients_and_slow_ones_lag() {
        let events = Events::new();
        let msg = ServerMsg::Update(UpdateEvent {
            signature: "s".into(),
            target_slot: 1,
            landed_slot: None,
            delay_slots: None,
            mid: 1.0,
            status: UpdateStatus::Sent,
        });
        events.send(&msg); // no receivers: no-op
        let mut rx = events.tx.subscribe();
        assert!(events.has_clients());
        for _ in 0..BROADCAST_CAPACITY + 10 {
            events.send(&msg);
        }
        // The channel is bounded: a receiver that did not keep up sees `Lagged`.
        assert!(matches!(
            rx.recv().await,
            Err(broadcast::error::RecvError::Lagged(_))
        ));
    }
}
