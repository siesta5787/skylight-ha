//! HA REST calls. Only used for the Calendar API — event ranges aren't
//! available over the WebSocket API, so this needs to be polled (on view
//! load/navigation and periodically) rather than pushed.

use time::OffsetDateTime;

use crate::entities::CalendarEvent;

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
        let url = format!(
            "{}/api/calendar/{entity_id}",
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
}
