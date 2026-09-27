//! HA REST calls. Used for the Calendar API — event ranges aren't available
//! over the WebSocket API, so this needs to be polled (on view load/
//! navigation and periodically) rather than pushed — and for fetching a
//! single entity's state (e.g. weather), which is simpler as one REST call
//! than round-tripping the WS `get_states` command for every poll.

use std::time::Duration;

use time::OffsetDateTime;

use crate::entities::{CalendarEvent, EntityState};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("failed to format a timestamp for the HA calendar API: {0}")]
    TimestampFormat(#[from] time::error::Format),
}

/// Total per-request budget (connect + TLS + request + response body).
///
/// `reqwest::Client::new()` sets no timeout whatsoever, so every REST call
/// here -- the calendar fetch on every nav tap and every periodic refresh,
/// and the weather entity poll -- could hang for as long as the OS kept the
/// socket open, which on a silently-dropped WiFi link is indefinitely. The
/// websocket path already had `CONNECT_TIMEOUT` (see `connection.rs`); this
/// is the equivalent it never got.
///
/// Sized the same way as `connection.rs`'s `CALL_TIMEOUT`: generous, because
/// this HA instance is genuinely slow sometimes, and a spurious timeout
/// shows up as missing calendar events rather than as an error anyone sees.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);

/// Just the TCP+TLS handshake. Much tighter than `REQUEST_TIMEOUT` because a
/// connect that hasn't completed in 15s is a network problem, not a slow HA
/// -- and matches `connection.rs`'s `CONNECT_TIMEOUT` exactly so both
/// transports fail over on the same timescale.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct RestClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl RestClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        // `ClientBuilder::build()` only fails on a bad TLS backend setup or
        // an unusable resolver -- nothing that depends on runtime input here
        // -- and there is no sensible way to carry on without an HTTP client
        // anyway, so this is one of the few genuinely unreachable
        // `expect()`s. Falling back to `Client::new()` would silently
        // reinstate the no-timeout behaviour this exists to fix.
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("failed to build the HA REST client");
        Self {
            http,
            base_url: base_url.into(),
            token: token.into(),
        }
    }

    /// Fetches events for `entity_id` (e.g. `calendar.alice`) in `[start, end)`.
    pub async fn calendar_events(
        &self,
        entity_id: &str,
        start: OffsetDateTime,
        end: OffsetDateTime,
    ) -> Result<Vec<CalendarEvent>, Error> {
        // Plural "calendars" -- confirmed against a real HA instance; the
        // singular "/api/calendar/{id}" (what this used to say) 404s.
        let url = format!(
            "{}/api/calendars/{entity_id}",
            self.base_url.trim_end_matches('/')
        );
        // `?` rather than `.unwrap()`: formatting an `OffsetDateTime` as
        // RFC3339 can't realistically fail (it would need a year outside
        // ±9999), but this function already returns `Result` and there's no
        // reason for an unreachable branch to be a panic on a wall-mounted
        // device rather than a logged error.
        let rfc3339 = &time::format_description::well_known::Rfc3339;
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .query(&[("start", start.format(rfc3339)?), ("end", end.format(rfc3339)?)])
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }

    /// Fetches one entity's current state (e.g. `weather.home`).
    pub async fn entity_state(&self, entity_id: &str) -> Result<EntityState, Error> {
        let url = format!("{}/api/states/{entity_id}", self.base_url.trim_end_matches('/'));
        let response = self.http.get(url).bearer_auth(&self.token).send().await?.error_for_status()?;
        Ok(response.json().await?)
    }
}
