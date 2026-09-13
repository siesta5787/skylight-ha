use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use dashboard_config::Config;
use ha_client::entities::{CalendarEvent, TodoItem, TodoStatus};
use ha_client::{Client, RestClient};
use slint::{ComponentHandle, Model, SharedString};
use time::{Date, Duration as TimeDuration, OffsetDateTime, UtcOffset, Weekday};
use ui::{
    AllDayBannerData, AppWindow, CalendarDayData, CalendarEventDot, EventFormMember,
    MemberChipData, TodoColumnData, TodoItemData, WeekDayColumnData, WeekEventData,
};

fn main() {
    tracing_subscriber::fmt::init();

    // `time`'s local-offset lookup isn't sound once other threads exist (on
    // Unix it reads TZ state that a concurrent fork/exec could race), so this
    // has to happen before the tokio runtime spawns any worker threads.
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);

    let config_path = std::env::args().nth(1).unwrap_or_else(|| "config.toml".into());
    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("failed to load config {config_path}: {err}");
            std::process::exit(1);
        }
    };

    let app = AppWindow::new().expect("failed to create window");
    app.set_hour_labels(slint::ModelRc::new(slint::VecModel::from(hour_labels())));

    // Correctly-shaped but empty grids/columns, so the layout is right
    // immediately rather than popping in once HA responds.
    let empty_grids = build_calendar_grids(local_offset, &[]);
    apply_calendar_grids(&app, empty_grids);
    let (empty_todos, _, empty_chips) =
        build_todo_model(&config.family, &vec![Vec::new(); config.family.len()]);
    app.set_todo_columns(empty_todos);
    app.set_members(slint::ModelRc::new(slint::VecModel::from(empty_chips)));
    // Set once, here, and never touched again -- toggling a chip afterward
    // is pure `.slint` state (see app-window.slint's `member-visible`).
    app.set_member_visible(slint::ModelRc::new(slint::VecModel::from(vec![
        true;
        config.family.len()
    ])));
    app.set_event_form_members(slint::ModelRc::new(slint::VecModel::from(
        config
            .family
            .iter()
            .map(|m| EventFormMember {
                name: m.name.clone().into(),
                color: parse_hex_color(&m.color),
            })
            .collect::<Vec<_>>(),
    )));

    // Runtime is created up front (rather than just before `app.run()`, as
    // before) so its `Handle` can be captured by callbacks below that need
    // to spawn HA calls from synchronous UI callbacks.
    let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let rt_handle = rt.handle().clone();
    let _guard = rt.enter();

    // Set once the HA WS connection comes up; several callbacks below are
    // registered before that happens, so they reach through this.
    let live_client: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
    // [column][item] -> that item's HA uid, refreshed on every periodic
    // fetch so the todo-toggle callback can resolve which item was tapped.
    let todo_uids: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let todo_entities: Vec<String> =
        config.family.iter().map(|m| m.todo_entity.clone()).collect();
    let calendar_entities: Vec<Option<String>> =
        config.family.iter().map(|m| m.calendar_entity.clone()).collect();
    // (date, hour) of the last-tapped empty calendar slot, read back when
    // the create-event form is confirmed. UI-thread-only, so a plain
    // `Rc<RefCell<_>>` is fine -- unlike `live_client`/`todo_uids`, nothing
    // here ever crosses onto a tokio worker thread.
    let pending_slot: Rc<RefCell<Option<(Date, u8)>>> = Rc::new(RefCell::new(None));

    {
        let app_weak = app.as_weak();
        let live_client = live_client.clone();
        let todo_uids = todo_uids.clone();
        let rt_handle = rt_handle.clone();
        app.on_todo_item_toggled(move |col, item| {
            let Some(app) = app_weak.upgrade() else { return };
            let Some(entity_id) = todo_entities.get(col as usize) else { return };
            let Some(uid) = todo_uids
                .lock()
                .unwrap()
                .get(col as usize)
                .and_then(|items| items.get(item as usize).cloned())
            else {
                return;
            };

            // Optimistic UI update -- flip the checkbox immediately rather
            // than waiting on the HA round-trip; a failed call gets quietly
            // corrected by the next periodic refresh.
            let Some(column) = app.get_todo_columns().row_data(col as usize) else { return };
            let Some(mut entry) = column.items.row_data(item as usize) else { return };
            entry.completed = !entry.completed;
            let new_status =
                if entry.completed { TodoStatus::Completed } else { TodoStatus::NeedsAction };
            column.items.set_row_data(item as usize, entry);

            if let Some(client) = live_client.lock().unwrap().clone() {
                let entity_id = entity_id.clone();
                rt_handle.spawn(async move {
                    if let Err(err) = client.todo_update_item(&entity_id, &uid, new_status).await {
                        tracing::warn!(%err, "failed to update HA todo item");
                    }
                });
            }
        });
    }

    // Slot taps: Month/Week/Day view all forward here, resolving to an
    // actual (date, hour) themselves -- Week/Day/Month never show anything
    // but the current week/today/this month, so "column 3" or grid cell
    // (2,4) is always unambiguous, no extra data needs to flow out of Slint.
    {
        let app_weak = app.as_weak();
        let pending_slot = pending_slot.clone();
        app.on_month_slot_tapped(move |week, day| {
            let Some(app) = app_weak.upgrade() else { return };
            let (grid_start, _) = month_grid_range(local_offset);
            let date = grid_start + TimeDuration::days(week as i64 * 7 + day as i64);
            open_event_form(&app, &pending_slot, date, 9);
        });
    }
    {
        let app_weak = app.as_weak();
        let pending_slot = pending_slot.clone();
        app.on_week_slot_tapped(move |col, hour| {
            let Some(app) = app_weak.upgrade() else { return };
            let today = OffsetDateTime::now_utc().to_offset(local_offset).date();
            let week_start =
                today - TimeDuration::days(today.weekday().number_days_from_sunday() as i64);
            let date = week_start + TimeDuration::days(col as i64);
            open_event_form(&app, &pending_slot, date, hour as u8);
        });
    }
    {
        let app_weak = app.as_weak();
        let pending_slot = pending_slot.clone();
        app.on_day_slot_tapped(move |hour| {
            let Some(app) = app_weak.upgrade() else { return };
            let today = OffsetDateTime::now_utc().to_offset(local_offset).date();
            open_event_form(&app, &pending_slot, today, hour as u8);
        });
    }
    {
        let app_weak = app.as_weak();
        let pending_slot = pending_slot.clone();
        app.on_new_event_requested(move || {
            let Some(app) = app_weak.upgrade() else { return };
            let now = OffsetDateTime::now_utc().to_offset(local_offset);
            let next_hour = (now.hour() as u16 + 1).min(23) as u8;
            open_event_form(&app, &pending_slot, now.date(), next_hour);
        });
    }
    {
        let live_client = live_client.clone();
        let rt_handle = rt_handle.clone();
        let pending_slot = pending_slot.clone();
        app.on_event_create_confirmed(move |member_index, duration_minutes| {
            let Some((date, hour)) = pending_slot.borrow_mut().take() else { return };
            let Some(Some(entity_id)) =
                calendar_entities.get(member_index as usize).cloned()
            else {
                tracing::warn!(member_index, "selected family member has no calendar_entity configured, can't create event");
                return;
            };
            let Some(client) = live_client.lock().unwrap().clone() else {
                tracing::warn!("not connected to HA yet, can't create event");
                return;
            };
            let Ok(start_naive) = date.with_hms(hour, 0, 0) else { return };
            let start = start_naive.assume_offset(local_offset);
            let end = start + TimeDuration::minutes(duration_minutes as i64);

            rt_handle.spawn(async move {
                // Fixed placeholder title -- no on-screen keyboard exists
                // yet in this app, so free-text entry isn't wired up here.
                if let Err(err) =
                    create_calendar_event(&client, &entity_id, "New Event", start, end).await
                {
                    tracing::warn!(%err, "failed to create HA calendar event");
                }
            });
        });
    }

    let clock_weak = app.as_weak();
    let clock_timer = slint::Timer::default();
    clock_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || {
            if let Some(app) = clock_weak.upgrade() {
                let now = OffsetDateTime::now_utc().to_offset(local_offset);
                app.set_clock_text(format!("{:02}:{:02}", now.hour(), now.minute()).into());
                app.set_date_text(format!("{}", now.date()).into());
                app.set_month_label(format!("{} {}", now.month(), now.year()).into());
            }
        },
    );

    rt.spawn(run_ha_sync(config, app.as_weak(), local_offset, live_client, todo_uids));

    app.run().expect("event loop error");
}

