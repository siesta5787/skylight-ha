//! The Rewards / Money tab's data, read off the Skylight Family integration.
//!
//! The integration (`siesta5787/skylight-family`) gives every member with
//! reward tracking turned on a `sensor.skylight_family_<name>_stars` entity:
//! its state is how many stars they have collected this week, and its
//! attributes carry the whole week plus today's chore progress. Its own
//! docstring calls this "what the wall tablet reads to draw a member's row of
//! stars", which is exactly what this module parses it into. Members with
//! pocket money turned on also get `_short_term` and `_long_term` balance
//! sensors, folded into the same column.
//!
//! Deliberately read from the entities rather than the integration's
//! `skylight_family/rewards` websocket command, which its own HA panel uses:
//! the entities are already in `get_states` and already arrive on the
//! `state_changed` subscription this app has open, so a kid ticking off a
//! chore updates the wall display without polling anything.

use std::collections::BTreeMap;

use ha_client::entities::EntityState;
use time::{Date, Month, Weekday};

const ENTITY_PREFIX: &str = "sensor.skylight_family_";

/// One tracked member: their star week, their money, or both. Either half is
/// optional because the integration turns reward tracking and pocket money on
/// separately, per member.
#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    pub name: String,
    pub has_stars: bool,
    pub stars: i32,
    pub goal: i32,
    /// In the integration's own order, so a household whose week starts on
    /// Sunday sees Sunday first. Empty without reward tracking.
    pub days: Vec<Day>,
    pub chores_done: i32,
    pub chores_total: i32,
    pub prize_earned: bool,
    /// Earned by yesterday's star, which is the whole point of the daily one.
    pub tablet_time: bool,
    pub stars_needed: i32,
    pub money: Option<Money>,
}

/// A member's two accounts. Amounts are whole cents, because that is what the
/// integration's ledger is in and it avoids a float ever being shown.
#[derive(Debug, Clone, PartialEq)]
pub struct Money {
    pub short_term_cents: i64,
    pub long_term_cents: i64,
    /// This part-week's interest, not credited yet.
    pub accruing_cents: i64,
    /// Annual percentage, e.g. 3.0 for 3%.
    pub interest_rate: f64,
    /// ISO code from the balance sensor's unit, e.g. "USD".
    pub currency: String,
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
    /// "MON".."SUN", from the date itself.
    pub label: String,
    pub state: DayState,
    pub today: bool,
    /// Set by hand rather than decided by the chore list.
    pub manual: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Stars,
    ShortTerm,
    LongTerm,
}

/// Which member an entity belongs to and what it holds, if it is one of the
/// integration's per-member reward or money sensors.
fn classify(entity_id: &str) -> Option<(&str, Kind)> {
    let rest = entity_id.strip_prefix(ENTITY_PREFIX)?;
    [("_stars", Kind::Stars), ("_short_term", Kind::ShortTerm), ("_long_term", Kind::LongTerm)]
        .into_iter()
        .find_map(|(suffix, kind)| rest.strip_suffix(suffix).map(|slug| (slug, kind)))
}

/// The member slug behind a reward or money sensor, if that's what it is.
///
/// These entities share a prefix with the member mapping sensors, so anything
/// enumerating family members has to skip them too.
pub fn member_sensor_slug(entity_id: &str) -> Option<&str> {
    classify(entity_id).map(|(slug, _)| slug)
}

/// Every tracked member in `states`, in a stable order.
pub fn from_states(states: &[EntityState]) -> Vec<Member> {
    #[derive(Default)]
    struct Sensors<'a> {
        stars: Option<&'a EntityState>,
        short_term: Option<&'a EntityState>,
        long_term: Option<&'a EntityState>,
    }

    let mut by_slug: BTreeMap<&str, Sensors> = BTreeMap::new();
    for state in states {
        let Some((slug, kind)) = classify(&state.entity_id) else { continue };
        let sensors = by_slug.entry(slug).or_default();
        match kind {
            Kind::Stars => sensors.stars = Some(state),
            Kind::ShortTerm => sensors.short_term = Some(state),
            Kind::LongTerm => sensors.long_term = Some(state),
        }
    }

    // A BTreeMap, so members come out sorted by slug rather than display name:
    // the columns don't reorder themselves when a nickname changes.
    by_slug
        .into_iter()
        .filter_map(|(slug, sensors)| {
            let week = sensors.stars.and_then(stars_from);
            let money = money_from(sensors.short_term, sensors.long_term);
            if week.is_none() && money.is_none() {
                return None;
            }
            let named = sensors.stars.or(sensors.short_term).or(sensors.long_term)?;
            let week = week.unwrap_or_default();
            Some(Member {
                name: display_name(named, slug),
                has_stars: week.present,
                stars: week.stars,
                goal: week.goal,
                days: week.days,
                chores_done: week.chores_done,
                chores_total: week.chores_total,
                prize_earned: week.prize_earned,
                tablet_time: week.tablet_time,
                stars_needed: week.stars_needed,
                money,
            })
        })
        .collect()
}

