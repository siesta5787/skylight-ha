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
/// `GET /api/calendar/{entity_id}?start=...&end=...`.
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
    #[serde(default, with = "time::serde::iso8601::option")]
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
