//! WebSocket subscriptions to the validator: current slot, the Market account and (optionally)
//! the program logs.
//! Reconnects with a short backoff; the caller also polls over RPC, so a gap here is harmless.

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEvent {
    Slot(u64),
    /// Market account bytes and the slot of the notification context.
    Market {
        data: Vec<u8>,
        slot: u64,
    },
    /// Logs of a transaction that mentions the program (`logsSubscribe`).
    Logs {
        signature: String,
        /// True when the transaction failed.
        failed: bool,
        logs: Vec<String>,
    },
}

/// Decodes one notification frame; `None` for acks and anything else.
pub fn parse_notification(text: &str) -> Option<WsEvent> {
    let v: Value = serde_json::from_str(text).ok()?;
    let result = v.pointer("/params/result")?;
    match v.get("method")?.as_str()? {
        "slotNotification" => Some(WsEvent::Slot(result.get("slot")?.as_u64()?)),
        "accountNotification" => {
            let data = B64
                .decode(result.pointer("/value/data/0")?.as_str()?)
                .ok()?;
            Some(WsEvent::Market {
                data,
                slot: result.pointer("/context/slot")?.as_u64()?,
            })
        }
        "logsNotification" => {
            let value = result.get("value")?;
            Some(WsEvent::Logs {
                signature: value.get("signature")?.as_str()?.to_owned(),
                failed: !value.get("err")?.is_null(),
                logs: value
                    .get("logs")?
                    .as_array()?
                    .iter()
                    .filter_map(|l| l.as_str().map(str::to_owned))
                    .collect(),
            })
        }
        _ => None,
    }
}

/// Subscribes to `slotSubscribe` and `accountSubscribe(market, processed)`, plus
/// `logsSubscribe(mentions program, processed)` when `program` is set, and forwards events
/// until the receiver is dropped.
pub async fn run(url: String, market: Pubkey, program: Option<Pubkey>, tx: mpsc::Sender<WsEvent>) {
    loop {
        match session(&url, &market, program.as_ref(), &tx).await {
            Ok(()) => return,
            Err(e) => warn!(error = %e, "ws session ended, reconnecting"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `Ok` when the receiver is gone, `Err` when the connection should be retried.
async fn session(
    url: &str,
    market: &Pubkey,
    program: Option<&Pubkey>,
    tx: &mpsc::Sender<WsEvent>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws, _) = connect_async(url).await?;
    let mut subs = vec![
        json!({"jsonrpc": "2.0", "id": 1, "method": "slotSubscribe"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "accountSubscribe",
            "params": [market.to_string(), {"encoding": "base64", "commitment": "processed"}]}),
    ];
    if let Some(program) = program {
        subs.push(json!({"jsonrpc": "2.0", "id": 3, "method": "logsSubscribe",
            "params": [{"mentions": [program.to_string()]}, {"commitment": "processed"}]}));
    }
    for s in subs {
        ws.send(Message::text(s.to_string())).await?;
    }
    // A healthy validator sends a slot every few hundred ms; silence means a dead socket.
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| "ws silent for 10 s")?;
        match frame {
            Some(Ok(Message::Text(text))) => {
                if let Some(ev) = parse_notification(&text)
                    && tx.send(ev).await.is_err()
                {
                    return Ok(());
                }
            }
            Some(Ok(Message::Close(_))) | None => return Err("ws closed".into()),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_slot_and_account_notifications() {
        let slot = r#"{"jsonrpc":"2.0","method":"slotNotification","params":{"result":{"parent":9,"root":3,"slot":10},"subscription":1}}"#;
        assert_eq!(parse_notification(slot), Some(WsEvent::Slot(10)));
        let account = r#"{"jsonrpc":"2.0","method":"accountNotification","params":{"result":{"context":{"slot":12},"value":{"data":["AQID","base64"],"lamports":1}},"subscription":2}}"#;
        assert_eq!(
            parse_notification(account),
            Some(WsEvent::Market {
                data: vec![1, 2, 3],
                slot: 12
            })
        );
        assert_eq!(
            parse_notification(r#"{"jsonrpc":"2.0","result":5,"id":1}"#),
            None
        );
    }

    #[test]
    fn parses_logs_notifications() {
        let ok = r#"{"jsonrpc":"2.0","method":"logsNotification","params":{"result":{"context":{"slot":7},"value":{"signature":"sig1","err":null,"logs":["Program x invoke [1]","Program data: AAEC"]}},"subscription":3}}"#;
        assert_eq!(
            parse_notification(ok),
            Some(WsEvent::Logs {
                signature: "sig1".into(),
                failed: false,
                logs: vec!["Program x invoke [1]".into(), "Program data: AAEC".into()],
            })
        );
        let failed = r#"{"jsonrpc":"2.0","method":"logsNotification","params":{"result":{"context":{"slot":7},"value":{"signature":"sig2","err":{"InstructionError":[0,{"Custom":6014}]},"logs":[]}},"subscription":3}}"#;
        assert!(matches!(
            parse_notification(failed),
            Some(WsEvent::Logs { failed: true, .. })
        ));
    }
}
