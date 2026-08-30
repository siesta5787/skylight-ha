//! Home Assistant WebSocket client (`/api/websocket`): auth handshake,
//! request/response commands, and a broadcast of `state_changed` events.
//!
//! This models a single connection. If the connection dies (network drop,
//! HA restart), all `Client` methods will start returning `Error::Closed`
//! and the event broadcast channel closes. The caller is expected to notice
//! that and call [`connect_with_backoff`] again to get a fresh `Client` —
//! this crate deliberately doesn't hide a full reconnect loop behind a
//! single long-lived handle, since the app layer needs to know when it's
//! showing stale data either way.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};

use crate::entities::EntityState;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("websocket error: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HA rejected the access token")]
    AuthInvalid,
    #[error("connection closed")]
    Closed,
    #[error("HA returned an error for request: {0}")]
    RequestFailed(Value),
}

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

enum ActorCommand {
    Call {
        payload: Value,
        respond_to: oneshot::Sender<Result<Value, Error>>,
    },
}

/// A live connection to Home Assistant's WebSocket API.
#[derive(Clone)]
pub struct Client {
    cmd_tx: mpsc::Sender<ActorCommand>,
    events_tx: broadcast::Sender<EntityState>,
}

/// Converts a `wss://`/`ws://`-agnostic HA base URL (e.g.
/// `http://homeassistant.local:8123`) into the websocket endpoint.
fn websocket_url(base_url: &str) -> String {
    let ws_base = base_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{}/api/websocket", ws_base.trim_end_matches('/'))
}

impl Client {
    /// Connects and authenticates once. Does not retry — see
    /// [`connect_with_backoff`] for that.
    pub async fn connect(base_url: &str, token: &str) -> Result<Self, Error> {
        let (mut ws, _) = tokio_tungstenite::connect_async(websocket_url(base_url)).await?;

        // auth_required -> auth -> auth_ok | auth_invalid
        expect_message_type(&mut ws, "auth_required").await?;
        ws.send(Message::Text(
            json!({ "type": "auth", "access_token": token }).to_string(),
        ))
        .await?;
        let auth_response = next_json(&mut ws).await?;
        match auth_response.get("type").and_then(Value::as_str) {
            Some("auth_ok") => {}
            Some("auth_invalid") => return Err(Error::AuthInvalid),
            _ => return Err(Error::Closed),
        }

        let (events_tx, _) = broadcast::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        tokio::spawn(run_actor(ws, cmd_rx, events_tx.clone()));

        let client = Self { cmd_tx, events_tx };
        client.call("subscribe_events", json!({ "event_type": "state_changed" })).await?;
        Ok(client)
    }

    /// Sends a command by `type` and returns HA's `result` payload.
    pub async fn call(&self, msg_type: &str, mut extra: Value) -> Result<Value, Error> {
        if let Value::Object(ref mut map) = extra {
            map.insert("type".into(), json!(msg_type));
        }
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::Call {
                payload: extra,
                respond_to: tx,
            })
            .await
            .map_err(|_| Error::Closed)?;
        rx.await.map_err(|_| Error::Closed)?
    }

    pub async fn get_states(&self) -> Result<Vec<EntityState>, Error> {
        let result = self.call("get_states", json!({})).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Live feed of `state_changed` events (the new state only). Lags drop
    /// old events rather than blocking the WS reader; the UI layer always
    /// wants the latest state, not a perfect history.
    pub fn subscribe_state_changed(&self) -> broadcast::Receiver<EntityState> {
        self.events_tx.subscribe()
    }

    pub async fn todo_items(&self, entity_id: &str) -> Result<Vec<crate::entities::TodoItem>, Error> {
        let result = self
            .call("todo/item/list", json!({ "entity_id": entity_id }))
            .await?;
        let items = result
            .get("items")
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value(items)?)
    }

    pub async fn todo_update_item(
        &self,
        entity_id: &str,
        uid: &str,
        status: crate::entities::TodoStatus,
    ) -> Result<(), Error> {
        self.call(
            "todo/item/update",
            json!({ "entity_id": entity_id, "item": uid, "status": status }),
        )
        .await?;
        Ok(())
    }
}

/// Repeatedly attempts [`Client::connect`] with exponential backoff (capped
/// at `max_delay`) until it succeeds. Intended for startup and for the app
/// layer to call again after a live `Client` reports `Error::Closed`.
pub async fn connect_with_backoff(base_url: &str, token: &str, max_delay: Duration) -> Client {
    let mut delay = Duration::from_secs(1);
    loop {
        match Client::connect(base_url, token).await {
            Ok(client) => return client,
            Err(err) => {
                tracing::warn!(error = %err, delay_secs = delay.as_secs(), "HA connection failed, retrying");
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, max_delay);
            }
        }
    }
}

async fn run_actor(
    ws: WsStream,
    mut cmd_rx: mpsc::Receiver<ActorCommand>,
    events_tx: broadcast::Sender<EntityState>,
) {
    let next_id = AtomicU64::new(1);
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, Error>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (mut write, mut read) = ws.split();

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                let Some(ActorCommand::Call { mut payload, respond_to }) = cmd else {
                    break; // all Client handles dropped
                };
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                if let Value::Object(ref mut map) = payload {
                    map.insert("id".into(), json!(id));
                }
                pending.lock().await.insert(id, respond_to);
                if let Err(err) = write.send(Message::Text(payload.to_string())).await {
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let _ = tx.send(Err(Error::WebSocket(err)));
                    }
                    break;
                }
            }
            msg = read.next() => {
                let Some(Ok(Message::Text(text))) = msg else {
                    break; // connection closed or errored
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                match value.get("type").and_then(Value::as_str) {
                    Some("result") => {
                        if let Some(id) = value.get("id").and_then(Value::as_u64) {
                            if let Some(tx) = pending.lock().await.remove(&id) {
                                let success = value.get("success").and_then(Value::as_bool).unwrap_or(false);
                                let outcome = if success {
                                    Ok(value.get("result").cloned().unwrap_or(Value::Null))
                                } else {
                                    Err(Error::RequestFailed(value.get("error").cloned().unwrap_or(Value::Null)))
                                };
                                let _ = tx.send(outcome);
                            }
                        }
                    }
                    Some("event") => {
                        if let Some(new_state) = value.pointer("/event/data/new_state") {
                            if let Ok(state) = serde_json::from_value::<EntityState>(new_state.clone()) {
                                let _ = events_tx.send(state);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Wake up anyone still waiting with a definitive error.
    for (_, tx) in pending.lock().await.drain() {
        let _ = tx.send(Err(Error::Closed));
    }
}

async fn next_json(ws: &mut WsStream) -> Result<Value, Error> {
    match ws.next().await {
        Some(Ok(Message::Text(text))) => Ok(serde_json::from_str(&text)?),
        Some(Ok(_)) => Err(Error::Closed),
        Some(Err(err)) => Err(Error::WebSocket(err)),
        None => Err(Error::Closed),
    }
}

async fn expect_message_type(ws: &mut WsStream, expected: &str) -> Result<Value, Error> {
    let value = next_json(ws).await?;
    if value.get("type").and_then(Value::as_str) == Some(expected) {
        Ok(value)
    } else {
        Err(Error::Closed)
    }
}
