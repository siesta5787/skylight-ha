use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use dashboard_config::{Config, DashboardSection, FamilyMember};
use ha_client::entities::{CalendarEvent, DailyForecast, EntityState, TodoItem, TodoStatus};
use ha_client::{Client, RestClient};
use slint::{ComponentHandle, Model, SharedString};
use time::{Date, Duration as TimeDuration, Month, OffsetDateTime, UtcOffset, Weekday};
use ui::{
    AllDayBannerData, AppWindow, CalendarDayData, CalendarEventDot, DashboardCardData,
    DashboardRowData, EventFormMember, MemberChipData, SensorRowData, TodoColumnData,
    TodoItemData, ToggleEntityData, WeekDayColumnData, WeekEventData,
};

fn main() {
    tracing_subscriber::fmt::init();

    // `time`'s local-offset lookup isn't sound once other threads exist (on
    // Unix it reads TZ state that a concurrent fork/exec could race), so this
    // has to happen before the tokio runtime spawns any worker threads.
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let today = OffsetDateTime::now_utc().to_offset(local_offset).date();

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
    app.set_month_label(month_label_for(today).into());

    // Correctly-shaped but empty grids/columns, so the layout is right
    // immediately rather than popping in once HA responds. The family
    // roster itself (chips/columns/member-visible/event-form-members) stays
    // empty until `run_ha_sync` resolves it below -- `config.family` is
    // usually empty too (see `discover_family`), so there's nothing
    // meaningful to seed it with yet anyway.
    let empty_grids = build_calendar_grids(local_offset, today, &[], &[]);
    apply_calendar_grids(&app, empty_grids);
    let (empty_todos, _, empty_chips) =
        build_todo_model(&config.family, &vec![Vec::new(); config.family.len()]);
    app.set_todo_columns(empty_todos);
    app.set_members(slint::ModelRc::new(slint::VecModel::from(empty_chips)));
    apply_family_roster(&app, &config.family);

    // Runtime is created up front (rather than just before `app.run()`, as
    // before) so its `Handle` can be captured by callbacks below that need
    // to spawn HA calls from synchronous UI callbacks.
    let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let rt_handle = rt.handle().clone();
    let _guard = rt.enter();

    // Set once the HA connection comes up; several callbacks below are
    // registered before that happens, so they reach through these.
    let live_client: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
    let live_rest: Arc<Mutex<Option<RestClient>>> = Arc::new(Mutex::new(None));
    // The actual family roster -- either `config.family` verbatim (if you
    // filled it in) or auto-discovered from HA's todo/calendar entities
    // (see `discover_family`) once `run_ha_sync` connects. Every callback
    // below reads this fresh at call time rather than capturing a snapshot,
    // since it isn't known for certain until after connecting.
    let family_state: Arc<Mutex<Vec<FamilyMember>>> = Arc::new(Mutex::new(config.family.clone()));
    // Which `weather.*` entity(ies) feed the weather widget -- `primary`
    // from `config.weather_entity` (condition/temperature/wind/forecast)
    // and `backfill` from `config.weather_backfill_entity` (humidity/
    // pressure, for integrations that don't expose them on the primary
    // entity), else auto-discovered once `run_ha_sync` connects (see
    // `discover_weather_entity`/`discover_weather_backfill_entity`). Same
    // shared-state shape as `family_state` since it's read from
    // `refresh_calendar_and_todos`'s other call site too (after creating a
    // calendar event).
    let weather_entities: Arc<Mutex<WeatherEntities>> = Arc::new(Mutex::new(WeatherEntities {
        primary: config.weather_entity.clone(),
        backfill: config.weather_backfill_entity.clone(),
    }));
    // The Dashboard page's cards -- `config.dashboard` if non-empty, else
    // auto-discovered from `sensor.skylight_dashboard_*` entities (Phase 2,
    // not yet built on the skylight-family HA integration's side --
    // `discover_dashboard_sections` returns `None` until it is). Same
    // override-else-auto-discover shape as `family_state`/`weather_entities`.
    let dashboard_sections: Arc<Mutex<Vec<DashboardSection>>> =
        Arc::new(Mutex::new(config.dashboard.clone()));
    // (column index into the Tasks page) -> (that column's todo entity id,
    // the uid of each item) -- only members with a todo_entity get a
    // column, so this is a *different*, potentially shorter, index space
    // than `family_state`. Refreshed on every fetch.
    let todo_uids: Arc<Mutex<Vec<(String, Vec<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    // The date Month/Week/Day view are currently centered on -- shared
    // across all three (switching view mode keeps your place), moved by the
    // nav-prev/next/today callbacks below. `Arc<Mutex<_>>` rather than
    // `Rc<RefCell<_>>` because the periodic refresh reads it from a tokio
    // worker thread.
    let reference_date: Arc<Mutex<Date>> = Arc::new(Mutex::new(today));
    // (date, hour) of the last-tapped empty calendar slot, read back when
    // the create-event form is confirmed. UI-thread-only, so a plain
    // `Rc<RefCell<_>>` is fine -- unlike the `Arc<Mutex<_>>`s above, nothing
    // here ever crosses onto a tokio worker thread.
    let pending_slot: Rc<RefCell<Option<(Date, u8)>>> = Rc::new(RefCell::new(None));
    // The virtual keyboard (see virtual-keyboard.slint) owns no text state
    // itself -- Slint's expression language has no string slicing/case
    // conversion to implement backspace/shift there, so every keystroke
    // bubbles up here instead. UI-thread-only, same reasoning as
    // `pending_slot`. `keyboard_target` records what "Done" should actually
    // do with the typed text -- currently just adding a task, but built to
    // grow (e.g. a free-text event title) without changing the keyboard
    // component itself.
    let keyboard_buffer: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let keyboard_target: Rc<RefCell<Option<KeyboardTarget>>> = Rc::new(RefCell::new(None));
    // The event-creation form's title, edited via the keyboard (a separate
    // modal on top of the form) -- reset to the default each time a new
    // slot/day/"+" is tapped, in `open_event_form`.
    let event_form_title: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    {
        let app_weak = app.as_weak();
        let live_client = live_client.clone();
        let todo_uids = todo_uids.clone();
        let rt_handle = rt_handle.clone();
        app.on_todo_item_toggled(move |col, item| {
            let Some(app) = app_weak.upgrade() else { return };
            let entry = todo_uids
                .lock()
                .unwrap()
                .get(col as usize)
                .and_then(|(entity_id, uids)| {
                    uids.get(item as usize).map(|uid| (entity_id.clone(), uid.clone()))
                });
            let Some((entity_id, uid)) = entry else { return };

            // Optimistic UI update -- flip the checkbox immediately rather
            // than waiting on the HA round-trip; a failed call gets quietly
            // corrected by the next periodic refresh.
            let Some(column) = app.get_todo_columns().row_data(col as usize) else { return };
            let Some(mut item_entry) = column.items.row_data(item as usize) else { return };
            item_entry.completed = !item_entry.completed;
            let new_status = if item_entry.completed {
                TodoStatus::Completed
            } else {
                TodoStatus::NeedsAction
            };
            column.items.set_row_data(item as usize, item_entry);

            match live_client.lock().unwrap().clone() {
                Some(client) => {
                    rt_handle.spawn(async move {
                        if let Err(err) = client.todo_update_item(&entity_id, &uid, new_status).await {
                            tracing::warn!(%err, "failed to update HA todo item");
                        }
                    });
                }
                // Previously fell through here silently -- toggling while
                // disconnected (e.g. mid-reconnect) looked identical to a
                // successful write that just hadn't landed yet, until the
                // next refresh reverted it with no indication why.
                None => tracing::warn!("not connected to HA, todo change won't be saved"),
            }
        });
    }

    // "Add task": opens the virtual keyboard targeted at this column.
    {
        let app_weak = app.as_weak();
        let keyboard_target = keyboard_target.clone();
        let keyboard_buffer = keyboard_buffer.clone();
        app.on_add_task_requested(move |col| {
            let Some(app) = app_weak.upgrade() else { return };
            let member_name = app
                .get_todo_columns()
                .row_data(col as usize)
                .map(|c| c.member_name.to_string())
                .unwrap_or_default();
            *keyboard_target.borrow_mut() = Some(KeyboardTarget::NewTaskSummary { column: col });
            *keyboard_buffer.borrow_mut() = String::new();
            app.set_keyboard_text("".into());
            app.set_keyboard_prompt(format!("New task for {member_name}").into());
            app.set_keyboard_open(true);
        });
    }
    {
        let app_weak = app.as_weak();
        let keyboard_buffer = keyboard_buffer.clone();
        app.on_keyboard_key_pressed(move |ch| {
            let Some(app) = app_weak.upgrade() else { return };
            let mut buf = keyboard_buffer.borrow_mut();
            buf.push_str(&ch);
            app.set_keyboard_text(buf.clone().into());
        });
    }
    {
        let app_weak = app.as_weak();
        let keyboard_buffer = keyboard_buffer.clone();
        app.on_keyboard_backspace_pressed(move || {
            let Some(app) = app_weak.upgrade() else { return };
            let mut buf = keyboard_buffer.borrow_mut();
            buf.pop();
            app.set_keyboard_text(buf.clone().into());
        });
    }
    {
        let app_weak = app.as_weak();
        let keyboard_buffer = keyboard_buffer.clone();
        let keyboard_target = keyboard_target.clone();
        app.on_keyboard_cancelled(move || {
            let Some(app) = app_weak.upgrade() else { return };
            app.set_keyboard_open(false);
            *keyboard_buffer.borrow_mut() = String::new();
            keyboard_target.borrow_mut().take();
            app.set_keyboard_text("".into());
        });
    }
    {
        let app_weak = app.as_weak();
        let live_client = live_client.clone();
        let rt_handle = rt_handle.clone();
        let todo_uids = todo_uids.clone();
        let keyboard_buffer = keyboard_buffer.clone();
        let keyboard_target = keyboard_target.clone();
        let event_form_title = event_form_title.clone();
        app.on_keyboard_done(move || {
            let Some(app) = app_weak.upgrade() else { return };
            app.set_keyboard_open(false);
            let text = keyboard_buffer.borrow().trim().to_string();
            *keyboard_buffer.borrow_mut() = String::new();
            app.set_keyboard_text("".into());
            let Some(target) = keyboard_target.borrow_mut().take() else { return };
            if text.is_empty() {
                return;
            }

            match target {
                KeyboardTarget::NewTaskSummary { column } => {
                    let Some(entity_id) =
                        todo_uids.lock().unwrap().get(column as usize).map(|(id, _)| id.clone())
                    else {
                        return;
                    };
                    let Some(client) = live_client.lock().unwrap().clone() else {
                        tracing::warn!("not connected to HA, new task won't be saved");
                        return;
                    };
                    rt_handle.spawn(async move {
                        // No optimistic UI insert here (unlike the toggle
                        // callback) -- adding a task doesn't have a
                        // client-side uid to give it yet, and the new
                        // item's own state_changed push (see run_ha_sync)
                        // picks it up within moments regardless.
                        if let Err(err) = add_todo_item(&client, &entity_id, &text).await {
                            tracing::warn!(%err, "failed to add HA todo item");
                        }
                    });
                }
                KeyboardTarget::EventTitle => {
                    *event_form_title.borrow_mut() = text.clone();
                    app.set_event_form_title(text.into());
                }
            }
        });
    }

    // Slot taps: Week/Day/Month view all forward here, resolving to an
    // actual (date, hour) themselves using `reference_date` -- no extra
    // data needs to flow out of Slint.
    {
        let app_weak = app.as_weak();
        let pending_slot = pending_slot.clone();
        let event_form_title = event_form_title.clone();
        let reference_date = reference_date.clone();
        app.on_month_slot_tapped(move |week, day| {
            let Some(app) = app_weak.upgrade() else { return };
            let ref_date = *reference_date.lock().unwrap();
            let (grid_start, _) = month_grid_range(ref_date);
            let date = grid_start + TimeDuration::days(week as i64 * 7 + day as i64);
            open_event_form(&app, &pending_slot, &event_form_title, date, 9);
        });
    }
    {
        let app_weak = app.as_weak();
        let pending_slot = pending_slot.clone();
        let event_form_title = event_form_title.clone();
        let reference_date = reference_date.clone();
        app.on_week_slot_tapped(move |col, hour| {
            let Some(app) = app_weak.upgrade() else { return };
            let ref_date = *reference_date.lock().unwrap();
            let week_start =
                ref_date - TimeDuration::days(ref_date.weekday().number_days_from_sunday() as i64);
            let date = week_start + TimeDuration::days(col as i64);
            open_event_form(&app, &pending_slot, &event_form_title, date, hour as u8);
        });
    }
    {
        let app_weak = app.as_weak();
        let pending_slot = pending_slot.clone();
        let event_form_title = event_form_title.clone();
        let reference_date = reference_date.clone();
        app.on_day_slot_tapped(move |hour| {
            let Some(app) = app_weak.upgrade() else { return };
            let ref_date = *reference_date.lock().unwrap();
            open_event_form(&app, &pending_slot, &event_form_title, ref_date, hour as u8);
        });
    }
    {
        let app_weak = app.as_weak();
        let pending_slot = pending_slot.clone();
        let event_form_title = event_form_title.clone();
        app.on_new_event_requested(move || {
            let Some(app) = app_weak.upgrade() else { return };
            let now = OffsetDateTime::now_utc().to_offset(local_offset);
            let next_hour = (now.hour() as u16 + 1).min(23) as u8;
            open_event_form(&app, &pending_slot, &event_form_title, now.date(), next_hour);
        });
    }
    {
        let app_weak = app.as_weak();
        let event_form_title = event_form_title.clone();
        let keyboard_buffer = keyboard_buffer.clone();
        let keyboard_target = keyboard_target.clone();
        app.on_event_form_title_tapped(move || {
            let Some(app) = app_weak.upgrade() else { return };
            // Starts from empty if the title is still the untouched
            // default ("New Event") -- no reason to make the user backspace
            // through it first -- but continues from whatever's there if
            // they're going back to fix something they already typed.
            let current = event_form_title.borrow().clone();
            let start_from = if current == DEFAULT_EVENT_TITLE { String::new() } else { current };
            *keyboard_buffer.borrow_mut() = start_from.clone();
            *keyboard_target.borrow_mut() = Some(KeyboardTarget::EventTitle);
            app.set_keyboard_text(start_from.into());
            app.set_keyboard_prompt("Event title".into());
            app.set_keyboard_open(true);
        });
    }
    {
        let live_client = live_client.clone();
        let live_rest = live_rest.clone();
        let rt_handle = rt_handle.clone();
        let pending_slot = pending_slot.clone();
        let event_form_title = event_form_title.clone();
        let todo_uids = todo_uids.clone();
        let reference_date = reference_date.clone();
        let family_state = family_state.clone();
        let weather_entities = weather_entities.clone();
        let dashboard_sections = dashboard_sections.clone();
        let app_weak = app.as_weak();
        app.on_event_create_confirmed(move |selected_members, duration_minutes| {
            let Some((date, hour)) = pending_slot.borrow_mut().take() else { return };
            let title = event_form_title.borrow().clone();
            // Every calendar belonging to any selected member, deduped --
            // multiple people can be picked (e.g. an event for both kids),
            // and any one of them can have more than one calendar linked
            // (the Skylight Family integration supports that). No fallback
            // to "whoever has a calendar" if the selection resolves to
            // nothing: with multi-select there's no single implicit
            // default left that wouldn't risk silently creating the event
            // somewhere other than what was actually picked.
            let entity_ids: Vec<String> = {
                let family = family_state.lock().unwrap();
                let mut ids: Vec<String> = (0..selected_members.row_count())
                    .filter(|&i| selected_members.row_data(i).unwrap_or(false))
                    .filter_map(|i| family.get(i))
                    .flat_map(|m| m.calendar_entities.iter().cloned())
                    .collect();
                ids.sort();
                ids.dedup();
                ids
            };
            if entity_ids.is_empty() {
                tracing::warn!("no calendar configured for the selected member(s), can't create event");
                return;
            }
            let Some(client) = live_client.lock().unwrap().clone() else {
                tracing::warn!("not connected to HA yet, can't create event");
                return;
            };
            let Ok(start_naive) = date.with_hms(hour, 0, 0) else { return };
            let start = start_naive.assume_offset(local_offset);
            let end = start + TimeDuration::minutes(duration_minutes as i64);

            let app_weak = app_weak.clone();
            let live_rest = live_rest.clone();
            let todo_uids = todo_uids.clone();
            let weather_entities = weather_entities.clone();
            let dashboard_sections = dashboard_sections.clone();
            let ref_date = *reference_date.lock().unwrap();
            let family = family_state.lock().unwrap().clone();
            rt_handle.spawn(async move {
                for entity_id in &entity_ids {
                    if let Err(err) =
                        create_calendar_event(&client, entity_id, &title, start, end).await
                    {
                        tracing::warn!(%err, entity = %entity_id, "failed to create HA calendar event");
                    }
                }
                // The whole point of tapping "Create" is to see it show up
                // -- don't make the user wait up to 5 minutes for the next
                // periodic refresh. (Binding the guard first and dropping it
                // before use matters -- a `MutexGuard` created directly in
                // an `if let` scrutinee stays alive for the whole block,
                // including across the `.await` below, which isn't `Send`.)
                let rest = live_rest.lock().unwrap().clone();
                if let Some(rest) = rest {
                    refresh_calendar_and_todos(
                        &rest,
                        &client,
                        &family,
                        local_offset,
                        ref_date,
                        &app_weak,
                        &todo_uids,
                        &weather_entities,
                        &dashboard_sections,
                    )
                    .await;
                }
            });
        });
    }

    // Prev/next/today: which unit a nav tap moves by (day/week/month) is
    // decided in `.slint` based on the currently-active calendar view mode
    // -- Rust doesn't track that, it just moves `reference_date` by
    // whatever unit it's told and refreshes.
    {
        let app_weak = app.as_weak();
        let live_client = live_client.clone();
        let live_rest = live_rest.clone();
        let rt_handle = rt_handle.clone();
        let todo_uids = todo_uids.clone();
        let reference_date = reference_date.clone();
        let family_state = family_state.clone();
        let weather_entities = weather_entities.clone();
        let dashboard_sections = dashboard_sections.clone();
        app.on_nav_month(move |delta| {
            let new_date = {
                let mut guard = reference_date.lock().unwrap();
                *guard = add_months(*guard, delta);
                *guard
            };
            navigate(&app_weak, new_date);
            let family = family_state.lock().unwrap().clone();
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset, new_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
        });
    }
    {
        let app_weak = app.as_weak();
        let live_client = live_client.clone();
        let live_rest = live_rest.clone();
        let rt_handle = rt_handle.clone();
        let todo_uids = todo_uids.clone();
        let reference_date = reference_date.clone();
        let family_state = family_state.clone();
        let weather_entities = weather_entities.clone();
        let dashboard_sections = dashboard_sections.clone();
        app.on_nav_week(move |delta| {
            let new_date = {
                let mut guard = reference_date.lock().unwrap();
                *guard += TimeDuration::days(7 * delta as i64);
                *guard
            };
            navigate(&app_weak, new_date);
            let family = family_state.lock().unwrap().clone();
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset, new_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
        });
    }
    {
        let app_weak = app.as_weak();
        let live_client = live_client.clone();
        let live_rest = live_rest.clone();
        let rt_handle = rt_handle.clone();
        let todo_uids = todo_uids.clone();
        let reference_date = reference_date.clone();
        let family_state = family_state.clone();
        let weather_entities = weather_entities.clone();
        let dashboard_sections = dashboard_sections.clone();
        app.on_nav_day(move |delta| {
            let new_date = {
                let mut guard = reference_date.lock().unwrap();
                *guard += TimeDuration::days(delta as i64);
                *guard
            };
            navigate(&app_weak, new_date);
            let family = family_state.lock().unwrap().clone();
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset, new_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
        });
    }
    {
        let app_weak = app.as_weak();
        let live_client = live_client.clone();
        let live_rest = live_rest.clone();
        let rt_handle = rt_handle.clone();
        let todo_uids = todo_uids.clone();
        let reference_date = reference_date.clone();
        let family_state = family_state.clone();
        let weather_entities = weather_entities.clone();
        let dashboard_sections = dashboard_sections.clone();
        app.on_nav_today(move || {
            let new_date = OffsetDateTime::now_utc().to_offset(local_offset).date();
            *reference_date.lock().unwrap() = new_date;
            navigate(&app_weak, new_date);
            let family = family_state.lock().unwrap().clone();
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset, new_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
        });
    }
    {
        let app_weak = app.as_weak();
        let live_client = live_client.clone();
        let live_rest = live_rest.clone();
        let rt_handle = rt_handle.clone();
        let todo_uids = todo_uids.clone();
        let reference_date = reference_date.clone();
        let family_state = family_state.clone();
        let weather_entities = weather_entities.clone();
        let dashboard_sections = dashboard_sections.clone();
        app.on_manual_refresh_requested(move || {
            let ref_date = *reference_date.lock().unwrap();
            let family = family_state.lock().unwrap().clone();
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset, ref_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
        });
    }

    // Dashboard controls: each fires its `call_service`, then refreshes --
    // but only the dashboard page (see refresh_dashboard_only), not the
    // full calendar+todos+weather refresh event creation uses. That full
    // refresh is what made these feel slow (3-4s to reflect a toggle that
    // HA itself applies almost instantly): it was doing a REST calendar
    // fetch, a todo fetch, and 3 sequential weather calls before it ever
    // got to the one get_states() the dashboard actually needed. No
    // optimistic client-side model flip (unlike the todo checkbox) --
    // this single WS round trip plus the entity's own state_changed event
    // (which the relevant-state filter below also listens for) both land
    // fast enough now that one wasn't worth the extra bookkeeping of
    // searching the nested card/entity model to mutate it.
    {
        let live_client = live_client.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        let dashboard_sections = dashboard_sections.clone();
        app.on_dashboard_entity_toggled(move |entity_id, on| {
            let Some(client) = live_client.lock().unwrap().clone() else {
                tracing::warn!("not connected to HA, can't toggle entity");
                return;
            };
            let entity_id = entity_id.to_string();
            let app_weak = app_weak.clone();
            let dashboard_sections = dashboard_sections.clone();
            rt_handle.spawn(async move {
                let domain = entity_domain(&entity_id).to_string();
                let service = if on { "turn_on" } else { "turn_off" };
                if let Err(err) = client.call_service(&domain, service, &[entity_id.clone()], serde_json::json!({})).await
                {
                    tracing::warn!(%err, entity = %entity_id, "failed to toggle entity");
                }
                refresh_dashboard_only(&client, &dashboard_sections, &app_weak).await;
            });
        });
    }
    {
        let live_client = live_client.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        let dashboard_sections = dashboard_sections.clone();
        app.on_dashboard_group_toggled(move |entity_ids, on| {
            let Some(client) = live_client.lock().unwrap().clone() else {
                tracing::warn!("not connected to HA, can't toggle group");
                return;
            };
            let ids: Vec<String> =
                (0..entity_ids.row_count()).filter_map(|i| entity_ids.row_data(i)).map(|s| s.to_string()).collect();
            let app_weak = app_weak.clone();
            let dashboard_sections = dashboard_sections.clone();
            rt_handle.spawn(async move {
                // Grouped by domain rather than assumed-homogeneous -- a
                // config section is expected to be all-light or all-fan,
                // but this stays correct even if someone mixes domains in
                // one section, since `light.turn_on`/`fan.turn_on` are
                // different services.
                let mut by_domain: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
                for id in ids {
                    by_domain.entry(entity_domain(&id).to_string()).or_default().push(id);
                }
                let service = if on { "turn_on" } else { "turn_off" };
                for (domain, ids) in by_domain {
                    if let Err(err) = client.call_service(&domain, service, &ids, serde_json::json!({})).await {
                        tracing::warn!(%err, domain = %domain, "failed to toggle group");
                    }
                }
                refresh_dashboard_only(&client, &dashboard_sections, &app_weak).await;
            });
        });
    }
    {
        let live_client = live_client.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        let dashboard_sections = dashboard_sections.clone();
        app.on_dashboard_climate_mode_selected(move |entity_id, mode| {
            let Some(client) = live_client.lock().unwrap().clone() else {
                tracing::warn!("not connected to HA, can't set thermostat mode");
                return;
            };
            let entity_id = entity_id.to_string();
            let mode = mode.to_string();
            let app_weak = app_weak.clone();
            let dashboard_sections = dashboard_sections.clone();
            rt_handle.spawn(async move {
                if let Err(err) = client
                    .call_service(
                        "climate",
                        "set_hvac_mode",
                        &[entity_id.clone()],
                        serde_json::json!({ "hvac_mode": mode }),
                    )
                    .await
                {
                    tracing::warn!(%err, entity = %entity_id, "failed to set thermostat mode");
                }
                refresh_dashboard_only(&client, &dashboard_sections, &app_weak).await;
            });
        });
    }
    {
        let live_client = live_client.clone();
        let live_rest = live_rest.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        let dashboard_sections = dashboard_sections.clone();
        app.on_dashboard_climate_temp_delta(move |entity_id, delta| {
            let (Some(client), Some(rest)) =
                (live_client.lock().unwrap().clone(), live_rest.lock().unwrap().clone())
            else {
                tracing::warn!("not connected to HA, can't change thermostat temperature");
                return;
            };
            let entity_id = entity_id.to_string();
            let app_weak = app_weak.clone();
            let dashboard_sections = dashboard_sections.clone();
            rt_handle.spawn(async move {
                // Reads the entity's current setpoint fresh rather than
                // caching it client-side -- a +/- tap is infrequent enough
                // that the extra round trip doesn't matter, and it avoids
                // keeping a second copy of dashboard state in sync with
                // what's actually on screen.
                let state = match rest.entity_state(&entity_id).await {
                    Ok(state) => state,
                    Err(err) => {
                        tracing::warn!(%err, entity = %entity_id, "failed to read current thermostat state");
                        return;
                    }
                };
                let Some(current) = state.attributes.get("temperature").and_then(|v| v.as_f64()) else {
                    tracing::warn!(entity = %entity_id, "thermostat has no current setpoint to adjust");
                    return;
                };
                let min = state.attributes.get("min_temp").and_then(|v| v.as_f64()).unwrap_or(f64::MIN);
                let max = state.attributes.get("max_temp").and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
                let new_target = (current + delta as f64).clamp(min, max);
                if let Err(err) = client
                    .call_service(
                        "climate",
                        "set_temperature",
                        &[entity_id.clone()],
                        serde_json::json!({ "temperature": new_target }),
                    )
                    .await
                {
                    tracing::warn!(%err, entity = %entity_id, "failed to set thermostat temperature");
                }
                refresh_dashboard_only(&client, &dashboard_sections, &app_weak).await;
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
                // Deliberately not touching `month_label` here -- it tracks
                // `reference_date` (whatever's being navigated/viewed), not
                // wall-clock "now"; the nav callbacks and initial setup own it.
            }
        },
    );

    rt.spawn(run_ha_sync(
        config,
        app.as_weak(),
        local_offset,
        live_client,
        live_rest,
        todo_uids,
        reference_date,
        family_state,
        weather_entities,
        dashboard_sections,
    ));

    app.run().expect("event loop error");
}

fn navigate(app_weak: &slint::Weak<AppWindow>, new_date: Date) {
    if let Some(app) = app_weak.upgrade() {
        app.set_month_label(month_label_for(new_date).into());
    }
}

/// Sets the UI state that only changes when the family roster itself
/// changes (as opposed to `members`/`todo_columns`, which are rebuilt on
/// every fetch): the calendar-visibility toggle array and the
/// event-creation form's member picker.
fn apply_family_roster(app: &AppWindow, family: &[FamilyMember]) {
    app.set_member_visible(slint::ModelRc::new(slint::VecModel::from(vec![true; family.len()])));
    app.set_event_form_members(slint::ModelRc::new(slint::VecModel::from(
        family
            .iter()
            .map(|m| EventFormMember {
                name: m.name.clone().into(),
                color: parse_hex_color(&m.color),
            })
            .collect::<Vec<_>>(),
    )));
}

/// Fetches + rebuilds + pushes to the UI using whichever `Client`/`RestClient`
/// are currently live; does nothing if HA isn't connected yet (the next nav
/// tap or periodic refresh will pick it up once it is).
#[allow(clippy::too_many_arguments)]
fn spawn_refresh(
    rt_handle: &tokio::runtime::Handle,
    live_rest: &Arc<Mutex<Option<RestClient>>>,
    live_client: &Arc<Mutex<Option<Client>>>,
    family: &[FamilyMember],
    local_offset: UtcOffset,
    reference_date: Date,
    app_weak: &slint::Weak<AppWindow>,
    todo_uids: &Arc<Mutex<Vec<(String, Vec<String>)>>>,
    weather_entities: &Arc<Mutex<WeatherEntities>>,
    dashboard_sections: &Arc<Mutex<Vec<DashboardSection>>>,
) {
    let (Some(rest), Some(client)) =
        (live_rest.lock().unwrap().clone(), live_client.lock().unwrap().clone())
    else {
        return;
    };
    let family = family.to_vec();
    let app_weak = app_weak.clone();
    let todo_uids = todo_uids.clone();
    let weather_entities = weather_entities.clone();
    let dashboard_sections = dashboard_sections.clone();
    rt_handle.spawn(async move {
        refresh_calendar_and_todos(
            &rest,
            &client,
            &family,
            local_offset,
            reference_date,
            &app_weak,
            &todo_uids,
            &weather_entities,
            &dashboard_sections,
        )
        .await;
    });
}

const DEFAULT_EVENT_TITLE: &str = "New Event";

fn open_event_form(
    app: &AppWindow,
    pending_slot: &Rc<RefCell<Option<(Date, u8)>>>,
    event_form_title: &Rc<RefCell<String>>,
    date: Date,
    hour: u8,
) {
    *pending_slot.borrow_mut() = Some((date, hour));
    *event_form_title.borrow_mut() = DEFAULT_EVENT_TITLE.to_string();
    app.set_event_form_title(DEFAULT_EVENT_TITLE.into());
    app.set_event_form_date_label(format!("{} {}", weekday_short(date.weekday()), date.day()).into());
    app.set_event_form_time_label(format_hour_label(hour).into());
    // Starts nobody selected each time, forcing a deliberate pick rather
    // than defaulting to whoever was selected last (which risked silently
    // double-booking the wrong person).
    let member_count = app.get_event_form_members().row_count();
    app.set_event_form_selected_members(slint::ModelRc::new(slint::VecModel::from(vec![
        false;
        member_count
    ])));
    app.set_event_form_open(true);
}

/// What the virtual keyboard's "Done" should do with the typed text --
/// see `keyboard_target` in `main`.
enum KeyboardTarget {
    NewTaskSummary { column: i32 },
    EventTitle,
}

/// Which `weather.*` entities feed the weather widget -- see
/// `weather_entities` in `main`.
#[derive(Default, Clone)]
struct WeatherEntities {
    primary: Option<String>,
    backfill: Option<String>,
}

async fn add_todo_item(
    client: &Client,
    entity_id: &str,
    summary: &str,
) -> Result<(), ha_client::connection::Error> {
    client
        .call(
            "call_service",
            serde_json::json!({
                "domain": "todo",
                "service": "add_item",
                "target": { "entity_id": entity_id },
                "service_data": { "item": summary },
            }),
        )
        .await?;
    Ok(())
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

/// One shared fetch (calendar events over the range around `reference_date`,
/// plus todos, plus weather) feeding all four calendar views + the Tasks
/// page + top-bar chips/weather. Used by the periodic refresh, right after
/// creating an event, and by every nav callback. Returns whether the WS
/// connection still looks alive -- see `run_ha_sync`, which reconnects if
/// not.
#[allow(clippy::too_many_arguments)]
async fn refresh_calendar_and_todos(
    rest: &RestClient,
    client: &Client,
    family: &[FamilyMember],
    local_offset: UtcOffset,
    reference_date: Date,
    app_weak: &slint::Weak<AppWindow>,
    todo_uids: &Arc<Mutex<Vec<(String, Vec<String>)>>>,
    weather_entities: &Arc<Mutex<WeatherEntities>>,
    dashboard_sections: &Arc<Mutex<Vec<DashboardSection>>>,
) -> bool {
    let (grid_start, grid_end) = month_grid_range(reference_date);
    let range_start = grid_start.midnight().assume_offset(local_offset);
    let range_end = grid_end.midnight().assume_offset(local_offset);

    // Fetching returns plain (Send) data -- `CalendarEvent`/`TodoItem` are
    // ordinary serde structs. Building the actual Slint models has to
    // happen below, *inside* `invoke_from_event_loop`: `ModelRc` is
    // `Rc`-based (not `Send`), so it can't be constructed on a tokio worker
    // thread and handed across into that closure.
    let per_member_events = fetch_calendar_events(rest, family, range_start, range_end).await;
    let (per_member_todos, connection_alive) = fetch_todos(client, family).await;

    // REST calls, not WS -- there's no dedicated WS query for a single
    // entity's state, and this only needs to happen on the same cadence as
    // the calendar poll above, not on every `state_changed` event. `None`
    // (entity not resolved yet, or the fetch failed) means "leave whatever's
    // already on screen alone" -- same don't-flash-to-placeholder reasoning
    // as the connection-dead check below.
    let (primary_entity, backfill_entity) = {
        let entities = weather_entities.lock().unwrap();
        (entities.primary.clone(), entities.backfill.clone())
    };
    let weather = match &primary_entity {
        Some(entity_id) => match rest.entity_state(entity_id).await {
            Ok(state) => Some(state),
            Err(err) => {
                tracing::warn!(entity = %entity_id, %err, "failed to fetch weather entity state");
                None
            }
        },
        None => None,
    };
    // A day's high/low isn't a plain state attribute on modern HA weather
    // entities -- it needs its own service call (see
    // Client::weather_daily_forecast). WS, not REST: the forecast service
    // isn't exposed over the REST API.
    let forecast_today = match &primary_entity {
        Some(entity_id) => match client.weather_daily_forecast(entity_id).await {
            Ok(days) => days.into_iter().next(),
            Err(err) => {
                tracing::warn!(entity = %entity_id, %err, "failed to fetch weather forecast");
                None
            }
        },
        None => None,
    };
    let backfill = match &backfill_entity {
        Some(entity_id) => match rest.entity_state(entity_id).await {
            Ok(state) => Some(state),
            Err(err) => {
                tracing::warn!(entity = %entity_id, %err, "failed to fetch weather backfill entity state");
                None
            }
        },
        None => None,
    };

    // One `get_states()` covers every configured dashboard entity in a
    // single round trip, same as the family/weather discovery scans --
    // cheap at a home instance's scale, and simpler than a REST call per
    // entity. Only the raw states are fetched here -- `EntityState` is a
    // plain (Send) serde struct, but `DashboardCardData` embeds `ModelRc`s
    // internally (same as every other Slint-facing struct this function
    // builds), so turning these into cards has to happen below, inside
    // `invoke_from_event_loop`, same as `build_calendar_grids`/
    // `build_todo_model`. `None` on failure means "leave the dashboard
    // page as it was" (same don't-flash-to-placeholder reasoning as the
    // weather fetches above) -- an empty `sections` list is different,
    // that's a legitimate "nothing configured" state, not a failure.
    let sections = dashboard_sections.lock().unwrap().clone();
    let dashboard_states: Option<Vec<EntityState>> = if sections.is_empty() {
        Some(Vec::new())
    } else {
        match client.get_states().await {
            Ok(states) => Some(states),
            Err(err) => {
                tracing::warn!(%err, "failed to fetch entity states for the dashboard page");
                None
            }
        }
    };

    if !connection_alive {
        // Don't push this over what's already correctly on screen -- a
        // dead-connection fetch means `per_member_todos` is empty for
        // everyone, and briefly showing that (then the real data popping
        // back in once reconnected) is exactly the flash the previous fix
        // still had: it reconnected promptly, but still committed the
        // empty result to the UI the instant it noticed, before the
        // reconnect even started. Leaving the old data in place and letting
        // the caller's immediate retry-after-reconnect refresh it is the
        // fix -- nothing visibly changes unless a fetch actually succeeds.
        return false;
    }

    let family_owned = family.to_vec();
    let app_weak = app_weak.clone();
    let todo_uids = todo_uids.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let grids = build_calendar_grids(local_offset, reference_date, &family_owned, &per_member_events);
        apply_calendar_grids(&app, grids);
        let (todo_columns, uid_map, chips) = build_todo_model(&family_owned, &per_member_todos);
        *todo_uids.lock().unwrap() = uid_map;
        app.set_todo_columns(todo_columns);
        app.set_members(slint::ModelRc::new(slint::VecModel::from(chips)));
        apply_weather(&app, weather.as_ref(), forecast_today.as_ref(), backfill.as_ref());
        if let Some(states) = &dashboard_states {
            let by_id: std::collections::HashMap<&str, &EntityState> =
                states.iter().map(|s| (s.entity_id.as_str(), s)).collect();
            let cards = build_dashboard_cards(&sections, &by_id);
            let rows = group_dashboard_rows(cards);
            app.set_dashboard_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
        }
    });

    true
}

/// A lighter sibling of `refresh_calendar_and_todos` for the Dashboard
/// page alone -- one `get_states()` round trip, no REST calendar fetch, no
/// todo fetch, no weather calls. Used after a dashboard control (toggle/
/// mode/temp) fires its `call_service`, where the calendar/todos/weather
/// obviously haven't changed and waiting on them was the entire reason a
/// toggle took 3-4 seconds to visibly react even though HA itself applied
/// it almost immediately. Does nothing if no sections are configured.
async fn refresh_dashboard_only(
    client: &Client,
    dashboard_sections: &Arc<Mutex<Vec<DashboardSection>>>,
    app_weak: &slint::Weak<AppWindow>,
) {
    let sections = dashboard_sections.lock().unwrap().clone();
    if sections.is_empty() {
        return;
    }
    let states = match client.get_states().await {
        Ok(states) => states,
        Err(err) => {
            tracing::warn!(%err, "failed to fetch entity states for the dashboard page");
            return;
        }
    };
    let app_weak = app_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(app) = app_weak.upgrade() else { return };
        let by_id: std::collections::HashMap<&str, &EntityState> =
            states.iter().map(|s| (s.entity_id.as_str(), s)).collect();
        let cards = build_dashboard_cards(&sections, &by_id);
        let rows = group_dashboard_rows(cards);
        app.set_dashboard_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    });
}