fn open_event_form(
    app: &AppWindow,
    pending_slot: &Rc<RefCell<Option<(Date, u8)>>>,
    date: Date,
    hour: u8,
) {
    *pending_slot.borrow_mut() = Some((date, hour));
    app.set_event_form_date_label(format!("{} {}", weekday_short(date.weekday()), date.day()).into());
    app.set_event_form_time_label(format_hour_label(hour).into());
    app.set_event_form_open(true);
}

async fn create_calendar_event(
    client: &Client,
    entity_id: &str,
    summary: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<(), ha_client::connection::Error> {
    let rfc3339 = &time::format_description::well_known::Rfc3339;
    client
        .call(
            "call_service",
            serde_json::json!({
                "domain": "calendar",
                "service": "create_event",
                "target": { "entity_id": entity_id },
                "service_data": {
                    "summary": summary,
                    "start_date_time": start.format(rfc3339).unwrap_or_default(),
                    "end_date_time": end.format(rfc3339).unwrap_or_default(),
                },
            }),
        )
        .await?;
    Ok(())
}

/// Connects to HA and keeps calendar/todo data flowing into the UI: an
/// initial fetch right after connecting, then a refresh on a fixed
/// interval (`tokio::time::interval`'s first tick fires immediately, so
/// this one loop covers both "on connect" and "periodically").
async fn run_ha_sync(
    config: Config,
    app_weak: slint::Weak<AppWindow>,
    local_offset: UtcOffset,
    live_client: Arc<Mutex<Option<Client>>>,
    todo_uids: Arc<Mutex<Vec<Vec<String>>>>,
) {
    let token = match config.ha.load_token() {
        Ok(token) => token,
        Err(err) => {
            tracing::error!(%err, "failed to load HA token, dashboard will show placeholder data only");
            return;
        }
    };

    let client = ha_client::connect_with_backoff(
        &config.ha.base_url,
        &token,
        std::time::Duration::from_secs(30),
    )
    .await;
    tracing::info!("connected to Home Assistant");
    *live_client.lock().unwrap() = Some(client.clone());

    let rest = RestClient::new(&config.ha.base_url, &token);

    // Same padded 6-week window the month grid needs; it's a superset of
    // "this week"/"today" too, so one fetch covers all four calendar views.
    let (grid_start, grid_end) = month_grid_range(local_offset);
    let range_start = grid_start.midnight().assume_offset(local_offset);
    let range_end = grid_end.midnight().assume_offset(local_offset);

    // docs/plan.md's stated cadence for calendar polling (it isn't pushed
    // over the HA websocket, unlike todos/entity states).
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
    loop {
        interval.tick().await;

        // Fetching returns plain (Send) data -- `CalendarEvent`/`TodoItem`
        // are ordinary serde structs. Building the actual Slint models has
        // to happen below, *inside* `invoke_from_event_loop`: `ModelRc` is
        // `Rc`-based (not `Send`), so it can't be constructed here on the
        // tokio worker thread and then handed across into that closure.
        let per_member_events = fetch_calendar_events(&rest, &config, range_start, range_end).await;
        let per_member_todos = fetch_todos(&client, &config).await;
        let family = config.family.clone();

        let app_weak = app_weak.clone();
        let todo_uids = todo_uids.clone();
        let outcome = slint::invoke_from_event_loop(move || {
            let Some(app) = app_weak.upgrade() else { return };
            let grids = build_calendar_grids(local_offset, &per_member_events);
            apply_calendar_grids(&app, grids);
            let (todo_columns, uid_map, chips) = build_todo_model(&family, &per_member_todos);
            *todo_uids.lock().unwrap() = uid_map;
            app.set_todo_columns(todo_columns);
            app.set_members(slint::ModelRc::new(slint::VecModel::from(chips)));
        });
        if outcome.is_err() {
            break; // window is gone
        }
    }
}

async fn fetch_calendar_events(
    rest: &RestClient,
    config: &Config,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Vec<(usize, slint::Color, Vec<CalendarEvent>)> {
    let mut out = Vec::new();
    for (index, member) in config.family.iter().enumerate() {
        let Some(entity) = &member.calendar_entity else { continue };
        let events = match rest.calendar_events(entity, start, end).await {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(entity = %entity, %err, "failed to fetch calendar events");
                Vec::new()
            }
        };
        out.push((index, parse_hex_color(&member.color), events));
    }
    out
}

async fn fetch_todos(client: &Client, config: &Config) -> Vec<Vec<TodoItem>> {
    let mut out = Vec::with_capacity(config.family.len());
    for member in &config.family {
        let items = match client.todo_items(&member.todo_entity).await {
            Ok(items) => items,
            Err(err) => {
                tracing::warn!(entity = %member.todo_entity, %err, "failed to fetch todo items");
                Vec::new()
            }
        };
        out.push(items);
    }
    out
}

/// The four calendar projections built from one shared fetch -- see
/// `build_calendar_grids`.
struct CalendarGrids {
    month: slint::ModelRc<slint::ModelRc<CalendarDayData>>,
    week: slint::ModelRc<WeekDayColumnData>,
    day: slint::ModelRc<WeekDayColumnData>,
    agenda: slint::ModelRc<WeekDayColumnData>,
}

fn apply_calendar_grids(app: &AppWindow, grids: CalendarGrids) {
    app.set_calendar_month(grids.month);
    app.set_calendar_week(grids.week);
    app.set_calendar_day(grids.day);
    app.set_calendar_agenda(grids.agenda);
}

/// Sunday of the week containing the 1st, through the Saturday of the week
/// containing the month's last day -- the same padded 6-week/42-day range
/// the month grid has always used, now also doubling as the HA fetch window.
fn month_grid_range(local_offset: UtcOffset) -> (Date, Date) {
    let today = OffsetDateTime::now_utc().to_offset(local_offset).date();
    let first_of_month = today.replace_day(1).expect("day 1 is always valid");
    let lead_days = first_of_month.weekday().number_days_from_sunday();
    let grid_start = first_of_month - TimeDuration::days(lead_days as i64);
    (grid_start, grid_start + TimeDuration::days(42))
}

#[derive(Default, Clone)]
struct DayBucket {
    /// Both all-day and timed events, for the Month view's cell listing.
    month_entries: Vec<CalendarEventDot>,
    /// All-day only, for the Week/Day/Agenda header banners.
    banners: Vec<AllDayBannerData>,
    /// Timed only, (start-of-day minutes, event) -- kept together so events
    /// can be sorted chronologically once per day rather than re-deriving
    /// order.
    events: Vec<(i32, WeekEventData)>,
}

/// Builds the Month/Week/Day/Agenda projections from each member's fetched
/// events (`per_member_events` is empty on first paint, before HA has
/// responded -- still produces correctly-shaped, just event-less, grids).
fn build_calendar_grids(
    local_offset: UtcOffset,
    per_member_events: &[(usize, slint::Color, Vec<CalendarEvent>)],
) -> CalendarGrids {
    let today = OffsetDateTime::now_utc().to_offset(local_offset).date();
    let (grid_start, grid_end) = month_grid_range(local_offset);
    let date_fmt = time::macros::format_description!("[year]-[month]-[day]");

    let mut buckets: BTreeMap<Date, DayBucket> = BTreeMap::new();

    for (member_index, color, events) in per_member_events {
        let member_index = *member_index as i32;
        for ev in events {
            if let Some(start_dt) = ev.start.date_time {
                // Multi-day timed events aren't split across days in this
                // pass -- they're bucketed under their start date only.
                let local_start = start_dt.to_offset(local_offset);
                let local_end =
                    ev.end.date_time.map(|dt| dt.to_offset(local_offset)).unwrap_or(local_start);
                let start_minutes = local_start.hour() as i32 * 60 + local_start.minute() as i32;
                // Clamped to a sane minimum so very short (or zero-length,
                // e.g. malformed) events still render as a visible block.
                let duration_minutes = ((local_end - local_start).whole_minutes() as i32).max(20);

                let bucket = buckets.entry(local_start.date()).or_default();
                bucket.month_entries.push(CalendarEventDot {
                    summary: ev.summary.clone().into(),
                    member_color: *color,
                    member_index,
                });
                bucket.events.push((
                    start_minutes,
                    WeekEventData {
                        summary: ev.summary.clone().into(),
                        time_label: format_time_range(local_start, local_end).into(),
                        start_minutes,
                        duration_minutes,
                        member_color: *color,
                        member_index,
                    },
                ));
            } else if let Some(date_str) = ev.start.date.as_deref() {
                if let Ok(date) = Date::parse(date_str, &date_fmt) {
                    let bucket = buckets.entry(date).or_default();
                    bucket.month_entries.push(CalendarEventDot {
                        summary: ev.summary.clone().into(),
                        member_color: *color,
                        member_index,
                    });
                    bucket.banners.push(AllDayBannerData {
                        text: ev.summary.clone().into(),
                        member_color: *color,
                        member_index,
                    });
                }
            }
        }
    }

    // Month: every day in the padded 6-week grid.
    let mut month_weeks = Vec::with_capacity(6);
    let mut cursor = grid_start;
    for _ in 0..6 {
        let mut week = Vec::with_capacity(7);
        for _ in 0..7 {
            let dots: Vec<CalendarEventDot> =
                buckets.get(&cursor).map(|b| b.month_entries.clone()).unwrap_or_default();
            week.push(CalendarDayData {
                day_number: cursor.day() as i32,
                in_current_month: cursor.month() == today.month(),
                is_today: cursor == today,
                events: slint::ModelRc::new(slint::VecModel::from(dots)),
            });
            cursor += TimeDuration::days(1);
        }
        month_weeks.push(slint::ModelRc::new(slint::VecModel::from(week)));
    }

    let to_column = |date: Date| -> WeekDayColumnData {
        let mut bucket = buckets.get(&date).cloned().unwrap_or_default();
        bucket.events.sort_by_key(|(start, _)| *start);
        let events: Vec<WeekEventData> = bucket.events.into_iter().map(|(_, e)| e).collect();
        WeekDayColumnData {
            day_name: weekday_short(date.weekday()).into(),
            day_number: date.day() as i32,
            is_today: date == today,
            all_day_banners: slint::ModelRc::new(slint::VecModel::from(bucket.banners)),
            events: slint::ModelRc::new(slint::VecModel::from(events)),
        }
    };

    let week_start = today - TimeDuration::days(today.weekday().number_days_from_sunday() as i64);
    let week_columns: Vec<WeekDayColumnData> =
        (0..7).map(|i| to_column(week_start + TimeDuration::days(i))).collect();
    let day_columns = vec![to_column(today)];

    let agenda_columns: Vec<WeekDayColumnData> = buckets
        .range(today..grid_end)
        .filter(|(_, b)| !b.banners.is_empty() || !b.events.is_empty())
        .map(|(date, _)| to_column(*date))
        .collect();

    CalendarGrids {
        month: slint::ModelRc::new(slint::VecModel::from(month_weeks)),
        week: slint::ModelRc::new(slint::VecModel::from(week_columns)),
        day: slint::ModelRc::new(slint::VecModel::from(day_columns)),
        agenda: slint::ModelRc::new(slint::VecModel::from(agenda_columns)),
    }
}

/// One column/tab per configured family member: their todo list (for the
/// Tasks page), the uid of each item (so the toggle callback can resolve
/// which item was tapped), and their completion ratio (for the top-bar chip).
fn build_todo_model(
    family: &[dashboard_config::FamilyMember],
    per_member_items: &[Vec<TodoItem>],
) -> (slint::ModelRc<TodoColumnData>, Vec<Vec<String>>, Vec<MemberChipData>) {
    let mut columns = Vec::with_capacity(family.len());
    let mut uid_map = Vec::with_capacity(family.len());
    let mut chips = Vec::with_capacity(family.len());

    for (member, items) in family.iter().zip(per_member_items) {
        let color = parse_hex_color(&member.color);
        let completed = items.iter().filter(|i| i.status == TodoStatus::Completed).count();

        uid_map.push(items.iter().map(|i| i.uid.clone()).collect());

        let item_data: Vec<TodoItemData> = items
            .iter()
            .map(|i| TodoItemData {
                summary: i.summary.clone().into(),
                completed: i.status == TodoStatus::Completed,
            })
            .collect();
        columns.push(TodoColumnData {
            member_name: member.name.clone().into(),
            member_color: color,
            items: slint::ModelRc::new(slint::VecModel::from(item_data)),
        });

        chips.push(MemberChipData {
            name: member.name.clone().into(),
            color,
            completed: completed as i32,
            total: items.len() as i32,
        });
    }

    (slint::ModelRc::new(slint::VecModel::from(columns)), uid_map, chips)
}

fn weekday_short(w: Weekday) -> &'static str {
    match w {
        Weekday::Sunday => "Sun",
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
    }
}

/// `["12 AM", "1 AM", ..., "11 PM"]` -- built once; Slint's expression
/// language has no numeric-to-label formatting to do this in `.slint`.
fn hour_labels() -> Vec<SharedString> {
    (0..24_i32).map(|h| SharedString::from(format_hour_label(h as u8))).collect()
}

fn format_hour_label(hour: u8) -> String {
    format_12h(hour, 0)
}

fn format_time_range(start: OffsetDateTime, end: OffsetDateTime) -> String {
    format!("{} - {}", format_time_12h(start), format_time_12h(end))
}

fn format_time_12h(dt: OffsetDateTime) -> String {
    format_12h(dt.hour(), dt.minute())
}

fn format_12h(hour: u8, minute: u8) -> String {
    let (label_hour, suffix) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    format!("{label_hour}:{minute:02} {suffix}")
}

fn parse_hex_color(hex: &str) -> slint::Color {
    let hex = hex.trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16).unwrap_or(0x6c8dfa);
    let [_, r, g, b] = value.to_be_bytes();
    slint::Color::from_rgb_u8(r, g, b)
}
