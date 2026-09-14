//! Config schema for the dashboard: HA connection info, the family member
//! roster, and the views/cards that make up the UI. Deliberately a small,
//! typed echo of Lovelace's dashboard/view/card model rather than a general
//! YAML-card engine.

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to read HA token file {path}: {source}")]
    ReadToken {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Home Assistant connection details. The access token is deliberately kept
/// out of this struct's `Deserialize` derive — it's loaded separately from
/// `token_path` at runtime so it never ends up in the config file itself.
#[derive(Debug, Clone, Deserialize)]
pub struct HaConnection {
    /// e.g. "http://homeassistant.local:8123"
    pub base_url: String,
    /// Path to a file (restricted permissions) containing a single HA
    /// long-lived access token.
    pub token_path: String,
}

impl HaConnection {
    pub fn load_token(&self) -> Result<String, ConfigError> {
        std::fs::read_to_string(&self.token_path)
            .map(|s| s.trim().to_string())
            .map_err(|source| ConfigError::ReadToken {
                path: self.token_path.clone(),
                source,
            })
    }
}

/// A family member the dashboard tracks. Todos are per-person: each member
/// has their own HA `todo.*` entity rather than everyone sharing one list.
/// Usually built at connect time from (in priority order, see
/// `apps/skylight-ha`'s `run_ha_sync`): `[[family]]` below if non-empty (a
/// manual override for custom colors/order/pairing), else the
/// `siesta5787/skylight-family` HA integration's `sensor.skylight_family_*`
/// entities if any exist (the controlled, purpose-built source), else
/// heuristically auto-discovered from whatever `todo.*`/`calendar.*`
/// entities exist in HA. None of these guarantee both fields present -- a
/// shared household calendar with no matching todo list becomes its own
/// entry with `todo_entity: None`.
#[derive(Debug, Clone, Deserialize)]
pub struct FamilyMember {
    pub id: String,
    pub name: String,
    /// Hex color, e.g. "#4f8ef7", used for their calendar events and todo tab.
    pub color: String,
    pub todo_entity: Option<String>,
    /// A member can have more than one calendar (the Skylight Family
    /// integration supports linking several) -- events from all of them are
    /// shown in this member's color, undifferentiated.
    #[serde(default)]
    pub calendar_entities: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Card {
    Calendar {
        /// Family member ids whose calendars should be shown; empty = all.
        #[serde(default)]
        members: Vec<String>,
    },
    TodoList {
        /// Family member ids to show as columns/tabs; empty = all.
        #[serde(default)]
        members: Vec<String>,
    },
    Weather {
        entity: String,
    },
    Clock,
    EntityTile {
        entity: String,
        label: Option<String>,
    },
    Media {
        entity: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct View {
    pub name: String,
    pub cards: Vec<Card>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub ha: HaConnection,
    #[serde(default)]
    pub family: Vec<FamilyMember>,
    /// Which `weather.*` entity feeds the top bar. Optional: if unset, the
    /// dashboard auto-discovers the first `weather.*` entity it finds in HA
    /// (see discover_weather_entity in apps/skylight-ha/src/main.rs), same
    /// override-else-auto-discover pattern as the family roster.
    #[serde(default)]
    pub weather_entity: Option<String>,
    pub views: Vec<View>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path_ref = path.as_ref();
        let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::Read {
            path: path_ref.display().to_string(),
            source,
        })?;
        let config: Config = toml::from_str(&raw)?;
        Ok(config)
    }

    /// The default/home view is always the first one defined in config.
    pub fn home_view(&self) -> Option<&View> {
        self.views.first()
    }

    pub fn member(&self, id: &str) -> Option<&FamilyMember> {
        self.family.iter().find(|m| m.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        // r##"..."## (not r#"..."#) because the TOML body below contains a
        // literal `"#` (the `color = "#4f8ef7"` hex value), which would
        // otherwise terminate a single-hash raw string early.
        let toml_src = r##"
            [ha]
            base_url = "http://homeassistant.local:8123"
            token_path = "/etc/skylight-ha/token"

            [[family]]
            id = "alice"
            name = "Alice"
            color = "#4f8ef7"
            todo_entity = "todo.chores_alice"

            [[views]]
            name = "Home"
            [[views.cards]]
            type = "calendar"
            [[views.cards]]
            type = "todo_list"
        "##;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(config.family.len(), 1);
        assert_eq!(config.home_view().unwrap().cards.len(), 2);
        assert!(config.member("alice").is_some());
    }
}