/// Pushes whatever weather data is actually available onto the widget's
/// properties. Each piece is set independently and only when present --
/// e.g. a forecast-fetch failure shouldn't blank out the current
/// condition/temperature that did come back, and `backfill` only fills in
/// humidity/pressure when `weather` itself doesn't already have them
/// (checked first, so a primary entity that *does* expose them needs no
/// backfill entity configured at all).
fn apply_weather(
    app: &AppWindow,
    weather: Option<&EntityState>,
    forecast_today: Option<&DailyForecast>,
    backfill: Option<&EntityState>,
) {
    let Some(state) = weather else { return };
    let unit = state.attributes.get("temperature_unit").and_then(|v| v.as_str()).unwrap_or("°");

    app.set_weather_icon_condition(state.state.clone().into());
    app.set_weather_condition_label(format_weather_condition_label(&state.state).into());
    if let Some(last_updated) = state.last_updated {
        let elapsed_label = format_relative_time(last_updated, OffsetDateTime::now_utc());
        app.set_weather_updated_label(elapsed_label.into());
    }
    if let Some(temp) = state.attributes.get("temperature").and_then(|v| v.as_f64()) {
        app.set_weather_temp_now(format_weather_temp(temp, unit).into());
    }
    let wind = format_wind(state);
    if !wind.is_empty() {
        app.set_weather_wind(wind.into());
    }
    if let Some(fc) = forecast_today {
        if let (Some(high), Some(low)) = (fc.temperature, fc.templow) {
            app.set_weather_temp_range(format_temp_range(high, low).into());
        }
        if let Some(precip) = fc.precipitation {
            let precip_unit = state.attributes.get("precipitation_unit").and_then(|v| v.as_str()).unwrap_or("");
            app.set_weather_precip(format_precip(precip, precip_unit).into());
        }
    }

    let sources = [Some(state), backfill];
    if let Some(humidity) = weather_numeric_attr(&sources, "humidity") {
        app.set_weather_humidity(format_humidity(humidity).into());
    }
    if let Some(pressure) = weather_numeric_attr(&sources, "pressure") {
        let pressure_unit = weather_string_attr(&sources, "pressure_unit").unwrap_or("");
        app.set_weather_pressure(format_pressure(pressure, pressure_unit).into());
    }
}