#[derive(Default)]
struct Week {
    present: bool,
    stars: i32,
    goal: i32,
    days: Vec<Day>,
    chores_done: i32,
    chores_total: i32,
    prize_earned: bool,
    tablet_time: bool,
    stars_needed: i32,
}

fn stars_from(state: &EntityState) -> Option<Week> {
    let attributes = &state.attributes;
    // Absent for an unavailable sensor, which reports no attributes at all.
    // A member with no week to show is left out rather than drawn as an empty
    // set of days.
    let days = attributes.get("days")?.as_object()?;
    let today = attributes.get("today").and_then(|v| v.as_str()).unwrap_or("");

    // Date-keyed, and ISO dates sort the same as calendar order. The first
    // date is whatever the household chose as the start of the week, so each
    // day is named from its own date rather than by its position.
    let mut dates: Vec<&String> = days.keys().collect();
    dates.sort();

    let days = dates
        .into_iter()
        .map(|date| {
            let record = days.get(date).filter(|value| !value.is_null());
            let star = record.and_then(|r| r.get("star")).and_then(|v| v.as_bool()).unwrap_or(false);
            let manual =
                record.and_then(|r| r.get("source")).and_then(|v| v.as_str()) == Some("manual");
            Day {
                label: weekday_label(date),
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

    Some(Week {
        present: true,
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

/// "MON".."SUN" for an ISO `YYYY-MM-DD` date; empty if it doesn't parse.
fn weekday_label(iso_date: &str) -> String {
    let mut parts = iso_date.splitn(3, '-');
    let date = (|| {
        let year = parts.next()?.parse().ok()?;
        let month = Month::try_from(parts.next()?.parse::<u8>().ok()?).ok()?;
        let day = parts.next()?.parse().ok()?;
        Date::from_calendar_date(year, month, day).ok()
    })();
    match date.map(|d| d.weekday()) {
        Some(Weekday::Monday) => "MON",
        Some(Weekday::Tuesday) => "TUE",
        Some(Weekday::Wednesday) => "WED",
        Some(Weekday::Thursday) => "THU",
        Some(Weekday::Friday) => "FRI",
        Some(Weekday::Saturday) => "SAT",
        Some(Weekday::Sunday) => "SUN",
        None => "",
    }
    .to_string()
}

fn money_from(short_term: Option<&EntityState>, long_term: Option<&EntityState>) -> Option<Money> {
    let cents = |state: Option<&EntityState>| {
        state.and_then(|s| s.state.parse::<f64>().ok()).map(|dollars| (dollars * 100.0).round() as i64)
    };
    let (short, long) = (cents(short_term), cents(long_term));
    if short.is_none() && long.is_none() {
        return None;
    }

    let long_attrs = long_term.map(|s| &s.attributes);
    let accruing = long_attrs
        .and_then(|a| a.get("accruing"))
        .and_then(|v| v.as_f64())
        .map(|dollars| (dollars * 100.0).round() as i64)
        .unwrap_or(0);
    let interest_rate =
        long_attrs.and_then(|a| a.get("interest_rate")).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let currency = [short_term, long_term]
        .into_iter()
        .flatten()
        .find_map(|s| s.attributes.get("unit_of_measurement").and_then(|v| v.as_str()))
        .unwrap_or("USD")
        .to_string();

    Some(Money {
        short_term_cents: short.unwrap_or(0),
        long_term_cents: long.unwrap_or(0),
        accruing_cents: accruing,
        interest_rate,
        currency,
    })
}

impl Money {
    pub fn short_term(&self) -> String {
        format_amount(self.short_term_cents, &self.currency)
    }

    pub fn long_term(&self) -> String {
        format_amount(self.long_term_cents, &self.currency)
    }

    /// "+$0.01 accruing · 3% a year" -- whichever halves there are, empty if
    /// the long-term account earns nothing.
    pub fn long_term_note(&self) -> String {
        let mut parts = Vec::new();
        if self.accruing_cents > 0 {
            parts.push(format!("+{} accruing", format_amount(self.accruing_cents, &self.currency)));
        }
        if self.interest_rate > 0.0 {
            parts.push(format!("{} a year", format_percent(self.interest_rate)));
        }
        parts.join(" \u{b7} ")
    }
}

fn format_percent(rate: f64) -> String {
    let rounded = (rate * 100.0).round() / 100.0;
    format!("{rounded}%")
}

/// "$1,234.56", "-$5.00", "EUR 3.20" -- a symbol for the currencies that have
/// an unambiguous one, the code otherwise.
fn format_amount(cents: i64, currency: &str) -> String {
    let symbol = match currency {
        "USD" | "CAD" | "AUD" | "NZD" | "MXN" => "$",
        "EUR" => "\u{20ac}",
        "GBP" => "\u{a3}",
        "JPY" | "CNY" => "\u{a5}",
        _ => "",
    };
    let prefix = if symbol.is_empty() { format!("{currency} ") } else { symbol.to_string() };

    let magnitude = cents.unsigned_abs();
    let whole = (magnitude / 100).to_string();
    let mut grouped = String::new();
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}{prefix}{grouped}.{:02}", magnitude % 100)
}

/// The member's own name, not the sensor's.
///
/// The integration names these entities "<Name> stars", "<Name> short term"
/// and "<Name> long term"; the column is headed by the person, so drop the
/// suffix. Falls back to the slug if some future version names them
/// differently.
fn display_name(state: &EntityState, slug: &str) -> String {
    let friendly = state.attributes.get("friendly_name").and_then(|v| v.as_str());
    match friendly {
        Some(name) => [" stars", " short term", " long term"]
            .into_iter()
            .find_map(|suffix| name.strip_suffix(suffix))
            .unwrap_or(name)
            .to_string(),
        None => slug.replace('_', " "),
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
    fn a_week_reads_in_date_order_with_today_marked() {
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

    /// A household that starts its week on Sunday: the integration sends the
    /// seven dates from that Sunday, so the first column must say SUN.
    #[test]
    fn a_week_starting_on_sunday_is_labelled_from_its_dates() {
        let mut state = ava();
        state.attributes["week_start"] = serde_json::json!("2026-10-04");
        state.attributes["today"] = serde_json::json!("2026-10-06");
        state.attributes["days"] = serde_json::json!({
            "2026-10-04": { "star": true, "source": "auto" },
            "2026-10-05": { "star": true, "source": "auto" },
            "2026-10-06": { "star": false, "source": "auto" },
            "2026-10-07": null,
            "2026-10-08": null,
            "2026-10-09": null,
            "2026-10-10": null,
        });
        let ava = &from_states(&[state])[0];
        let labels: Vec<&str> = ava.days.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels, ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"]);
        assert!(ava.days[2].today);
        assert_eq!(ava.days[0].state, DayState::Earned);
    }

    fn balance(entity_id: &str, state: &str, attributes: serde_json::Value) -> EntityState {
        stars_sensor(entity_id, state, attributes)
    }

    fn ava_money() -> Vec<EntityState> {
        vec![
            balance(
                "sensor.skylight_family_ava_short_term",
                "0.0",
                serde_json::json!({
                    "friendly_name": "Ava short term",
                    "unit_of_measurement": "USD",
                    "account": "short_term",
                }),
            ),
            balance(
                "sensor.skylight_family_ava_long_term",
                "24.0",
                serde_json::json!({
                    "friendly_name": "Ava long term",
                    "unit_of_measurement": "USD",
                    "account": "long_term",
                    "interest_rate": 3.0,
                    "interest_total": 0.5,
                    "accruing": 0.01,
                    "last_interest_date": "2026-10-05",
                }),
            ),
        ]
    }

    #[test]
    fn money_joins_the_same_members_column_as_their_stars() {
        let mut states = vec![ava()];
        states.extend(ava_money());
        let members = from_states(&states);
        assert_eq!(members.len(), 1, "stars and balances are one person, not three");
        let ava = &members[0];
        assert!(ava.has_stars);
        let money = ava.money.as_ref().expect("money");
        assert_eq!(money.short_term(), "$0.00");
        assert_eq!(money.long_term(), "$24.00");
        assert_eq!(money.long_term_note(), "+$0.01 accruing \u{b7} 3% a year");
    }

    /// Reward tracking and pocket money are switched on separately.
    #[test]
    fn a_member_can_have_money_without_stars() {
        let members = from_states(&ava_money());
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, "Ava");
        assert!(!members[0].has_stars);
        assert!(members[0].days.is_empty());
        assert!(members[0].money.is_some());
    }

    #[test]
    fn a_member_can_have_stars_without_money() {
        let members = from_states(&[ava()]);
        assert!(members[0].money.is_none());
    }

    #[test]
    fn nothing_accruing_and_no_interest_leaves_the_note_empty() {
        let mut money = from_states(&ava_money())[0].money.clone().unwrap();
        money.accruing_cents = 0;
        assert_eq!(money.long_term_note(), "3% a year");
        money.interest_rate = 0.0;
        assert_eq!(money.long_term_note(), "");
    }

    #[test]
    fn amounts_read_like_money() {
        assert_eq!(format_amount(0, "USD"), "$0.00");
        assert_eq!(format_amount(5, "USD"), "$0.05");
        assert_eq!(format_amount(123456, "USD"), "$1,234.56");
        assert_eq!(format_amount(100_000_000, "USD"), "$1,000,000.00");
        assert_eq!(format_amount(-500, "USD"), "-$5.00");
        assert_eq!(format_amount(320, "EUR"), "\u{20ac}3.20");
        assert_eq!(format_amount(320, "CHF"), "CHF 3.20");
        assert_eq!(format_percent(3.0), "3%");
        assert_eq!(format_percent(3.5), "3.5%");
    }

    #[test]
    fn every_member_sensor_is_recognised_and_nothing_else() {
        for id in [
            "sensor.skylight_family_ava_stars",
            "sensor.skylight_family_ava_short_term",
            "sensor.skylight_family_ava_long_term",
        ] {
            assert_eq!(member_sensor_slug(id), Some("ava"), "{id}");
        }
        assert_eq!(member_sensor_slug("sensor.skylight_family_ava"), None);
        assert_eq!(member_sensor_slug("sensor.living_room_stars"), None);
    }
}
