//! Minimal Solana JSON-RPC client, transaction building and the few SPL Token / ATA / compute
//! budget instructions the bot needs. Hand-rolled over `reqwest` instead of `solana-client`,
//! which would pull in a very large dependency tree for six RPC methods.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::{Pubkey, pubkey};
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;

use propamm_client::{SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID};

pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    pubkey!("ComputeBudget111111111111111111111111111111");
pub const ATA_PROGRAM_ID: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// Size of an SPL Token mint account.
pub const MINT_SIZE: u64 = 82;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("rpc error {code}: {message}{}", fmt_logs(.logs))]
    Rpc {
        code: i64,
        message: String,
        logs: Vec<String>,
    },
    #[error("unexpected rpc response: {0}")]
    Decode(String),
    #[error("transaction failed: {0}")]
    TxFailed(String),
    #[error("timed out waiting for {0}")]
    Timeout(String),
}

fn fmt_logs(logs: &[String]) -> String {
    if logs.is_empty() {
        String::new()
    } else {
        format!("\n  {}", logs.join("\n  "))
    }
}

impl RpcError {
    /// The program's custom error code, if this error carries one ("custom program error: 0x..").
    pub fn custom_code(&self) -> Option<u32> {
        let text = match self {
            Self::Rpc { message, logs, .. } => format!("{message} {}", logs.join(" ")),
            Self::TxFailed(m) => m.clone(),
            _ => return None,
        };
        custom_code_in(&text)
    }
}

/// Parses `custom program error: 0x177e` or `{"Custom":6014}` out of an error text.
pub fn custom_code_in(text: &str) -> Option<u32> {
    if let Some(rest) = text.split("custom program error: 0x").nth(1) {
        let hex: String = rest.chars().take_while(char::is_ascii_hexdigit).collect();
        return u32::from_str_radix(&hex, 16).ok();
    }
    let rest = text.split("\"Custom\":").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[derive(Debug, Clone)]
pub struct AccountData {
    pub data: Vec<u8>,
    pub lamports: u64,
    pub owner: Pubkey,
}

#[derive(Debug, Clone)]
pub struct SigStatus {
    pub slot: u64,
    /// Raw `err` field; `None` when the transaction succeeded.
    pub err: Option<Value>,
}

/// Attempts per call (the first try plus five retries).
const MAX_ATTEMPTS: u32 = 6;
const BACKOFF_BASE: Duration = Duration::from_millis(250);
const BACKOFF_CAP: Duration = Duration::from_secs(8);

/// Delay before retry number `retry` (0-based): `250 ms * 2^retry`, capped at 8 s, scaled by
/// `jitter` in `[0, 1]` into `[50%, 100%]` of that value. A server `Retry-After` is a floor
/// (itself capped at 8 s).
fn backoff_delay(retry: u32, jitter: f64, retry_after: Option<Duration>) -> Duration {
    let exp = BACKOFF_BASE
        .saturating_mul(1_u32.checked_shl(retry).unwrap_or(u32::MAX))
        .min(BACKOFF_CAP);
    let delay = exp.mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0));
    retry_after.map_or(delay, |ra| ra.min(BACKOFF_CAP).max(delay))
}

/// `Retry-After` in delta-seconds form (the HTTP-date form is not used by Solana RPC hosts).
fn parse_retry_after(value: &str) -> Option<Duration> {
    let secs: f64 = value.trim().parse().ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs.min(3600.0)))
}

/// HTTP statuses worth retrying: rate limiting and transient server errors.
fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || matches!(status.as_u16(), 500 | 502 | 503 | 504)
}

/// Retry counters shared by all clones of an [`Rpc`].
#[derive(Debug, Default)]
struct RetryStats {
    rate_limited: AtomicU64,
    retries: AtomicU64,
}

/// A failed attempt: retryable (with the server's `Retry-After`, if any) or final.
enum Attempt {
    Transient(RpcError, Option<Duration>),
    Final(RpcError),
}

