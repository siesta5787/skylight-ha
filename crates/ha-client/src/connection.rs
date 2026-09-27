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
    #[error("timed out connecting to HA")]
    ConnectTimeout,
    #[error("HA did not answer the request within {}s", CALL_TIMEOUT.as_secs())]
    CallTimeout,
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

/// `tokio_tungstenite::connect_async` (TCP connect + TLS + WS upgrade, all
/// in one future) has no built-in timeout -- if any of those steps stalls
/// (a proxy that accepts the TCP connection but never completes the WS
/// handshake, for instance) this would otherwise hang forever with no
/// error, no retry, and no log line. Wrapping the whole `connect` body
/// gives `connect_with_backoff` something to actually retry on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Upper bound on how long [`Client::call`] will wait for HA to answer.
///
/// This exists because `rx.await` had no timeout at all: if the actor task
/// stopped making progress (a silently-dropped TCP connection where reads
/// never return -- see `PING_INTERVAL` below), every caller parked forever
/// with no error and no log line, and the UI simply stopped updating.
///
/// Deliberately generous rather than snappy. `get_states` on this project's
/// real HA instance has been measured at ~460ms, 17.5s (538 entities), and
/// once >30s, so anything in the "feels responsive" range would fire
/// spuriously on a loaded instance and turn a slow refresh into a
/// reconnect loop. 45s is comfortably past the worst *observed* figure while
/// still being far short of "forever".
const CALL_TIMEOUT: Duration = Duration::from_secs(45);

/// How often the actor sends HA's `ping` command, and how long it then waits
/// for the matching `pong` before declaring the connection dead.
///
/// `tungstenite` does not enable TCP keepalive, and nothing in this crate
/// previously imposed a read deadline -- so on a silently-dropped link (WiFi
/// gone, socket still "open" at the OS level, no FIN/RST ever delivered)
/// `read.next()` blocks indefinitely: writes still appear to succeed because
/// they just fill the kernel's send buffer, reads never return, and the app
/// sits on stale data forever without ever entering the reconnect path.
/// An application-level round trip is the only thing that actually detects
/// this.
///
/// Both values are 30s, so a dead link is noticed within 30-60s. The
/// timeout is not tighter for the same reason `CALL_TIMEOUT` isn't: HA's own
/// event loop can be busy for many seconds on this instance, and a
/// late pong is not the same thing as a dead socket.
const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_TIMEOUT: Duration = Duration::from_secs(30);

impl Client {
    /// Connects and authenticates once. Does not retry — see
    /// [`connect_with_backoff`] for that.
    pub async fn connect(base_url: &str, token: &str) -> Result<Self, Error> {
        match tokio::time::timeout(CONNECT_TIMEOUT, Self::connect_inner(base_url, token)).await {
            Ok(result) => result,
            Err(_) => Err(Error::ConnectTimeout),
        }
    }