/// Connects to HA, resolves the family roster (config.toml's `[[family]]`
/// if you filled it in, otherwise auto-discovered -- see `discover_family`),
/// then keeps calendar/todo data flowing into the UI on a fixed interval
/// (`tokio::time::interval`'s first tick fires immediately, so this one loop
/// covers both "on connect" and "periodically"). Nav taps and event
/// creation refresh independently of this loop via `spawn_refresh`.
///
/// Reconnects (outer loop) whenever the WS connection is detected dead
/// (see `fetch_todos`) rather than connecting exactly once for the life of
/// the process -- `ha_client::Client` doesn't reconnect itself by design
/// (its own doc comment says so explicitly), so something has to.
#[allow(clippy::too_many_arguments)]
async fn run_ha_sync(
    config: Config,
    app_weak: slint::Weak<AppWindow>,
    local_offset: UtcOffset,
    live_client: Arc<Mutex<Option<Client>>>,
    live_rest: Arc<Mutex<Option<RestClient>>>,
    todo_uids: Arc<Mutex<Vec<(String, Vec<String>)>>>,
    reference_date: Arc<Mutex<Date>>,
    family_state: Arc<Mutex<Vec<FamilyMember>>>,
    weather_entities: Arc<Mutex<WeatherEntities>>,
    dashboard_sections: Arc<Mutex<Vec<DashboardSection>>>,
) {
    let token = match config.ha.load_token() {
        Ok(token) => token,
        Err(err) => {
            tracing::error!(%err, "failed to load HA token, dashboard will show placeholder data only");
            return;
        }
    };

    // The family roster only needs resolving once -- re-discovering on
    // every reconnect would be harmless (same HA entities, same result)
    // but would also reset `member_visible`/`event_form_members`, wiping
    // out any chip the user had toggled off.
    let mut family_resolved = false;

    loop {
        let client = ha_client::connect_with_backoff(
            &config.ha.base_url,
            &token,
            std::time::Duration::from_secs(30),
        )
        .await;
        tracing::info!("connected to Home Assistant");
        *live_client.lock().unwrap() = Some(client.clone());
        let rest = RestClient::new(&config.ha.base_url, &token);
        *live_rest.lock().unwrap() = Some(rest.clone());

        if !family_resolved {
            // Priority: `[[family]]` in config.toml (manual override, for
            // custom colors/order/pairing) > the Skylight Family
            // integration's sensor.skylight_family_* entities (a
            // controlled, purpose-built mapping set up once in HA's own
            // Settings UI) > heuristic todo/calendar name-matching (the
            // fallback for anyone who hasn't installed that integration).
            let family = if !config.family.is_empty() {
                config.family.clone()
            } else if let Some(from_integration) =
                discover_family_from_skylight_integration(&client).await
            {
                tracing::info!(
                    count = from_integration.len(),
                    "loaded family roster from the Skylight Family integration"
                );
                from_integration
            } else {
                let discovered = discover_family(&client).await;
                tracing::info!(
                    count = discovered.len(),
                    "discovered family roster from HA todo/calendar entities (Skylight Family integration not found)"
                );
                discovered
            };
            *family_state.lock().unwrap() = family.clone();
            let app_weak_for_roster = app_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak_for_roster.upgrade() {
                    apply_family_roster(&app, &family);
                }
            });
            family_resolved = true;
        }

        // Same override-else-auto-discover shape as the family roster, but
        // resolved independently -- a `[[family]]` override says nothing
        // about which `weather.*` entity to use. `is_none()` (not a
        // separate "resolved" flag) doubles as "keep trying on the next
        // reconnect if it wasn't found yet" -- harmless since a HA restart
        // could add the entity later, and there's no per-member state to
        // preserve the way `family_resolved` protects `member_visible`.
        // Locks aren't held across the `.await`s below (a `MutexGuard` kept
        // alive that way isn't `Send` -- bit this exact function once
        // already, see `fetch_calendar_events`'s call site history).
        let current_primary = weather_entities.lock().unwrap().primary.clone();
        if current_primary.is_none() {
            if let Some(discovered) = discover_weather_entity(&client).await {
                tracing::info!(entity = %discovered, "discovered weather entity");
                weather_entities.lock().unwrap().primary = Some(discovered);
            }
        }
        let current_backfill = weather_entities.lock().unwrap().backfill.clone();
        if current_backfill.is_none() {
            let primary_now = weather_entities.lock().unwrap().primary.clone();
            if let Some(discovered) =
                discover_weather_backfill_entity(&client, primary_now.as_deref()).await
            {
                tracing::info!(
                    entity = %discovered,
                    "discovered weather backfill entity for humidity/pressure"
                );
                weather_entities.lock().unwrap().backfill = Some(discovered);
            }
        }

        // Same retry-every-reconnect-until-found shape as weather above --
        // no per-card UI state a re-resolve could disturb, so there's no
        // reason to resolve only once like the family roster does.
        let current_sections = dashboard_sections.lock().unwrap().clone();
        if current_sections.is_empty() {
            if let Some(discovered) = discover_dashboard_sections(&client).await {
                tracing::info!(
                    count = discovered.len(),
                    "loaded dashboard sections from the Skylight Dashboard integration"
                );
                *dashboard_sections.lock().unwrap() = discovered;
            }
        }

        // Calendar event ranges aren't pushed over the WS event bus (per
        // docs/plan.md), only polled -- but a todo entity's own `state` is
        // its needs-action count, which *does* change (and gets pushed as
        // a state_changed event) the instant an item is added/checked
        // off/removed anywhere, phone included. Subscribing to that turns
        // "wait up to 5 minutes" into "near-instant" for the common case
        // (todos) without needing to poll more aggressively for everything.
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5 * 60));
        let mut state_events = client.subscribe_state_changed();
        loop {
            enum Wake {
                Interval,
                RelevantStateChange,
                Irrelevant,
                ConnectionDead,
            }
            let wake = tokio::select! {
                _ = interval.tick() => Wake::Interval,
                event = state_events.recv() => match event {
                    Ok(state) => {
                        let is_todo = family_state
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|m| m.todo_entity.as_deref() == Some(state.entity_id.as_str()));
                        // Same near-instant treatment for dashboard entities
                        // -- a light toggled from the HA app or a physical
                        // switch should reflect here without waiting for
                        // the 5-minute poll, same as a todo checked off
                        // from a phone.
                        let is_dashboard = dashboard_sections
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|section| dashboard_section_contains(section, &state.entity_id));
                        if is_todo || is_dashboard { Wake::RelevantStateChange } else { Wake::Irrelevant }
                    }
                    // Lagged just means we missed some events under load --
                    // refreshing anyway is the safe default. A closed
                    // channel means the actor (and so the whole connection)
                    // has died, same as a failed fetch below.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Wake::RelevantStateChange,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => Wake::ConnectionDead,
                },
            };
            if matches!(wake, Wake::Irrelevant) {
                continue;
            }
            if matches!(wake, Wake::ConnectionDead) {
                tracing::warn!("HA event stream closed, reconnecting");
                *live_client.lock().unwrap() = None;
                *live_rest.lock().unwrap() = None;
                break;
            }

            let ref_date = *reference_date.lock().unwrap();
            let family = family_state.lock().unwrap().clone();
            let alive = refresh_calendar_and_todos(
                &rest,
                &client,
                &family,
                local_offset,
                ref_date,
                &app_weak,
                &todo_uids,
                &weather_entities,
                &dashboard_sections,
            )
            .await;
            if !alive {
                tracing::warn!("HA connection appears to have dropped, reconnecting");
                *live_client.lock().unwrap() = None;
                *live_rest.lock().unwrap() = None;
                break; // back to the outer loop to reconnect
            }
        }
    }
}