impl From<Attempt> for RpcError {
    fn from(a: Attempt) -> Self {
        match a {
            Attempt::Transient(e, _) | Attempt::Final(e) => e,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Rpc {
    http: reqwest::Client,
    url: String,
    stats: Arc<RetryStats>,
}

fn field_u64(v: &Value, key: &str) -> Result<u64, RpcError> {
    v.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::Decode(format!("missing `{key}` in {v}")))
}

fn decode_err(v: &Value) -> RpcError {
    RpcError::Decode(v.to_string())
}

fn as_u64(v: &Value) -> Result<u64, RpcError> {
    v.as_u64().ok_or_else(|| decode_err(v))
}

fn as_string(v: &Value) -> Result<String, RpcError> {
    v.as_str().map(str::to_owned).ok_or_else(|| decode_err(v))
}

/// The strings of a JSON array; empty when `v` is missing or not an array.
fn strings(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

impl Rpc {
    pub fn new(url: &str) -> Result<Self, RpcError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            url: url.to_owned(),
            stats: Arc::default(),
        })
    }

    /// `(HTTP 429 responses, retries)` seen so far.
    pub fn retry_stats(&self) -> (u64, u64) {
        (
            self.stats.rate_limited.load(Ordering::Relaxed),
            self.stats.retries.load(Ordering::Relaxed),
        )
    }

