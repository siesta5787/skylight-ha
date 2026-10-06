//! The Rewards tab's data, read off the Skylight Family integration.
//!
//! The integration (`siesta5787/skylight-family`) gives every member with
//! reward tracking turned on a `sensor.skylight_family_<name>_stars` entity:
//! its state is how many stars they have collected this week, and its
//! attributes carry the whole week plus today's chore progress. Its own
//! docstring calls this "what the wall tablet reads to draw a member's row of
//! stars", which is exactly what this module parses it into.
//!
//! Deliberately read from the entity rather than the integration's
//! `skylight_family/rewards` websocket command, which its own HA panel uses:
//! the entity is already in `get_states` and already arrives on the
//! `state_changed` subscription this app has open, so a kid ticking off a
//! chore updates the wall display without polling anything.

use ha_client::entities::EntityState;

/// One tracked member's week.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub stars: i32,
    pub goal: i32,
    /// Monday first, one entry per day of the week.
    pub days: Vec<Day>,
    pub chores_done: i32,
    pub chores_total: i32,
    pub prize_earned: bool,
    /// Earned by yesterday's star, which is the whole point of the daily one.
    pub tablet_time: bool,
    pub stars_needed: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayState {
    Earned,
    /// The day happened and no star came of it.
    Missed,
    /// Hasn't happened yet -- drawn as neither earned nor lost.
    Upcoming,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Day {
    /// "MON".."SUN".
    pub label: String,
    pub state: DayState,
    pub today: bool,
    /// Set by hand rather than decided by the chore list.
    pub manual: bool,
}

/// Monday first, matching the integration's own week.
const WEEKDAYS: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

/// Every tracked member in `states`, in a stable order.
pub fn from_states(states: &[EntityState]) -> Vec<Member> {
    let mut members: Vec<(String, Member)> = states
        .iter()
        .filter_map(|state| {
            let slug = stars_sensor_slug(&state.entity_id)?;
            Some((slug.to_string(), member_from(state)?))
        })
        .collect();
    // Sorted by slug rather than display name so the columns don't reorder
    // themselves when a nickname changes.
    members.sort_by(|a, b| a.0.cmp(&b.0));
    members.into_iter().map(|(_, member)| member).collect()
}

/// The member slug behind a star sensor's entity id, if that's what it is.
pub fn stars_sensor_slug(entity_id: &str) -> Option<&str> {
    entity_id.strip_prefix("sensor.skylight_family_")?.strip_suffix("_stars")
}

fn member_from(state: &EntityState) -> Option<Member> {
    let attributes = &state.attributes;
    // Absent for an unavailable sensor, which reports no attributes at all.
    // A member with no week to show is left out entirely rather than drawn as
    // an empty column.
    let days = attributes.get("days")?.as_object()?;
    let today = attributes.get("today").and_then(|v| v.as_str()).unwrap_or("");

    // Date-keyed, and ISO dates sort the same as calendar order -- so position
    // in this sorted list is the day of the week, the integration's weeks
    // always running Monday to Sunday.
    let mut dates: Vec<&String> = days.keys().collect();
    dates.sort();

    let days = dates
        .into_iter()
        .enumerate()
        .map(|(index, date)| {
            let record = days.get(date).filter(|value| !value.is_null());
            let star = record.and_then(|r| r.get("star")).and_then(|v| v.as_bool()).unwrap_or(false);
            let manual =
                record.and_then(|r| r.get("source")).and_then(|v| v.as_str()) == Some("manual");
            Day {
                label: WEEKDAYS.get(index).copied().unwrap_or("").to_string(),
                // String comparison, which for ISO dates is date comparison.
                state: if date.as_str() > today {
                    DayState::Upcoming
                } else if star {
                    DayState::Earned
                } else {
                    DayState::Missed
                },
                today: date.as_str() == today,
                manual,
            }
        })
        .collect();

    let int = |key: &str| attributes.get(key).and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let flag = |key: &str| attributes.get(key).and_then(|v| v.as_bool()).unwrap_or(false);

    Some(Member {
        name: display_name(state),
        // The state carries the count; the attributes don't repeat it.
        stars: state.state.parse().unwrap_or(0),
        goal: int("goal"),
        days,
        chores_done: int("chores_done"),
        chores_total: int("chores_total"),
        prize_earned: flag("prize_earned"),
        tablet_time: flag("tablet_time"),
        stars_needed: int("stars_needed"),
    })
}

/// The member's own name, not the sensor's.
///
/// The integration names these entities "<Name> stars"; the column is headed
/// by the person, so drop the suffix. Falls back to the slug if some future
/// version names them differently.
fn display_name(state: &EntityState) -> String {
    let friendly = state.attributes.get("friendly_name").and_then(|v| v.as_str());
    match friendly {
        Some(name) => name.strip_suffix(" stars").unwrap_or(name).to_string(),
        None => stars_sensor_slug(&state.entity_id).unwrap_or_default().replace('_', " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stars_sensor(entity_id: &str, state: &str, attributes: serde_json::Value) -> EntityState {
        EntityState {
            entity_id: entity_id.into(),
            state: state.into(),
            attributes,
            last_updated: None,
        }
    }

    /// Shaped exactly like the real attributes: a date-keyed week where a
    /// future day is `null`, not a record saying "no star".
    fn ava() -> EntityState {
        stars_sensor(
            "sensor.skylight_family_ava_stars",
            "1",
            serde_json::json!({
                "friendly_name": "Ava stars",
                "week_start": "2026-10-05",
                "today": "2026-10-06",
                "days": {
                    "2026-10-05": { "star": true, "source": "auto" },
                    "2026-10-06": { "star": false, "source": "auto" },
                    "2026-10-07": null,
                    "2026-10-08": null,
                    "2026-10-09": null,
                    "2026-10-10": null,
                    "2026-10-11": null,
                },
                "goal": 6,
                "stars_needed": 5,
                "days_remaining": 6,
                "prize_earned": false,
                "star_today": false,
                "star_today_source": "auto",
                "chores_done": 1,
                "chores_total": 2,
                "tablet_time": true,
            }),
        )
    }

    #[test]
    fn a_week_reads_monday_first_with_today_marked() {
        let members = from_states(&[ava()]);
        assert_eq!(members.len(), 1);
        let ava = &members[0];
        assert_eq!(ava.name, "Ava", "the column is headed by the person, not the sensor");
        assert_eq!(ava.stars, 1);
        assert_eq!(ava.goal, 6);
        assert_eq!(ava.stars_needed, 5);
        assert!(ava.tablet_time);
        assert_eq!((ava.chores_done, ava.chores_total), (1, 2));

        let labels: Vec<&str> = ava.days.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels, ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"]);
        assert_eq!(ava.days[0].state, DayState::Earned);
        assert_eq!(ava.days[1].state, DayState::Missed, "today, with chores still to do");
        assert_eq!(ava.days[2].state, DayState::Upcoming, "not a missed day -- it hasn't happened");
        assert!(ava.days[1].today);
        assert_eq!(ava.days.iter().filter(|d| d.today).count(), 1);
    }

    #[test]
    fn a_hand_set_day_says_so() {
        let mut state = ava();
        state.attributes["days"]["2026-10-05"] =
            serde_json::json!({ "star": true, "source": "manual" });
        let ava = &from_states(&[state])[0];
        assert!(ava.days[0].manual);
        assert!(!ava.days[1].manual);
    }

    /// Anything else in the entity list, including the member mapping sensors
    /// these sit beside, must not be mistaken for a tracked member.
    #[test]
    fn only_star_sensors_count() {
        let states = vec![
            ava(),
            stars_sensor(
                "sensor.skylight_family_ava",
                "ok",
                serde_json::json!({ "friendly_name": "Ava", "color": [255, 0, 128] }),
            ),
            stars_sensor("sensor.living_room_stars", "3", serde_json::json!({})),
        ];
        let members = from_states(&states);
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "Ava");
    }

    /// An unavailable sensor reports no attributes at all. Better no column
    /// than a column of zeroes implying a week with nothing earned.
    #[test]
    fn an_unavailable_member_is_left_out() {
        let states = vec![stars_sensor(
            "sensor.skylight_family_evelyn_stars",
            "unavailable",
            serde_json::json!({}),
        )];
        assert!(from_states(&states).is_empty());
    }

    #[test]
    fn members_come_back_in_a_stable_order() {
        let mut evelyn = ava();
        evelyn.entity_id = "sensor.skylight_family_evelyn_stars".into();
        evelyn.attributes["friendly_name"] = serde_json::json!("Evelyn stars");
        let mut brielle = ava();
        brielle.entity_id = "sensor.skylight_family_brielle_stars".into();
        brielle.attributes["friendly_name"] = serde_json::json!("Brielle stars");

        let names: Vec<String> =
            from_states(&[evelyn, ava(), brielle]).into_iter().map(|m| m.name).collect();
        assert_eq!(names, ["Ava", "Brielle", "Evelyn"]);
    }
}