/// Shared by both auto-discovery paths below, for members whose color isn't
/// otherwise known.
const PALETTE: &[&str] = &[
    "#4f8ef7", "#e0607a", "#f2b705", "#8b5cf6", "#22c55e", "#f97316", "#64748b", "#06b6d4",
    "#ec4899", "#84cc16",
];

/// Builds a family roster from the `siesta5787/skylight-family` HA
/// integration (https://github.com/siesta5787/skylight-family), if it's
/// installed and has at least one member configured. This is the
/// purpose-built, controlled source -- explicit person/calendar/todo
/// pairing done once through HA's own Settings UI -- so it takes priority
/// over `discover_family`'s heuristic todo/calendar name-matching below,
/// which exists as a fallback for anyone who hasn't installed it. `None`
/// means "integration not present/configured", not "connection error" (a
/// real fetch error is treated the same way -- if we can't tell, fall back).
async fn discover_family_from_skylight_integration(client: &Client) -> Option<Vec<FamilyMember>> {
    let states = match client.get_states().await {
        Ok(states) => states,
        Err(err) => {
            tracing::warn!(%err, "failed to list HA entities while checking for the Skylight Family integration");
            return None;
        }
    };

    let mut members: Vec<(String, FamilyMember)> = Vec::new(); // (slug, member), sorted after
    for state in &states {
        let Some(slug) = state.entity_id.strip_prefix("sensor.skylight_family_") else { continue };

        let name = state
            .attributes
            .get("friendly_name")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| titlecase_slug(slug));
        let todo_entity =
            state.attributes.get("todo_entity_id").and_then(|v| v.as_str()).map(str::to_string);
        let calendar_entities: Vec<String> = state
            .attributes
            .get("calendar_entity_ids")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|e| e.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let color = parse_ha_color_attribute(state.attributes.get("color"))
            .unwrap_or_else(|| PALETTE[members.len() % PALETTE.len()].to_string());

        members.push((
            slug.to_string(),
            FamilyMember { id: slug.to_string(), name, color, todo_entity, calendar_entities },
        ));
    }

    if members.is_empty() {
        return None; // integration not installed, or installed with nobody configured yet
    }
    members.sort_by(|a, b| a.0.cmp(&b.0)); // stable across runs
    Some(members.into_iter().map(|(_, m)| m).collect())
}