    /// One JSON-RPC call, retried on 429, transient 5xx and transport timeouts with exponential
    /// backoff and jitter. Every method here is a read or a resend of the same signed
    /// transaction (same signature, so at most one lands), so a retry is safe.
    /// `requestAirdrop` is the exception and goes through `call_once` directly.
    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let mut retry = 0;
        loop {
            let (err, retry_after) = match self.call_once(method, &params).await {
                Ok(v) => return Ok(v),
                Err(Attempt::Final(e)) => return Err(e),
                Err(Attempt::Transient(e, ra)) => (e, ra),
            };
            if retry + 1 >= MAX_ATTEMPTS {
                return Err(err);
            }
            let delay = backoff_delay(retry, fastrand::f64(), retry_after);
            retry += 1;
            self.stats.retries.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                method,
                attempt = retry,
                delay_ms = delay.as_millis(),
                error = %err,
                "rpc retry"
            );
            tokio::time::sleep(delay).await;
        }
    }

    async fn call_once(&self, method: &str, params: &Value) -> Result<Value, Attempt> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let http = match self.http.post(&self.url).json(&body).send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() || e.is_connect() => {
                return Err(Attempt::Transient(e.into(), None));
            }
            Err(e) => return Err(Attempt::Final(e.into())),
        };
        let status = http.status();
        let retry_after = http
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        if retryable_status(status) {
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                self.stats.rate_limited.fetch_add(1, Ordering::Relaxed);
            }
            let err = http.error_for_status().err().map_or_else(
                || RpcError::Decode(format!("http {status}")),
                RpcError::from,
            );
            return Err(Attempt::Transient(err, retry_after));
        }
        let resp: Value = match http.error_for_status() {
            Ok(r) => r.json().await.map_err(|e| Attempt::Final(e.into()))?,
            Err(e) => return Err(Attempt::Final(e.into())),
        };
        if let Some(err) = resp.get("error") {
            return Err(Attempt::Final(RpcError::Rpc {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                logs: strings(err.pointer("/data/logs")),
            }));
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| Attempt::Final(decode_err(&resp)))
    }

    pub async fn get_slot(&self) -> Result<u64, RpcError> {
        let v = self
            .call("getSlot", json!([{"commitment": "processed"}]))
            .await?;
        as_u64(&v)
    }

    pub async fn latest_blockhash(&self) -> Result<Hash, RpcError> {
        let v = self
            .call("getLatestBlockhash", json!([{"commitment": "confirmed"}]))
            .await?;
        v.pointer("/value/blockhash")
            .and_then(Value::as_str)
            .and_then(|s| Hash::from_str(s).ok())
            .ok_or_else(|| decode_err(&v))
    }

    /// Accounts at `processed` commitment, with the slot of the response context.
    pub async fn get_accounts(
        &self,
        keys: &[Pubkey],
    ) -> Result<(u64, Vec<Option<AccountData>>), RpcError> {
        let list: Vec<String> = keys.iter().map(ToString::to_string).collect();
        let v = self
            .call(
                "getMultipleAccounts",
                json!([list, {"encoding": "base64", "commitment": "processed"}]),
            )
            .await?;
        let slot = v
            .pointer("/context/slot")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let values = v
            .pointer("/value")
            .and_then(Value::as_array)
            .ok_or_else(|| decode_err(&v))?;
        let accounts = values
            .iter()
            .map(parse_account)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((slot, accounts))
    }

    pub async fn get_account(&self, key: &Pubkey) -> Result<Option<AccountData>, RpcError> {
        let (_, mut accounts) = self.get_accounts(&[*key]).await?;
        Ok(accounts.pop().flatten())
    }

    pub async fn balance(&self, key: &Pubkey) -> Result<u64, RpcError> {
        let v = self
            .call(
                "getBalance",
                json!([key.to_string(), {"commitment": "confirmed"}]),
            )
            .await?;
        field_u64(&v, "value")
    }

    pub async fn rent_exempt_minimum(&self, space: u64) -> Result<u64, RpcError> {
        let v = self
            .call("getMinimumBalanceForRentExemption", json!([space]))
            .await?;
        as_u64(&v)
    }

    pub async fn request_airdrop(&self, key: &Pubkey, lamports: u64) -> Result<String, RpcError> {
        // Not idempotent: a retried airdrop could pay out twice.
        let v = self
            .call_once("requestAirdrop", &json!([key.to_string(), lamports]))
            .await?;
        as_string(&v)
    }

    /// Sends a signed, serialized transaction. Returns the signature string. Retried on
    /// transient errors: the same signed bytes carry the same signature, so a duplicate send
    /// cannot execute twice.
    pub async fn send_raw(&self, tx: &[u8], skip_preflight: bool) -> Result<String, RpcError> {
        let v = self
            .call(
                "sendTransaction",
                json!([B64.encode(tx), {
                    "encoding": "base64",
                    "skipPreflight": skip_preflight,
                    "preflightCommitment": "processed",
                }]),
            )
            .await?;
        as_string(&v)
    }

    /// Status per signature (`None` while unknown), at any commitment level.
    pub async fn signature_statuses(
        &self,
        sigs: &[String],
    ) -> Result<Vec<Option<SigStatus>>, RpcError> {
        let v = self.call("getSignatureStatuses", json!([sigs])).await?;
        let values = v
            .pointer("/value")
            .and_then(Value::as_array)
            .ok_or_else(|| decode_err(&v))?;
        Ok(values
            .iter()
            .map(|s| {
                if s.is_null() {
                    return None;
                }
                Some(SigStatus {
                    slot: s.get("slot").and_then(Value::as_u64).unwrap_or(0),
                    err: s.get("err").filter(|e| !e.is_null()).cloned(),
                })
            })
            .collect())
    }

    /// Log messages of a confirmed transaction.
    pub async fn transaction_logs(&self, sig: &str) -> Result<Vec<String>, RpcError> {
        let v = self
            .call(
                "getTransaction",
                json!([sig, {"encoding": "json", "commitment": "confirmed",
                    "maxSupportedTransactionVersion": 0}]),
            )
            .await?;
        Ok(strings(v.pointer("/meta/logMessages")))
    }

    /// Signs and sends `ixs` (preflight on), then waits until the transaction is processed.
    /// A program error comes back as `RpcError::Rpc` (preflight) or `RpcError::TxFailed`.
    pub async fn send_and_confirm(
        &self,
        ixs: &[Instruction],
        payer: &Keypair,
        extra_signers: &[&Keypair],
    ) -> Result<String, RpcError> {
        let hash = self.latest_blockhash().await?;
        let (_, wire) = build_tx(ixs, payer, extra_signers, hash)?;
        let sig = self.send_raw(&wire, false).await?;
        for _ in 0..120 {
            if let Some(Some(status)) = self
                .signature_statuses(std::slice::from_ref(&sig))
                .await?
                .pop()
            {
                return match status.err {
                    Some(e) => Err(RpcError::TxFailed(e.to_string())),
                    None => Ok(sig),
                };
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(RpcError::Timeout(sig))
    }
}

fn parse_account(a: &Value) -> Result<Option<AccountData>, RpcError> {
    if a.is_null() {
        return Ok(None);
    }
    let bad = || decode_err(a);
    let data = a
        .pointer("/data/0")
        .and_then(Value::as_str)
        .ok_or_else(bad)
        .and_then(|s| B64.decode(s).map_err(|_| bad()))?;
    let owner = a
        .get("owner")
        .and_then(Value::as_str)
        .and_then(|s| Pubkey::from_str(s).ok())
        .ok_or_else(bad)?;
    Ok(Some(AccountData {
        data,
        lamports: field_u64(a, "lamports")?,
        owner,
    }))
}

/// Builds and signs a legacy transaction. Returns the first signature and the wire bytes.
pub fn build_tx(
    ixs: &[Instruction],
    payer: &Keypair,
    extra_signers: &[&Keypair],
    blockhash: Hash,
) -> Result<(Signature, Vec<u8>), RpcError> {
    let message = Message::new_with_blockhash(ixs, Some(&payer.pubkey()), &blockhash);
    let mut signers = vec![payer];
    signers.extend_from_slice(extra_signers);
    let tx = Transaction::new(&signers, message, blockhash);
    // Wire format: short-vec signature count, signatures, message.
    let mut wire = shortvec_len(tx.signatures.len())?;
    for sig in &tx.signatures {
        wire.extend_from_slice(sig.as_ref());
    }
    wire.extend_from_slice(&tx.message_data());
    let sig = tx
        .signatures
        .first()
        .copied()
        .ok_or_else(|| RpcError::Decode("transaction has no signature".into()))?;
    Ok((sig, wire))
}

/// Solana short-vec length: little-endian base-128, seven bits per byte, high bit = more bytes.
/// The length is at most `u16::MAX`.
fn shortvec_len(len: usize) -> Result<Vec<u8>, RpcError> {
    let mut rest = u16::try_from(len).map_err(|_| RpcError::Decode("length too large".into()))?;
    let mut out = Vec::new();
    loop {
        let low = u8::try_from(rest & 0x7f).unwrap_or(0);
        rest >>= 7;
        if rest == 0 {
            out.push(low);
            return Ok(out);
        }
        out.push(low | 0x80);
    }
}

/// Compute budget `SetComputeUnitLimit`.
pub fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = vec![2];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: vec![],
        data,
    }
}

