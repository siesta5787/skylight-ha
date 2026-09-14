//! HA REST calls. Used for the Calendar API — event ranges aren't available
//! over the WebSocket API, so this needs to be polled (on view load/
//! navigation and periodically) rather than pushed — and for fetching a
//! single entity's state (e.g. weather), which is simpler as one REST call
//! than round-tripping the WS `get_states` command for every poll.

use time::OffsetDateTime;

use crate::entities::{CalendarEvent, EntityState};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Clone)]
pub struct RestClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl RestClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
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
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .query(&[
                ("start", start.format(&time::format_description::well_known::Rfc3339).unwrap()),
                ("end", end.format(&time::format_description::well_known::Rfc3339).unwrap()),
            ])
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
