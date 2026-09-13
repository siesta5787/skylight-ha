//! Typed views over the subset of Home Assistant's data model this
//! dashboard cares about: generic entity state, calendar events, and todo
//! items. HA's own API is far broader than this; we only model what's
//! needed for the calendar+tasks dashboard.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A generic HA entity state, as returned by `get_states` and carried on
/// `state_changed` events.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EntityState {
    pub entity_id: String,
    pub state: String,
    #[serde(default)]
    pub attributes: serde_json::Value,
}

/// One event from a calendar entity's event range, as returned by
/// `GET /api/calendars/{entity_id}?start=...&end=...`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CalendarEvent {
    pub summary: String,
    pub start: CalendarDateTime,
    pub end: CalendarDateTime,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
}

/// HA represents calendar event times as either a date (all-day) or a
/// datetime, distinguished by which JSON key is present.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CalendarDateTime {
    // HA's JSON key is camelCase ("dateTime") even though everything else in
    // this API is snake_case -- without the rename, serde silently leaves
    // this `None` (it has `#[serde(default)]`) instead of erroring, so every
    // timed event looked like a malformed/all-day event with neither field
    // set and got silently dropped rather than failing loudly.
    #[serde(default, rename = "dateTime", with = "time::serde::iso8601::option")]
    pub date_time: Option<OffsetDateTime>,
    #[serde(default)]
    pub date: Option<String>,
}

/// One item from a `todo.*` list, as returned by the `todo/item/list` WS
/// command.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TodoItem {
    pub uid: String,
    pub summary: String,
    pub status: TodoStatus,
    #[serde(default)]
    pub due: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    NeedsAction,
    Completed,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression test for a real bug: HA's calendar API returns "dateTime"
    // (camelCase) even though the rest of its API is snake_case. Without
    // `#[serde(rename = "dateTime")]`, `date_time` silently stayed `None`
    // (it has `#[serde(default)]`) instead of erroring, so every timed
    // event looked malformed and got dropped before ever reaching the UI.
    #[test]
    fn parses_real_ha_timed_event_shape() {
        let json = r#"{
            "start": { "dateTime": "2026-09-12T21:00:00-04:00" },
            "end": { "dateTime": "2026-09-12T22:00:00-04:00" },
            "summary": "Test",
            "description": "",
            "location": null
        }"#;
        let event: CalendarEvent = serde_json::from_str(json).unwrap();
        assert!(event.start.date_time.is_some(), "date_time should have parsed, not fallen back to None");
        assert!(event.start.date.is_none());
    }

    #[test]
    fn parses_all_day_event_shape() {
        let json = r#"{
            "start": { "date": "2026-09-20" },
            "end": { "date": "2026-09-21" },
            "summary": "Camping Trip"
        }"#;
        let event: CalendarEvent = serde_json::from_str(json).unwrap();
        assert!(event.start.date_time.is_none());
        assert_eq!(event.start.date.as_deref(), Some("2026-09-20"));
    }
}