/// Builds a family roster from whatever `todo.*`/`calendar.*` entities
/// exist in HA, rather than requiring `[[family]]` to be hand-written in
/// config.toml. A `todo.*` entity becomes a member (named from its
/// `friendly_name`, falling back to title-casing the entity id); a
/// `calendar.*` entity is attached to the member with the same slug (e.g.
/// `todo.jesse` + `calendar.jesse`) if one exists, otherwise it becomes its
/// own member with no todo list (e.g. a shared household calendar that
/// isn't any one person's). Fallback for when the Skylight Family
/// integration (see `discover_family_from_skylight_integration` above)
/// isn't installed.
async fn discover_family(client: &Client) -> Vec<FamilyMember> {
    let states = match client.get_states().await {
        Ok(states) => states,
        Err(err) => {
            tracing::warn!(%err, "failed to list HA entities for family-roster discovery");
            return Vec::new();
        }
    };

    fn friendly_name(state: &EntityState) -> Option<String> {
        state.attributes.get("friendly_name")?.as_str().map(str::to_string)
    }

    let mut todos: Vec<(String, String, String)> = Vec::new(); // (slug, entity_id, name)
    let mut calendars: BTreeMap<String, (String, String)> = BTreeMap::new(); // slug -> (entity_id, name)

    for state in &states {
        if let Some(slug) = state.entity_id.strip_prefix("todo.") {
            let name = friendly_name(state).unwrap_or_else(|| titlecase_slug(slug));
            todos.push((slug.to_string(), state.entity_id.clone(), name));
        } else if let Some(slug) = state.entity_id.strip_prefix("calendar.") {
            let name = friendly_name(state).unwrap_or_else(|| titlecase_slug(slug));
            calendars.insert(slug.to_string(), (state.entity_id.clone(), name));
        }
    }
    todos.sort_by(|a, b| a.0.cmp(&b.0)); // stable across runs

    let mut members = Vec::new();

    for (slug, todo_entity, name) in todos {
        let calendar_entities = calendars.remove(&slug).map(|(id, _)| vec![id]).unwrap_or_default();
        let color = PALETTE[members.len() % PALETTE.len()].to_string();
        members.push(FamilyMember {
            id: slug,
            name,
            color,
            todo_entity: Some(todo_entity),
            calendar_entities,
        });
    }

    // Calendars that didn't match any todo list's slug -- most commonly a
    // single shared household calendar -- become their own entries.
    let mut leftover_calendars: Vec<_> = calendars.into_iter().collect();
    leftover_calendars.sort_by(|a, b| a.0.cmp(&b.0));
    for (slug, (entity_id, name)) in leftover_calendars {
        let color = PALETTE[members.len() % PALETTE.len()].to_string();
        members.push(FamilyMember {
            id: slug,
            name,
            color,
            todo_entity: None,
            calendar_entities: vec![entity_id],
        });
    }

    members
}

/// "brielle_todo" -> "Brielle Todo" -- used when an entity has no
/// `friendly_name` attribute to fall back on.
fn titlecase_slug(slug: &str) -> String {
    titlecase_words(slug, '_')
}

/// Splits `s` on `sep` and capitalizes each word's first letter.
fn titlecase_words(s: &str, sep: char) -> String {
    s.split(sep)
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The first `weather.*` entity found in HA, if any -- used when
/// `config.weather_entity` isn't set. Most setups only ever have one
/// weather integration configured, so "first one found" needs no further
/// disambiguation; anyone who wants a specific one among several can set
/// `weather_entity` explicitly.
async fn discover_weather_entity(client: &Client) -> Option<String> {
    let states = match client.get_states().await {
        Ok(states) => states,
        Err(err) => {
            tracing::warn!(%err, "failed to list HA entities while looking for a weather entity");
            return None;
        }
    };
    states.into_iter().map(|s| s.entity_id).find(|id| id.starts_with("weather."))
}

/// Any other `weather.*` entity (besides `primary`) that has a `humidity`
/// or `pressure` attribute -- used when `config.weather_backfill_entity`
/// isn't set and `primary` itself doesn't expose those (some integrations,
/// confirmed against a real instance, only report condition/temperature/
/// wind, while HA's own default Met.no forecast entity has the rest).
async fn discover_weather_backfill_entity(client: &Client, primary: Option<&str>) -> Option<String> {
    let states = match client.get_states().await {
        Ok(states) => states,
        Err(err) => {
            tracing::warn!(%err, "failed to list HA entities while looking for a weather backfill entity");
            return None;
        }
    };
    states
        .into_iter()
        .find(|s| {
            s.entity_id.starts_with("weather.")
                && Some(s.entity_id.as_str()) != primary
                && (s.attributes.get("humidity").is_some() || s.attributes.get("pressure").is_some())
        })
        .map(|s| s.entity_id)
}

/// Dashboard sections from `sensor.skylight_dashboard_*` entities (Phase 2
/// -- github.com/siesta5787/skylight-family extended with a "Dashboard
/// Section" subentry type, same pattern as `sensor.skylight_family_*`).
/// Not built on that integration's side yet, so this always returns
/// `None` today; `config.dashboard` is the only working source until it
/// is. Expected attribute shape once it exists: `section_type` ("toggle_
/// group"/"climate"/"sensor_group"), `title`, `entities` (a list; for
/// `climate` just the one entry is used).
async fn discover_dashboard_sections(client: &Client) -> Option<Vec<DashboardSection>> {
    let states = match client.get_states().await {
        Ok(states) => states,
        Err(err) => {
            tracing::warn!(%err, "failed to list HA entities while checking for Skylight Dashboard sections");
            return None;
        }
    };

    let mut sections = Vec::new();
    for state in &states {
        if !state.entity_id.starts_with("sensor.skylight_dashboard_") {
            continue;
        }
        let section_type = state.attributes.get("section_type").and_then(|v| v.as_str());
        let title = state
            .attributes
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_default();
        let entities: Vec<String> = state
            .attributes
            .get("entities")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|e| e.as_str().map(String::from)).collect())
            .unwrap_or_default();

        match section_type {
            Some("toggle_group") => sections.push(DashboardSection::ToggleGroup { title, entities }),
            Some("sensor_group") => sections.push(DashboardSection::SensorGroup { title, entities }),
            Some("climate") => {
                if let Some(entity) = entities.into_iter().next() {
                    sections.push(DashboardSection::Climate { entity });
                }
            }
            _ => tracing::warn!(entity = %state.entity_id, ?section_type, "unrecognized dashboard section_type"),
        }
    }

    if sections.is_empty() { None } else { Some(sections) }
}

fn dashboard_section_contains(section: &DashboardSection, entity_id: &str) -> bool {
    match section {
        DashboardSection::ToggleGroup { entities, .. } | DashboardSection::SensorGroup { entities, .. } => {
            entities.iter().any(|e| e == entity_id)
        }
        DashboardSection::Climate { entity } => entity == entity_id,
    }
}