/// Compute budget `SetComputeUnitPrice` (micro-lamports per CU).
pub fn set_compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = vec![3];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_PROGRAM_ID,
        accounts: vec![],
        data,
    }
}

/// System program `CreateAccount`.
pub fn create_account(
    payer: &Pubkey,
    new: &Pubkey,
    lamports: u64,
    space: u64,
    owner: &Pubkey,
) -> Instruction {
    let mut data = 0_u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&space.to_le_bytes());
    data.extend_from_slice(owner.as_ref());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*payer, true), AccountMeta::new(*new, true)],
        data,
    }
}

/// System program `Transfer`.
pub fn transfer(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = 2_u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

/// SPL Token `InitializeMint2` without a freeze authority.
pub fn initialize_mint2(mint: &Pubkey, decimals: u8, authority: &Pubkey) -> Instruction {
    let mut data = vec![20, decimals];
    data.extend_from_slice(authority.as_ref());
    data.push(0);
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![AccountMeta::new(*mint, false)],
        data,
    }
}

/// SPL Token `MintTo`.
pub fn mint_to(mint: &Pubkey, dest: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = vec![7];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// Associated token account address of `owner` for `mint`.
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// Associated token program `CreateIdempotent`.
pub fn create_ata_idempotent(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(owner, mint), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: vec![1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_250ms_and_caps_at_8s() {
        let ms = |r| backoff_delay(r, 1.0, None).as_millis();
        assert_eq!(
            (0..8).map(ms).collect::<Vec<_>>(),
            [250, 500, 1000, 2000, 4000, 8000, 8000, 8000]
        );
        assert_eq!(backoff_delay(u32::MAX, 1.0, None), BACKOFF_CAP);
    }

    #[test]
    fn jitter_spans_half_to_full_delay() {
        assert_eq!(backoff_delay(2, 0.0, None), Duration::from_millis(500));
        assert_eq!(backoff_delay(2, 1.0, None), Duration::from_secs(1));
        assert_eq!(backoff_delay(2, 9.0, None), Duration::from_secs(1));
    }

    #[test]
    fn retry_after_is_a_floor_capped_at_the_backoff_cap() {
        let ra = |s| Some(Duration::from_secs(s));
        assert_eq!(backoff_delay(0, 1.0, ra(3)), Duration::from_secs(3));
        assert_eq!(backoff_delay(3, 1.0, ra(1)), Duration::from_secs(2));
        assert_eq!(backoff_delay(0, 1.0, ra(60)), BACKOFF_CAP);
    }

    #[test]
    fn retry_after_parses_seconds_only() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_retry_after(" 1.5 "),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(parse_retry_after("0"), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after("NaN"), None);
        assert_eq!(parse_retry_after("inf"), None);
        assert_eq!(parse_retry_after("1e400"), None);
        assert_eq!(parse_retry_after("1e300"), Some(Duration::from_hours(1)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn only_rate_limits_and_transient_server_errors_retry() {
        for (code, retry) in [
            (429, true),
            (500, true),
            (502, true),
            (503, true),
            (504, true),
            (200, false),
            (400, false),
            (401, false),
            (403, false),
            (404, false),
        ] {
            let status = reqwest::StatusCode::from_u16(code).unwrap();
            assert_eq!(retryable_status(status), retry, "{code}");
        }
    }

    type Seen = Arc<std::sync::Mutex<Vec<String>>>;

    /// Serves `responses` (raw HTTP) one per connection on a loopback port, then closes the
    /// listener. Returns the URL and the raw requests received.
    async fn serve_once_each(responses: Vec<String>) -> (String, Seen) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Seen::default();
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            for r in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                sock.write_all(r.as_bytes()).await.unwrap();
            }
        });
        (url, seen)
    }

    fn http(status: &str, extra: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\n{extra}content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn a_429_with_retry_after_is_retried_and_counted() {
        let (url, _) = serve_once_each(vec![
            http("429 Too Many Requests", "retry-after: 0\r\n", ""),
            http("200 OK", "", r#"{"jsonrpc":"2.0","id":1,"result":42}"#),
        ])
        .await;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rpc = Rpc::new(&url).unwrap();
        assert_eq!(rpc.get_slot().await.unwrap(), 42);
        assert_eq!(rpc.retry_stats(), (1, 1));
    }

    #[tokio::test]
    async fn rpc_errors_are_not_retried() {
        let (url, seen) = serve_once_each(vec![http(
            "200 OK",
            "",
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32002,"message":"boom"}}"#,
        )])
        .await;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rpc = Rpc::new(&url).unwrap();
        assert!(matches!(rpc.get_slot().await, Err(RpcError::Rpc { .. })));
        assert_eq!(rpc.retry_stats(), (0, 0));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn send_raw_retries_with_identical_bytes() {
        let (url, seen) = serve_once_each(vec![
            http("503 Service Unavailable", "", ""),
            http("200 OK", "", r#"{"jsonrpc":"2.0","id":1,"result":"sig"}"#),
        ])
        .await;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rpc = Rpc::new(&url).unwrap();
        assert_eq!(rpc.send_raw(&[1, 2, 3], true).await.unwrap(), "sig");
        let seen = seen.lock().unwrap();
        let bodies: Vec<_> = seen.iter().map(|r| r.split("\r\n\r\n").nth(1)).collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0], bodies[1]);
    }

    #[tokio::test]
    async fn request_airdrop_is_never_retried() {
        let (url, seen) = serve_once_each(vec![http("429 Too Many Requests", "", "")]).await;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rpc = Rpc::new(&url).unwrap();
        assert!(rpc.request_airdrop(&Pubkey::default(), 1).await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(rpc.retry_stats().1, 0);
    }

    #[test]
    fn custom_codes_are_parsed_from_both_forms() {
        assert_eq!(
            custom_code_in("Program failed: custom program error: 0x177e"),
            Some(6014)
        );
        assert_eq!(
            custom_code_in(r#"{"InstructionError":[0,{"Custom":6013}]}"#),
            Some(6013)
        );
        assert_eq!(custom_code_in("blockhash not found"), None);
    }

    #[test]
    fn system_transfer_encoding() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let ix = transfer(&a, &b, 5);
        assert_eq!(ix.data, [2, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0]);
        assert!(ix.accounts[0].is_signer && ix.accounts[0].is_writable);
        assert!(!ix.accounts[1].is_signer && ix.accounts[1].is_writable);
    }

    #[test]
    fn compute_budget_encoding() {
        assert_eq!(set_compute_unit_limit(4_000).data, [2, 0xa0, 0x0f, 0, 0]);
        assert_eq!(set_compute_unit_price(1).data, [3, 1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn built_transactions_carry_a_signature() {
        let kp = Keypair::new();
        let ix = set_compute_unit_limit(1);
        let (sig, wire) = build_tx(&[ix], &kp, &[], Hash::default()).unwrap();
        // 1 signature byte-count + 64-byte signature come first on the wire.
        assert_eq!(wire[0], 1);
        assert_eq!(&wire[1..65], sig.as_ref());
    }

    #[test]
    fn signatures_follow_the_message_header_order() {
        let (payer, other) = (Keypair::new(), Keypair::new());
        let ix = Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![AccountMeta::new_readonly(other.pubkey(), true)],
            data: vec![],
        };
        let (sig, wire) = build_tx(&[ix], &payer, &[&other], Hash::default()).unwrap();
        assert_eq!(wire[0], 2);
        // The fee payer is the first signer key and signs first.
        assert_eq!(&wire[1..65], sig.as_ref());
        let message = &wire[1 + 2 * 64..];
        assert_eq!(payer.sign_message(message).as_ref(), &wire[1..65]);
        assert_eq!(other.sign_message(message).as_ref(), &wire[65..129]);
    }

    #[test]
    fn shortvec_encoding() {
        let enc = |n| shortvec_len(n).unwrap();
        assert_eq!(enc(0), [0]);
        assert_eq!(enc(1), [1]);
        assert_eq!(enc(127), [0x7f]);
        assert_eq!(enc(128), [0x80, 0x01]);
        assert_eq!(enc(255), [0xff, 0x01]);
        assert_eq!(enc(16_383), [0xff, 0x7f]);
        assert_eq!(enc(16_384), [0x80, 0x80, 0x01]);
        assert_eq!(enc(65_535), [0xff, 0xff, 0x03]);
        assert!(shortvec_len(65_536).is_err());
    }
}