    async fn connect_inner(base_url: &str, token: &str) -> Result<Self, Error> {
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

        // 64 was too small for this instance: `state_changed` arrives for
        // *every* entity (538 of them here), so a burst -- an HA restart, a
        // scene/automation touching many entities at once -- overran the
        // buffer routinely, and every overrun costs the consumer in
        // `run_ha_sync` a `Lagged` error which it (correctly, but
        // expensively) treats as "refresh everything". 512 is still tiny in
        // absolute terms (`EntityState` is a few hundred bytes) and makes
        // that essentially stop happening outside of genuinely pathological
        // load.
        let (events_tx, _) = broadcast::channel(512);
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
        // See `CALL_TIMEOUT`: without this, a wedged actor task parked every
        // caller indefinitely. `Err(Elapsed)` here is not the same as
        // `Error::Closed` -- the actor may still be alive and merely slow --
        // so it gets its own variant, and it's logged, because "waited 45s
        // and gave up" was previously indistinguishable from "still waiting".
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::Closed), // actor dropped the responder
            Err(_) => {
                tracing::warn!(
                    msg_type,
                    timeout_secs = CALL_TIMEOUT.as_secs(),
                    "HA did not answer a websocket command in time"
                );
                Err(Error::CallTimeout)
            }
        }
    }

    /// Resolves as soon as the actor task driving this connection has exited
    /// -- i.e. the connection is definitively dead and every subsequent
    /// `call` will fail.
    ///
    /// This is the liveness signal the app layer should select on. The
    /// obvious-looking alternative, waiting for
    /// `subscribe_state_changed()`'s receiver to report `RecvError::Closed`,
    /// cannot work: that only fires once *all* senders drop, and every live
    /// `Client` clone owns one -- so any code holding a `Client` for the
    /// session (which is exactly the code that wants to know) structurally
    /// prevents its own notification. `mpsc::Sender::closed()` has no such
    /// problem: the matching receiver lives in `run_actor` and nowhere else,
    /// so it drops precisely when the actor returns.
    pub async fn wait_closed(&self) {
        self.cmd_tx.closed().await
    }

    pub async fn get_states(&self) -> Result<Vec<EntityState>, Error> {
        let result = self.call("get_states", json!({})).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Calls any HA service against one or more entities in one request
    /// (`target.entity_id` accepts an array) -- covers `light.turn_on/off`,
    /// `fan.turn_on/off`, `climate.set_hvac_mode`, `climate.
    /// set_temperature`, and the dashboard's group-toggle case (every
    /// entity in a section at once), without a dedicated method per
    /// domain. `todo_update_item`/`create_calendar_event` (apps/
    /// skylight-ha) built this same `call_service` shape inline before
    /// this existed; this is the generalized version.
    pub async fn call_service(
        &self,
        domain: &str,
        service: &str,
        entity_ids: &[String],
        service_data: Value,
    ) -> Result<(), Error> {
        self.call(
            "call_service",
            json!({
                "domain": domain,
                "service": service,
                "target": { "entity_id": entity_ids },
                "service_data": service_data,
            }),
        )
        .await?;
        Ok(())
    }

    /// Today's (and the next several days') forecast via the `weather.
    /// get_forecasts` service. Needs `return_response: true` -- this is a
    /// service call, not a plain query, so the result only carries HA's
    /// generic `call_service` ack unless asked to also hand back the
    /// service's own response payload, which is where the forecast lives.
    pub async fn weather_daily_forecast(
        &self,
        entity_id: &str,
    ) -> Result<Vec<crate::entities::DailyForecast>, Error> {
        let result = self
            .call(
                "call_service",
                json!({
                    "domain": "weather",
                    "service": "get_forecasts",
                    "service_data": { "type": "daily" },
                    "target": { "entity_id": entity_id },
                    "return_response": true,
                }),
            )
            .await?;
        let forecast = result
            .get("response")
            .and_then(|r| r.get(entity_id))
            .and_then(|e| e.get("forecast"))
            .cloned()
            .unwrap_or(Value::Array(vec![]));
        Ok(serde_json::from_value(forecast)?)
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
        // There's no dedicated `todo/item/update` WS command -- that's what
        // this used to call, and HA rejected it every single time with
        // "unknown_command" (confirmed against a real instance). Updating
        // an item is the `todo.update_item` *service*, called the same way
        // as any other (see `Client::call`'s `call_service` usage in
        // apps/skylight-ha's create_calendar_event). Confirmed empirically
        // that `item` accepts the item's uid directly, not just its summary
        // text (the service's own field docs only show a summary example).
        self.call(
            "call_service",
            json!({
                "domain": "todo",
                "service": "update_item",
                "target": { "entity_id": entity_id },
                "service_data": { "item": uid, "status": status },
            }),
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

    // Application-level keepalive state. `awaiting_pong` holds the id of the
    // outstanding `ping` command, if any; `pong_deadline` is when that ping
    // gives up. The deadline is only *armed* (via the `if` guard on its
    // select branch) while a ping is outstanding, so an idle connection with
    // no ping in flight never trips it.
    let mut ping_ticker = tokio::time::interval(PING_INTERVAL);
    ping_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut awaiting_pong: Option<u64> = None;
    let mut pong_deadline = tokio::time::Instant::now();

    loop {
        tokio::select! {
            _ = ping_ticker.tick() => {
                // One ping outstanding at a time. If the previous one is
                // still unanswered when the next tick comes round, the
                // deadline branch below has already handled (or is about to
                // handle) it -- don't stack a second id on top.
                if awaiting_pong.is_some() {
                    continue;
                }
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                // HA's websocket API answers `{"type": "ping"}` with
                // `{"type": "pong"}` carrying the same id. Note this is
                // *not* a `result` response, so it deliberately does not go
                // through the `pending` map / `Client::call` -- it's handled
                // entirely inside this actor, which also means a keepalive
                // can never be starved by or interleave badly with a real
                // caller's request.
                if let Err(err) = write.send(Message::Text(json!({ "id": id, "type": "ping" }).to_string())).await {
                    tracing::warn!(error = %err, "failed to send HA keepalive ping");
                    break;
                }
                awaiting_pong = Some(id);
                pong_deadline = tokio::time::Instant::now() + PONG_TIMEOUT;
            }
            _ = tokio::time::sleep_until(pong_deadline), if awaiting_pong.is_some() => {
                tracing::warn!(
                    timeout_secs = PONG_TIMEOUT.as_secs(),
                    "no pong from HA, treating the connection as dead"
                );
                // Falling out of this loop drops `cmd_rx`, which is what
                // makes `Client::call` fail with `Error::Closed` and
                // `Client::wait_closed()` resolve -- that's the path
                // `run_ha_sync` reconnects on.
                break;
            }
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
                // This used to be `let Some(Ok(Message::Text(text))) = msg
                // else { break }`, which killed the actor -- and so forced a
                // full reconnect -- on *any* non-Text frame. A server-sent
                // Ping (HA behind a proxy that keepalives), the Pong for one,
                // or a Binary frame are all perfectly normal and mean
                // nothing is wrong. Only a Close frame, a stream error, or
                // end-of-stream actually end the connection.
                let text = match msg {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_))) => {
                        // Intentionally *not* replying here: tungstenite
                        // already queued the Pong itself
                        // (`protocol/mod.rs`'s `OpCtl::Ping` arm calls
                        // `set_additional(Frame::pong(..))`, flushed on the
                        // next read/write) and still surfaces the Ping to us
                        // for information. Sending our own would put a
                        // duplicate, unsolicited Pong on the wire.
                        continue;
                    }
                    Some(Ok(Message::Close(frame))) => {
                        tracing::info!(?frame, "HA closed the websocket");
                        break;
                    }
                    // Pong / Binary / raw Frame: nothing this client needs.
                    Some(Ok(_)) => continue,
                    Some(Err(err)) => {
                        tracing::warn!(error = %err, "HA websocket read error");
                        break;
                    }
                    None => break, // stream ended
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                match value.get("type").and_then(Value::as_str) {
                    // Answer to our keepalive (see `PING_INTERVAL`). Matched
                    // on id so a stale pong from a previous ping can't clear
                    // the deadline for the current one.
                    Some("pong") => {
                        if value.get("id").and_then(Value::as_u64) == awaiting_pong {
                            awaiting_pong = None;
                        }
                    }
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