async fn fetch_calendar_events(
    rest: &RestClient,
    family: &[FamilyMember],
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Vec<(usize, slint::Color, Vec<CalendarEvent>)> {
    let mut out = Vec::new();
    for (index, member) in family.iter().enumerate() {
        if member.calendar_entities.is_empty() {
            continue;
        }
        // A member can have more than one calendar linked; events from all
        // of them are merged and shown in this member's single color --
        // nothing downstream needs to know which specific calendar an
        // event came from.
        let mut events = Vec::new();
        for entity in &member.calendar_entities {
            match rest.calendar_events(entity, start, end).await {
                Ok(fetched) => events.extend(fetched),
                Err(err) => {
                    tracing::warn!(entity = %entity, %err, "failed to fetch calendar events");
                }
            }
        }
        out.push((index, parse_hex_color(&member.color), events));
    }
    out
}

/// Also reports whether the connection still looks alive: `ha-client`
/// deliberately doesn't hide reconnection behind `Client` itself (see its
/// own doc comment) -- once the underlying WS actor dies, every call on
/// this `Client` returns `Error::Closed` forever, which without this check
/// looked identical to "no items" (logged, then silently swapped in an
/// empty list) instead of "we need to reconnect". That's what was making
/// the Tasks page's columns go empty after a few minutes: the fetch wasn't
/// slow or wrong, the connection had quietly died and nothing ever asked
/// for a fresh one.
async fn fetch_todos(client: &Client, family: &[FamilyMember]) -> (Vec<Vec<TodoItem>>, bool) {
    let mut out = Vec::with_capacity(family.len());
    let mut connection_alive = true;
    for member in family {
        let items = match &member.todo_entity {
            Some(entity) => match client.todo_items(entity).await {
                Ok(items) => items,
                Err(err) => {
                    if matches!(err, ha_client::connection::Error::Closed) {
                        connection_alive = false;
                    }
                    tracing::warn!(entity = %entity, %err, "failed to fetch todo items");
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        out.push(items);
    }
    (out, connection_alive)
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

/// Sunday of the week containing `reference_date`'s 1st, through the
/// Saturday of the week containing that month's last day -- the padded
/// 6-week/42-day range the month grid needs, now also doubling as the HA
/// fetch window.
fn month_grid_range(reference_date: Date) -> (Date, Date) {
    let first_of_month = reference_date.replace_day(1).expect("day 1 is always valid");
    let lead_days = first_of_month.weekday().number_days_from_sunday();
    let grid_start = first_of_month - TimeDuration::days(lead_days as i64);
    (grid_start, grid_start + TimeDuration::days(42))
}

/// Adds (or subtracts) whole months, clamping the day-of-month into the
/// target month's actual length (e.g. Jan 31 + 1 month -> Feb 28/29).
fn add_months(date: Date, delta: i32) -> Date {
    let total_months = date.year() * 12 + (date.month() as i32 - 1) + delta;
    let year = total_months.div_euclid(12);
    let month = Month::try_from((total_months.rem_euclid(12) + 1) as u8).expect("0..12 -> valid month");
    let last_day = month.length(year);
    Date::from_calendar_date(year, month, date.day().min(last_day)).expect("clamped day is valid")
}

fn month_label_for(date: Date) -> String {
    format!("{} {}", date.month(), date.year())
}

/// A single family member's contribution to a not-yet-merged event, before
/// events created for multiple people (one HA calendar event per selected
/// member -- see on_event_create_confirmed) are collapsed into one card.
#[derive(Clone)]
struct RawTimed {
    summary: String,
    time_label: String,
    start_minutes: i32,
    duration_minutes: i32,
    member_index: i32,
    color: slint::Color,
}

#[derive(Clone)]
struct RawAllDay {
    summary: String,
    member_index: i32,
    color: slint::Color,
}

#[derive(Default, Clone)]
struct RawDayBucket {
    timed: Vec<RawTimed>,
    all_day: Vec<RawAllDay>,
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

/// Comma-joined display name(s) for a merged event's participants, and the
/// `member-index` sentinel that goes with it (-1 once there's more than
/// one, so a shared event isn't hidden by toggling just one participant off
/// in the top bar -- see the field doc on CalendarEventDot).
fn member_label_and_index(family: &[FamilyMember], indexes: &[i32]) -> (String, i32) {
    let label = indexes
        .iter()
        .filter_map(|&i| family.get(i as usize))
        .map(|m| m.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let index = if indexes.len() == 1 { indexes[0] } else { -1 };
    (label, index)
}

/// Builds the Month/Week/Day/Agenda projections from each member's fetched
/// events (`per_member_events` is empty on first paint, before HA has
/// responded -- still produces correctly-shaped, just event-less, grids).
/// `reference_date` is whichever date Month/Week/Day are currently centered
/// on (via nav); `is_today`/`in_current_month` still compare against the
/// real wall-clock date, computed separately below.
fn build_calendar_grids(
    local_offset: UtcOffset,
    reference_date: Date,
    family: &[FamilyMember],
    per_member_events: &[(usize, slint::Color, Vec<CalendarEvent>)],
) -> CalendarGrids {
    let real_today = OffsetDateTime::now_utc().to_offset(local_offset).date();
    let (grid_start, grid_end) = month_grid_range(reference_date);
    let date_fmt = time::macros::format_description!("[year]-[month]-[day]");

    // Pass 1: collect every member's events per day, unmerged.
    let mut raw_buckets: BTreeMap<Date, RawDayBucket> = BTreeMap::new();

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
                // e.g. malformed) events still render as a visible, tappable
                // block -- 20min (~21px at the default row height) turned
                // out to be a hard target to hit precisely on a touchscreen.
                let duration_minutes = ((local_end - local_start).whole_minutes() as i32).max(30);
                let time_label = format_time_range(local_start, local_end);

                raw_buckets.entry(local_start.date()).or_default().timed.push(RawTimed {
                    summary: ev.summary.clone(),
                    time_label,
                    start_minutes,
                    duration_minutes,
                    member_index,
                    color: *color,
                });
            } else if let Some(date_str) = ev.start.date.as_deref() {
                if let Ok(date) = Date::parse(date_str, &date_fmt) {
                    raw_buckets.entry(date).or_default().all_day.push(RawAllDay {
                        summary: ev.summary.clone(),
                        member_index,
                        color: *color,
                    });
                }
            }
        }
    }

    // Pass 2: merge events that were created for multiple family members at
    // once -- one HA calendar event per selected member (see
    // on_event_create_confirmed), so they arrive back from HA as separate
    // events sharing the same summary/start/duration. Matched on those three
    // fields per day; O(n^2) but both n (events/day) and family size are
    // small. `per_member_events`'s outer loop above is already in family
    // order, so each group's colors/names come out in that order too.
    let mut buckets: BTreeMap<Date, DayBucket> = BTreeMap::new();
    for (date, raw) in raw_buckets {
        let bucket = buckets.entry(date).or_default();

        let mut timed_groups: Vec<(RawTimed, Vec<i32>, Vec<slint::Color>)> = Vec::new();
        for t in raw.timed {
            if let Some(group) = timed_groups.iter_mut().find(|(g, _, _)| {
                g.summary == t.summary
                    && g.start_minutes == t.start_minutes
                    && g.duration_minutes == t.duration_minutes
            }) {
                group.1.push(t.member_index);
                group.2.push(t.color);
            } else {
                let indexes = vec![t.member_index];
                let colors = vec![t.color];
                timed_groups.push((t, indexes, colors));
            }
        }
        for (t, indexes, colors) in timed_groups {
            let (member_label, member_index) = member_label_and_index(family, &indexes);
            bucket.month_entries.push(CalendarEventDot {
                summary: t.summary.clone().into(),
                time_label: t.time_label.clone().into(),
                member_colors: slint::ModelRc::new(slint::VecModel::from(colors.clone())),
                member_label: member_label.clone().into(),
                member_index,
            });
            bucket.events.push((
                t.start_minutes,
                WeekEventData {
                    summary: t.summary.into(),
                    time_label: t.time_label.into(),
                    start_minutes: t.start_minutes,
                    duration_minutes: t.duration_minutes,
                    member_colors: slint::ModelRc::new(slint::VecModel::from(colors)),
                    member_label: member_label.into(),
                    member_index,
                },
            ));
        }

        let mut all_day_groups: Vec<(String, Vec<i32>, Vec<slint::Color>)> = Vec::new();
        for a in raw.all_day {
            if let Some(group) = all_day_groups.iter_mut().find(|(s, _, _)| *s == a.summary) {
                group.1.push(a.member_index);
                group.2.push(a.color);
            } else {
                all_day_groups.push((a.summary, vec![a.member_index], vec![a.color]));
            }
        }
        for (summary, indexes, colors) in all_day_groups {
            let (member_label, member_index) = member_label_and_index(family, &indexes);
            bucket.month_entries.push(CalendarEventDot {
                summary: summary.clone().into(),
                time_label: "All day".into(),
                member_colors: slint::ModelRc::new(slint::VecModel::from(colors.clone())),
                member_label: member_label.clone().into(),
                member_index,
            });
            bucket.banners.push(AllDayBannerData {
                text: summary.into(),
                member_colors: slint::ModelRc::new(slint::VecModel::from(colors)),
                member_index,
            });
        }
    }

    // Month: every day in the padded 6-week grid around `reference_date`.
    let mut month_weeks = Vec::with_capacity(6);
    let mut cursor = grid_start;
    for _ in 0..6 {
        let mut week = Vec::with_capacity(7);
        for _ in 0..7 {
            let dots: Vec<CalendarEventDot> =
                buckets.get(&cursor).map(|b| b.month_entries.clone()).unwrap_or_default();
            week.push(CalendarDayData {
                day_number: cursor.day() as i32,
                in_current_month: cursor.month() == reference_date.month(),
                is_today: cursor == real_today,
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
            is_today: date == real_today,
            all_day_banners: slint::ModelRc::new(slint::VecModel::from(bucket.banners)),
            events: slint::ModelRc::new(slint::VecModel::from(events)),
        }
    };

    // Week/Day: around `reference_date`, so navigating either one moves the
    // shared cursor Month also uses.
    let week_start = reference_date
        - TimeDuration::days(reference_date.weekday().number_days_from_sunday() as i64);
    let week_columns: Vec<WeekDayColumnData> =
        (0..7).map(|i| to_column(week_start + TimeDuration::days(i))).collect();
    let day_columns = vec![to_column(reference_date)];

    // Agenda always looks forward from the real "now" regardless of
    // Month/Week/Day navigation -- it has no nav controls of its own, so
    // there'd be no way to get back to "upcoming" if navigating elsewhere
    // also dragged Agenda along. Bounded by the currently-fetched window,
    // so it can go empty if you've navigated Month far from the present.
    let agenda_columns: Vec<WeekDayColumnData> = buckets
        .range(real_today..grid_end)
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

/// Builds the Tasks page's columns (only members with a `todo_entity` get
/// one -- a calendar-only entry, e.g. a shared household calendar, has
/// nothing to show there), the uid map that goes with those columns in the
/// same order (so the toggle callback can resolve which item was tapped),
/// and the top-bar chips (every member, calendar-only ones included, since
/// they're still toggleable for calendar visibility).
fn build_todo_model(
    family: &[FamilyMember],
    per_member_items: &[Vec<TodoItem>],
) -> (slint::ModelRc<TodoColumnData>, Vec<(String, Vec<String>)>, Vec<MemberChipData>) {
    let mut columns = Vec::new();
    let mut uid_map = Vec::new();
    let mut chips = Vec::with_capacity(family.len());

    for (member, items) in family.iter().zip(per_member_items) {
        let color = parse_hex_color(&member.color);
        let completed = items.iter().filter(|i| i.status == TodoStatus::Completed).count();

        chips.push(MemberChipData {
            name: member.name.clone().into(),
            color,
            completed: completed as i32,
            total: items.len() as i32,
        });

        let Some(todo_entity) = &member.todo_entity else { continue };

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
        uid_map.push((todo_entity.clone(), items.iter().map(|i| i.uid.clone()).collect()));
    }

    (slint::ModelRc::new(slint::VecModel::from(columns)), uid_map, chips)
}

/// Builds the Dashboard page's cards from the configured sections and a
/// snapshot of every HA entity's state (see `refresh_calendar_and_todos`).
/// A section referencing an entity that isn't in `states` (not fetched
/// yet, or deleted from HA) just skips that one row/card rather than
/// showing something broken -- `SensorGroup`/`ToggleGroup` skip the
/// missing entity and keep the rest; `Climate` (a single entity) skips
/// the whole card if its one entity is missing.
fn build_dashboard_cards(
    sections: &[DashboardSection],
    states: &std::collections::HashMap<&str, &EntityState>,
) -> Vec<DashboardCardData> {
    let empty_toggle_entities = || slint::ModelRc::new(slint::VecModel::from(Vec::<ToggleEntityData>::new()));
    let empty_strings = || slint::ModelRc::new(slint::VecModel::from(Vec::<SharedString>::new()));
    let empty_sensor_rows = || slint::ModelRc::new(slint::VecModel::from(Vec::<SensorRowData>::new()));

    sections
        .iter()
        .filter_map(|section| match section {
            DashboardSection::ToggleGroup { title, entities } => {
                let toggle_entities: Vec<ToggleEntityData> = entities
                    .iter()
                    .filter_map(|id| {
                        let state = *states.get(id.as_str())?;
                        Some(ToggleEntityData {
                            entity_id: id.clone().into(),
                            name: entity_friendly_name(state).into(),
                            domain: entity_domain(id).into(),
                            is_on: state.state == "on",
                        })
                    })
                    .collect();
                let group_is_on = toggle_entities.iter().any(|e| e.is_on);
                let group_entity_ids: Vec<SharedString> =
                    toggle_entities.iter().map(|e| e.entity_id.clone()).collect();
                Some(DashboardCardData {
                    kind: "toggle_group".into(),
                    title: title.clone().into(),
                    toggle_entities: slint::ModelRc::new(slint::VecModel::from(toggle_entities)),
                    group_is_on,
                    group_entity_ids: slint::ModelRc::new(slint::VecModel::from(group_entity_ids)),
                    climate_entity_id: SharedString::default(),
                    climate_current: SharedString::default(),
                    climate_target: SharedString::default(),
                    climate_mode: SharedString::default(),
                    climate_modes: empty_strings(),
                    sensor_rows: empty_sensor_rows(),
                })
            }
            DashboardSection::Climate { entity } => {
                let state = *states.get(entity.as_str())?;
                let current = state.attributes.get("current_temperature").and_then(|v| v.as_f64());
                let target = state.attributes.get("temperature").and_then(|v| v.as_f64());
                let hvac_mode = state.state.clone();
                let hvac_modes: Vec<SharedString> = state
                    .attributes
                    .get("hvac_modes")
                    .and_then(|v| v.as_array())
                    .map(|modes| modes.iter().filter_map(|m| m.as_str()).map(SharedString::from).collect())
                    .unwrap_or_default();
                let current_label = match current {
                    Some(c) => format!("{} · {}", capitalize_first(&hvac_mode), format_climate_temp(c)),
                    None => capitalize_first(&hvac_mode),
                };
                Some(DashboardCardData {
                    kind: "climate".into(),
                    title: entity_friendly_name(state).into(),
                    toggle_entities: empty_toggle_entities(),
                    group_is_on: false,
                    group_entity_ids: empty_strings(),
                    climate_entity_id: entity.clone().into(),
                    climate_current: current_label.into(),
                    climate_target: target.map(format_climate_temp).unwrap_or_default().into(),
                    climate_mode: hvac_mode.into(),
                    climate_modes: slint::ModelRc::new(slint::VecModel::from(hvac_modes)),
                    sensor_rows: empty_sensor_rows(),
                })
            }
            DashboardSection::SensorGroup { title, entities } => {
                let rows: Vec<SensorRowData> = entities
                    .iter()
                    .filter_map(|id| {
                        let state = *states.get(id.as_str())?;
                        let unit =
                            state.attributes.get("unit_of_measurement").and_then(|v| v.as_str()).unwrap_or("");
                        let device_class =
                            state.attributes.get("device_class").and_then(|v| v.as_str()).unwrap_or("");
                        Some(SensorRowData {
                            label: entity_friendly_name(state).into(),
                            value: format_sensor_value(&state.state, unit, device_class).into(),
                            device_class: device_class.into(),
                        })
                    })
                    .collect();
                Some(DashboardCardData {
                    kind: "sensor_group".into(),
                    title: title.clone().into(),
                    toggle_entities: empty_toggle_entities(),
                    group_is_on: false,
                    group_entity_ids: empty_strings(),
                    climate_entity_id: SharedString::default(),
                    climate_current: SharedString::default(),
                    climate_target: SharedString::default(),
                    climate_mode: SharedString::default(),
                    climate_modes: empty_strings(),
                    sensor_rows: slint::ModelRc::new(slint::VecModel::from(rows)),
                })
            }
        })
        .collect()
}

/// Groups consecutive same-`kind` cards into a shared row -- e.g. a config
/// with Lights, Downstairs, Upstairs, Fans, Sunroom AC, Garage Freezer (in
/// that order) produces rows [Lights], [Downstairs, Upstairs], [Fans],
/// [Sunroom AC], [Garage Freezer] rather than one card per row throughout.
/// Reordering `config.dashboard` so same-kind sections sit next to each
/// other is how a user controls which cards end up sharing a row -- e.g.
/// Lights immediately followed by Fans puts them side by side.
fn group_dashboard_rows(cards: Vec<DashboardCardData>) -> Vec<DashboardRowData> {
    let mut rows: Vec<DashboardRowData> = Vec::new();
    let mut current: Vec<DashboardCardData> = Vec::new();
    let mut current_kind: Option<SharedString> = None;

    for card in cards {
        if current_kind.as_ref() != Some(&card.kind) && !current.is_empty() {
            rows.push(DashboardRowData {
                cards: slint::ModelRc::new(slint::VecModel::from(std::mem::take(&mut current))),
            });
        }
        current_kind = Some(card.kind.clone());
        current.push(card);
    }
    if !current.is_empty() {
        rows.push(DashboardRowData { cards: slint::ModelRc::new(slint::VecModel::from(current)) });
    }

    rows
}

fn entity_domain(entity_id: &str) -> &str {
    entity_id.split('.').next().unwrap_or("")
}

/// HA's own `friendly_name` attribute if set, else a titlecased version of
/// the entity_id's own name part (e.g. "garage_freezer_temperature" ->
/// "Garage Freezer Temperature") -- same fallback shape as
/// `discover_family`'s local `friendly_name` helper, just usable from here
/// too.
fn entity_friendly_name(state: &EntityState) -> String {
    match state.attributes.get("friendly_name").and_then(|v| v.as_str()) {
        Some(name) => name.to_string(),
        None => {
            let slug = state.entity_id.split('.').nth(1).unwrap_or(&state.entity_id);
            titlecase_slug(slug)
        }
    }
}

/// "75°" -- no unit letter, same reasoning as the weather widget's
/// compact temp-range: climate entities don't carry their own
/// `temperature_unit` attribute (the unit is HA's system-wide setting,
/// implicit), and this always sits next to a mode label that gives it
/// context.
fn format_climate_temp(value: f64) -> String {
    format!("{}°", value.round() as i64)
}

/// Sensor rows format to 1 decimal place normally (matches a real
/// instance's "-15.88" -> "-15.9 °F"), but whole numbers for battery/
/// signal-strength (matches "100%"/"−69 dBm", not "100.0%"). No space
/// before a bare "%" unit; a space otherwise. Falls back to the raw state
/// string unchanged if it isn't numeric (defensive -- every sensor this
/// app targets has a numeric state, but a malformed one shouldn't panic).
fn format_sensor_value(state: &str, unit: &str, device_class: &str) -> String {
    let Ok(value) = state.parse::<f64>() else {
        return if unit.is_empty() { state.to_string() } else { format!("{state} {unit}") };
    };
    let formatted = match device_class {
        "battery" | "signal_strength" => format!("{}", value.round() as i64),
        _ => format!("{value:.1}"),
    };
    if unit.is_empty() || unit == "%" {
        format!("{formatted}{unit}")
    } else {
        format!("{formatted} {unit}")
    }
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

/// "75 °F" -- HA's `temperature_unit` attribute already includes the
/// degree sign ("°C"/"°F"), confirmed against a real instance, so this
/// doesn't add its own; the space before it matches the weather widget's
/// styling (see the reference screenshot this widget was built from).
fn format_weather_temp(value: f64, unit: &str) -> String {
    format!("{} {unit}", value.round() as i64)
}

/// "91°/75°" -- today's forecast high/low. No unit letter on either side
/// (unlike `format_weather_temp`) since this always sits directly under
/// the primary temperature reading, which already has one.
fn format_temp_range(high: f64, low: f64) -> String {
    format!("{}°/{}°", high.round() as i64, low.round() as i64)
}

/// "clear-night" -> "Clear, night", "sunny" -> "Sunny" -- HA's weather
/// `state` is one of a fixed set of dash-separated lowercase condition
/// slugs; this mirrors the wording HA's own more-info dialog uses (first
/// word capitalized, dash becomes a comma) rather than a lookup table, so
/// it has no translation and multi-word slugs without a dash ("partly-
/// cloudy" doesn't exist, but e.g. "partlycloudy" does) stay one word.
fn format_weather_condition_label(condition: &str) -> String {
    match condition.split_once('-') {
        Some((first, rest)) => format!("{}, {rest}", capitalize_first(first)),
        None => capitalize_first(condition),
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// "17 minutes ago" / "2 hours ago" / "Just now" -- how long ago a weather
/// entity's state last changed. Deliberately coarse (minutes, then hours --
/// no days) since weather data this stale would be a fetch problem worth
/// noticing on its own, not something to label precisely.
fn format_relative_time(then: OffsetDateTime, now: OffsetDateTime) -> String {
    let elapsed_minutes = (now - then).whole_minutes();
    if elapsed_minutes < 1 {
        "Just now".to_string()
    } else if elapsed_minutes < 60 {
        format!("{elapsed_minutes} minute{} ago", if elapsed_minutes == 1 { "" } else { "s" })
    } else {
        let hours = elapsed_minutes / 60;
        format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" })
    }
}

/// 16-point compass label from a wind bearing in degrees (0 = north,
/// clockwise) -- HA reports wind direction as a raw bearing, not a label.
fn compass_direction(bearing_degrees: f64) -> &'static str {
    const DIRECTIONS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let normalized = bearing_degrees.rem_euclid(360.0);
    let index = ((normalized / 22.5) + 0.5) as usize % 16;
    DIRECTIONS[index]
}

/// "2.92 mph (WNW)" -- wind speed alone (no bearing available) if that's
/// all the entity has; empty if it has neither.
fn format_wind(state: &EntityState) -> String {
    let speed = state.attributes.get("wind_speed").and_then(|v| v.as_f64());
    let unit = state.attributes.get("wind_speed_unit").and_then(|v| v.as_str()).unwrap_or("");
    let bearing = state.attributes.get("wind_bearing").and_then(|v| v.as_f64());
    match (speed, bearing) {
        (Some(speed), Some(bearing)) => format!("{speed:.2} {unit} ({})", compass_direction(bearing)),
        (Some(speed), None) => format!("{speed:.2} {unit}"),
        (None, _) => String::new(),
    }
}

fn format_pressure(value: f64, unit: &str) -> String {
    format!("{value:.2} {unit}")
}

fn format_humidity(value: f64) -> String {
    format!("{}%", value.round() as i64)
}

/// "0.19 in" -- today's forecast rain amount, not a percentage (see the
/// doc comment on `DailyForecast::precipitation` for why there's no
/// percentage-chance field to show instead).
fn format_precip(value: f64, unit: &str) -> String {
    format!("{value:.2} {unit}")
}

/// Reads `key` from the first of `sources` (in order) that has it -- used
/// to check the primary weather entity before falling back to the backfill
/// one for humidity/pressure (see `apply_weather`).
fn weather_numeric_attr(sources: &[Option<&EntityState>], key: &str) -> Option<f64> {
    sources.iter().flatten().find_map(|s| s.attributes.get(key).and_then(|v| v.as_f64()))
}

fn weather_string_attr<'a>(sources: &[Option<&'a EntityState>], key: &str) -> Option<&'a str> {
    sources.iter().flatten().find_map(|s| s.attributes.get(key).and_then(|v| v.as_str()))
}

fn parse_hex_color(hex: &str) -> slint::Color {
    let hex = hex.trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16).unwrap_or(0x6c8dfa);
    let [_, r, g, b] = value.to_be_bytes();
    slint::Color::from_rgb_u8(r, g, b)
}

/// A family member's `color` attribute from the Skylight Family integration
/// (or, in principle, anything else supplying one). HA's color selector --
/// what the integration's config flow uses -- stores this as an `[r, g, b]`
/// array of 0-255 ints, not a hex string; confirmed against a real
/// configured member, where it was silently falling back to the palette
/// default before this handled the array form. A plain hex string is also
/// accepted, in case that ever changes.
fn parse_ha_color_attribute(value: Option<&serde_json::Value>) -> Option<String> {
    let value = value?;
    if let Some(s) = value.as_str().filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    let rgb = value.as_array()?;
    let mut channels = rgb.iter().filter_map(|n| n.as_u64());
    match (channels.next(), channels.next(), channels.next()) {
        (Some(r), Some(g), Some(b)) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weather_state(state: &str, attributes: serde_json::Value) -> EntityState {
        EntityState { entity_id: "weather.home".into(), state: state.into(), attributes, last_updated: None }
    }

    #[test]
    fn formats_weather_temperature() {
        assert_eq!(format_weather_temp(77.0, "°F"), "77 °F");
    }

    #[test]
    fn formats_temp_range() {
        assert_eq!(format_temp_range(91.4, 75.2), "91°/75°");
    }

    #[test]
    fn formats_precip() {
        assert_eq!(format_precip(0.19, "in"), "0.19 in");
        assert_eq!(format_precip(0.0, "in"), "0.00 in");
    }

    #[test]
    fn formats_condition_labels_like_ha_more_info() {
        assert_eq!(format_weather_condition_label("sunny"), "Sunny");
        assert_eq!(format_weather_condition_label("clear-night"), "Clear, night");
        assert_eq!(format_weather_condition_label("partlycloudy"), "Partlycloudy");
    }

    #[test]
    fn formats_relative_time() {
        let now = OffsetDateTime::now_utc();
        assert_eq!(format_relative_time(now, now), "Just now");
        assert_eq!(format_relative_time(now - TimeDuration::minutes(17), now), "17 minutes ago");
        assert_eq!(format_relative_time(now - TimeDuration::minutes(1), now), "1 minute ago");
        assert_eq!(format_relative_time(now - TimeDuration::hours(2), now), "2 hours ago");
    }

    #[test]
    fn resolves_compass_directions() {
        assert_eq!(compass_direction(0.0), "N");
        assert_eq!(compass_direction(288.7), "WNW");
        assert_eq!(compass_direction(359.9), "N");
    }

    #[test]
    fn formats_wind_with_and_without_bearing() {
        // The exact shape returned by a real weather.* entity's
        // GET /api/states/{entity_id}, confirmed against a live instance.
        let state = weather_state(
            "sunny",
            serde_json::json!({ "wind_speed": 2.92, "wind_speed_unit": "mph", "wind_bearing": 288.7 }),
        );
        assert_eq!(format_wind(&state), "2.92 mph (WNW)");

        let no_bearing = weather_state("sunny", serde_json::json!({ "wind_speed": 5.0, "wind_speed_unit": "mph" }));
        assert_eq!(format_wind(&no_bearing), "5.00 mph");

        let no_wind = weather_state("sunny", serde_json::json!({}));
        assert_eq!(format_wind(&no_wind), "");
    }

    #[test]
    fn backfills_humidity_and_pressure_from_a_second_entity_only_when_primary_lacks_them() {
        let primary = weather_state("sunny", serde_json::json!({ "temperature": 77 }));
        let backfill =
            weather_state("clear-night", serde_json::json!({ "humidity": 95, "pressure": 30.01, "pressure_unit": "inHg" }));
        let sources = [Some(&primary), Some(&backfill)];
        assert_eq!(weather_numeric_attr(&sources, "humidity"), Some(95.0));
        assert_eq!(weather_numeric_attr(&sources, "pressure"), Some(30.01));
        assert_eq!(weather_string_attr(&sources, "pressure_unit"), Some("inHg"));

        // Primary's own value wins when it has one -- no need to touch the
        // backfill entity's data at all in that case.
        let primary_with_humidity = weather_state("sunny", serde_json::json!({ "humidity": 40 }));
        let sources = [Some(&primary_with_humidity), Some(&backfill)];
        assert_eq!(weather_numeric_attr(&sources, "humidity"), Some(40.0));
    }

    #[test]
    fn parses_rgb_array_color_attribute() {
        // The exact shape HA's color selector actually sends, confirmed
        // against a real Skylight Family member entity.
        let value = serde_json::json!([0, 255, 0]);
        assert_eq!(parse_ha_color_attribute(Some(&value)), Some("#00ff00".to_string()));
    }

    #[test]
    fn parses_hex_string_color_attribute() {
        let value = serde_json::json!("#4f8ef7");
        assert_eq!(parse_ha_color_attribute(Some(&value)), Some("#4f8ef7".to_string()));
    }

    #[test]
    fn rejects_missing_or_malformed_color_attribute() {
        assert_eq!(parse_ha_color_attribute(None), None);
        assert_eq!(parse_ha_color_attribute(Some(&serde_json::json!(""))), None);
        assert_eq!(parse_ha_color_attribute(Some(&serde_json::json!([255, 0]))), None);
        assert_eq!(parse_ha_color_attribute(Some(&serde_json::json!(null))), None);
    }

    fn member(id: &str, name: &str) -> FamilyMember {
        FamilyMember {
            id: id.into(),
            name: name.into(),
            color: "#4f8ef7".into(),
            todo_entity: None,
            calendar_entities: vec![format!("calendar.{id}")],
        }
    }

    fn timed_event(summary: &str, start: &str, end: &str) -> CalendarEvent {
        let dt = |s: &str| OffsetDateTime::parse(s, &time::format_description::well_known::Iso8601::DEFAULT).unwrap();
        CalendarEvent {
            summary: summary.into(),
            start: ha_client::entities::CalendarDateTime { date_time: Some(dt(start)), date: None },
            end: ha_client::entities::CalendarDateTime { date_time: Some(dt(end)), date: None },
            description: None,
            location: None,
        }
    }

    // The bug this guards against: creating one event for multiple family
    // members makes one real HA calendar event per person (see
    // on_event_create_confirmed), so they come back from HA as separate
    // same-summary/same-time events -- these must collapse into a single
    // card with every participant's color, not render as fully-overlapping
    // duplicates (which is invisible in the time-grid views, since they
    // position events absolutely by time).
    #[test]
    fn merges_events_created_for_multiple_members() {
        let family = vec![member("mom", "Mom"), member("dad", "Dad")];
        let per_member_events = vec![
            (
                0,
                slint::Color::from_rgb_u8(255, 0, 0),
                vec![timed_event("Dog's Bath", "2026-09-17T11:00:00+00:00", "2026-09-17T12:00:00+00:00")],
            ),
            (
                1,
                slint::Color::from_rgb_u8(0, 0, 255),
                vec![timed_event("Dog's Bath", "2026-09-17T11:00:00+00:00", "2026-09-17T12:00:00+00:00")],
            ),
        ];
        let reference_date = Date::from_calendar_date(2026, Month::September, 17).unwrap();

        let grids = build_calendar_grids(UtcOffset::UTC, reference_date, &family, &per_member_events);

        let day = grids.day.row_data(0).unwrap();
        assert_eq!(day.events.row_count(), 1, "expected one merged event, not two overlapping ones");
        let merged = day.events.row_data(0).unwrap();
        assert_eq!(merged.member_colors.row_count(), 2);
        assert_eq!(merged.member_label, "Mom, Dad");
        assert_eq!(merged.member_index, -1, "shared events must stay visible regardless of per-member toggling");
    }

    #[test]
    fn keeps_solo_events_separate_and_indexed() {
        let family = vec![member("mom", "Mom"), member("dad", "Dad")];
        let per_member_events = vec![
            (
                0,
                slint::Color::from_rgb_u8(255, 0, 0),
                vec![timed_event("Mom's Coffee", "2026-09-17T09:00:00+00:00", "2026-09-17T10:00:00+00:00")],
            ),
            (
                1,
                slint::Color::from_rgb_u8(0, 0, 255),
                vec![timed_event("Dad's Gym", "2026-09-17T09:00:00+00:00", "2026-09-17T10:00:00+00:00")],
            ),
        ];
        let reference_date = Date::from_calendar_date(2026, Month::September, 17).unwrap();

        let grids = build_calendar_grids(UtcOffset::UTC, reference_date, &family, &per_member_events);

        let day = grids.day.row_data(0).unwrap();
        assert_eq!(day.events.row_count(), 2, "different summaries must not merge");
        for i in 0..2 {
            let ev = day.events.row_data(i).unwrap();
            assert_eq!(ev.member_colors.row_count(), 1);
            assert_eq!(ev.member_index, i as i32);
        }
    }

    fn entity_state(entity_id: &str, state: &str, attributes: serde_json::Value) -> EntityState {
        EntityState { entity_id: entity_id.into(), state: state.into(), attributes, last_updated: None }
    }

    #[test]
    fn resolves_entity_domain() {
        assert_eq!(entity_domain("light.family_room_fan_light"), "light");
        assert_eq!(entity_domain("climate.x2s_smart_thermostat"), "climate");
    }

    #[test]
    fn formats_climate_temp() {
        assert_eq!(format_climate_temp(75.4), "75°");
    }

    #[test]
    fn formats_sensor_values_by_device_class() {
        // Exact shapes confirmed against a real Garage Freezer thermometer
        // device (4 separate sensor.* entities).
        assert_eq!(format_sensor_value("-15.88", "°F", "temperature"), "-15.9 °F");
        assert_eq!(format_sensor_value("100", "%", "battery"), "100%");
        assert_eq!(format_sensor_value("-69", "dBm", "signal_strength"), "-69 dBm");
        assert_eq!(format_sensor_value("60.0", "%", "humidity"), "60.0%");
        // Non-numeric state (defensive -- shouldn't happen for these
        // sensors, but shouldn't panic either) falls back to raw text.
        assert_eq!(format_sensor_value("unavailable", "°F", "temperature"), "unavailable °F");
    }

    #[test]
    fn friendly_name_falls_back_to_titlecased_entity_id() {
        let named = entity_state("light.ava", "on", serde_json::json!({ "friendly_name": "Ava Light" }));
        assert_eq!(entity_friendly_name(&named), "Ava Light");

        let unnamed = entity_state("light.garage_side_door", "on", serde_json::json!({}));
        assert_eq!(entity_friendly_name(&unnamed), "Garage Side Door");
    }

    #[test]
    fn builds_toggle_group_card_with_group_on_state() {
        // family_room on (brightness color_mode), ava off (onoff color
        // mode) -- exact shapes confirmed against real light.* entities.
        let states = [
            entity_state(
                "light.family_room_fan_light",
                "on",
                serde_json::json!({ "friendly_name": "Family Room Light", "color_mode": "brightness" }),
            ),
            entity_state(
                "light.ava_fan_light",
                "off",
                serde_json::json!({ "friendly_name": "Ava Light", "color_mode": "onoff" }),
            ),
        ];
        let by_id: std::collections::HashMap<&str, &EntityState> =
            states.iter().map(|s| (s.entity_id.as_str(), s)).collect();
        let sections = vec![DashboardSection::ToggleGroup {
            title: "Lights".into(),
            entities: vec!["light.family_room_fan_light".into(), "light.ava_fan_light".into()],
        }];

        let cards = build_dashboard_cards(&sections, &by_id);
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.kind, "toggle_group");
        assert_eq!(card.title, "Lights");
        assert_eq!(card.toggle_entities.row_count(), 2);
        assert!(card.group_is_on, "one light on should mean the group switch shows on");
        let first = card.toggle_entities.row_data(0).unwrap();
        assert_eq!(first.name, "Family Room Light");
        assert_eq!(first.domain, "light");
        assert!(first.is_on);
        assert_eq!(card.group_entity_ids.row_count(), 2);
    }

    #[test]
    fn builds_climate_card_with_only_supported_modes() {
        // Exact shape confirmed against a real climate.* entity.
        let states = [entity_state(
            "climate.x2s_smart_thermostat",
            "cool",
            serde_json::json!({
                "friendly_name": "Upstairs Thermostat",
                "hvac_modes": ["off", "heat", "cool", "fan_only"],
                "current_temperature": 75,
                "temperature": 75,
                "min_temp": 50,
                "max_temp": 99,
            }),
        )];
        let by_id: std::collections::HashMap<&str, &EntityState> =
            states.iter().map(|s| (s.entity_id.as_str(), s)).collect();
        let sections = vec![DashboardSection::Climate { entity: "climate.x2s_smart_thermostat".into() }];

        let cards = build_dashboard_cards(&sections, &by_id);
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.kind, "climate");
        assert_eq!(card.title, "Upstairs Thermostat");
        assert_eq!(card.climate_current, "Cool · 75°");
        assert_eq!(card.climate_target, "75°");
        assert_eq!(card.climate_mode, "cool");
        assert_eq!(card.climate_modes.row_count(), 4);
    }

    #[test]
    fn builds_sensor_group_card() {
        let states = [
            entity_state(
                "sensor.garage_freezer_thermometer_temperature",
                "-15.88",
                serde_json::json!({
                    "friendly_name": "Garage Freezer thermometer Temperature",
                    "device_class": "temperature",
                    "unit_of_measurement": "°F",
                }),
            ),
            entity_state(
                "sensor.garage_freezer_thermometer_battery",
                "100",
                serde_json::json!({
                    "friendly_name": "Garage Freezer thermometer Battery",
                    "device_class": "battery",
                    "unit_of_measurement": "%",
                }),
            ),
        ];
        let by_id: std::collections::HashMap<&str, &EntityState> =
            states.iter().map(|s| (s.entity_id.as_str(), s)).collect();
        let sections = vec![DashboardSection::SensorGroup {
            title: "Garage Freezer thermometer".into(),
            entities: vec![
                "sensor.garage_freezer_thermometer_temperature".into(),
                "sensor.garage_freezer_thermometer_battery".into(),
                "sensor.garage_freezer_thermometer_missing".into(), // not in `states` -- must be skipped
            ],
        }];

        let cards = build_dashboard_cards(&sections, &by_id);
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.kind, "sensor_group");
        assert_eq!(card.sensor_rows.row_count(), 2, "the missing entity must be skipped, not shown broken");
        let temp_row = card.sensor_rows.row_data(0).unwrap();
        assert_eq!(temp_row.value, "-15.9 °F");
        assert_eq!(temp_row.device_class, "temperature");
    }

    #[test]
    fn skips_climate_card_entirely_when_its_entity_is_missing() {
        let by_id: std::collections::HashMap<&str, &EntityState> = std::collections::HashMap::new();
        let sections = vec![DashboardSection::Climate { entity: "climate.gone".into() }];
        assert!(build_dashboard_cards(&sections, &by_id).is_empty());
    }

    fn dummy_card(kind: &str) -> DashboardCardData {
        DashboardCardData {
            kind: kind.into(),
            title: kind.into(),
            toggle_entities: slint::ModelRc::new(slint::VecModel::from(Vec::<ToggleEntityData>::new())),
            group_is_on: false,
            group_entity_ids: slint::ModelRc::new(slint::VecModel::from(Vec::<SharedString>::new())),
            climate_entity_id: SharedString::default(),
            climate_current: SharedString::default(),
            climate_target: SharedString::default(),
            climate_mode: SharedString::default(),
            climate_modes: slint::ModelRc::new(slint::VecModel::from(Vec::<SharedString>::new())),
            sensor_rows: slint::ModelRc::new(slint::VecModel::from(Vec::<SensorRowData>::new())),
        }
    }

    #[test]
    fn groups_consecutive_same_kind_cards_into_shared_rows() {
        // Lights, Downstairs, Upstairs, Fans, Sunroom AC, Garage Freezer --
        // matches the real config.toml ordering this was built for.
        let cards = vec![
            dummy_card("toggle_group"), // Lights
            dummy_card("climate"),      // Downstairs
            dummy_card("climate"),      // Upstairs
            dummy_card("toggle_group"), // Fans
            dummy_card("climate"),      // Sunroom AC
            dummy_card("sensor_group"), // Garage Freezer
        ];

        let rows = group_dashboard_rows(cards);
        let row_sizes: Vec<usize> = rows.iter().map(|r| r.cards.row_count()).collect();
        assert_eq!(row_sizes, vec![1, 2, 1, 1, 1], "Lights alone, [Downstairs, Upstairs] together, Fans alone, Sunroom AC alone, Garage Freezer alone");
    }

    #[test]
    fn groups_adjacent_toggle_groups_into_one_row() {
        // The actual ask this was built for: reordering config so Lights
        // and Fans are adjacent puts them side by side.
        let cards = vec![dummy_card("toggle_group"), dummy_card("toggle_group")];
        let rows = group_dashboard_rows(cards);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cards.row_count(), 2);
    }

    #[test]
    fn dashboard_section_contains_matches_expected_entities() {
        let toggle = DashboardSection::ToggleGroup {
            title: "Lights".into(),
            entities: vec!["light.a".into(), "light.b".into()],
        };
        assert!(dashboard_section_contains(&toggle, "light.a"));
        assert!(!dashboard_section_contains(&toggle, "light.c"));

        let climate = DashboardSection::Climate { entity: "climate.upstairs".into() };
        assert!(dashboard_section_contains(&climate, "climate.upstairs"));
        assert!(!dashboard_section_contains(&climate, "climate.downstairs"));
    }
}
