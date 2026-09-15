//! Config schema for the dashboard: HA connection info, the family member
//! roster, weather entities, and the Dashboard page's sections.

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

/// One card on the Dashboard page. Order in `Config.dashboard` is render
/// order, so e.g. a `Climate` card can sit between two `ToggleGroup`s.
/// Usually built at connect time from (in priority order, see
/// `apps/skylight-ha`'s `run_ha_sync`): `dashboard` below if non-empty (a
/// manual override, same pattern as `family`/`weather_entity`), else the
/// `siesta5787/skylight-family` HA integration's `sensor.skylight_
/// dashboard_*` entities if any exist (not yet implemented on that
/// integration's side -- `discover_dashboard_sections` in main.rs returns
/// `None` until it is).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DashboardSection {
    /// A titled group of on/off entities with a header switch that toggles
    /// all of them at once -- e.g. "Lights" or "Fans". Brightness/speed
    /// control is out of scope; this is plain on/off, matching what the
    /// reference Lovelace dashboard this was modeled on actually shows.
    ToggleGroup {
        title: String,
        entities: Vec<String>,
    },
    /// One `climate.*` entity: mode buttons (only for whichever of off/
    /// fan_only/cool/heat the entity's own `hvac_modes` supports) plus a
    /// +/- temperature stepper. No `title` -- the card header is the
    /// entity's own `friendly_name`.
    Climate {
        entity: String,
    },
    /// A titled group of read-only sensor rows (e.g. a device's
    /// temperature/battery/signal-strength sensors, each its own HA
    /// entity but shown together under one device name).
    SensorGroup {
        title: String,
        entities: Vec<String>,
    },
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
    /// A second `weather.*` entity to pull humidity/air-pressure from when
    /// `weather_entity` doesn't expose them itself (some integrations only
    /// report condition/temperature/wind). Optional: if unset, the
    /// dashboard auto-discovers any other weather.* entity that does have
    /// those attributes (see discover_weather_backfill_entity).
    #[serde(default)]
    pub weather_backfill_entity: Option<String>,
    #[serde(default)]
    pub dashboard: Vec<DashboardSection>,
    /// Where the (hashed) parental PIN lock lives, if one's been set up --
    /// unlike `ha.token_path`, this file is written by the app itself
    /// (Settings page), not user-supplied, so it's optional with a default
    /// rather than required. Relative to the process's current working
    /// directory, same caveat as `ha.token_path` -- keep it absolute in
    /// the real device's `config.toml` for the same reason.
    #[serde(default = "default_pin_hash_path")]
    pub pin_hash_path: String,
}

fn default_pin_hash_path() -> String {
    "pin.secret".to_string()
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
        "##;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(config.family.len(), 1);
        assert!(config.member("alice").is_some());
        assert!(config.dashboard.is_empty());
    }

    #[test]
    fn parses_dashboard_sections() {
        let toml_src = r##"
            [ha]
            base_url = "http://homeassistant.local:8123"
            token_path = "/etc/skylight-ha/token"

            [[dashboard]]
            type = "toggle_group"
            title = "Lights"
            entities = ["light.family_room", "light.kitchen"]

            [[dashboard]]
            type = "climate"
            entity = "climate.upstairs"

            [[dashboard]]
            type = "sensor_group"
            title = "Garage Freezer"
            entities = ["sensor.garage_freezer_temperature"]
        "##;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(config.dashboard.len(), 3);
        assert!(matches!(config.dashboard[0], DashboardSection::ToggleGroup { .. }));
        assert!(matches!(config.dashboard[1], DashboardSection::Climate { .. }));
        assert!(matches!(config.dashboard[2], DashboardSection::SensorGroup { .. }));
    }
}
