mod music;
mod timezone;
mod wifi;
mod update;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use dashboard_config::{Config, DashboardSection, FamilyMember};
use ha_client::entities::{CalendarEvent, DailyForecast, EntityState, TodoItem, TodoStatus};
use ha_client::{Client, RestClient};
use slint::{ComponentHandle, Model, SharedString};
use time::{Date, Duration as TimeDuration, Month, OffsetDateTime, UtcOffset, Weekday};
use ui::{
    AllDayBannerData, AppWindow, CalendarDayData, CalendarEventDot, DashboardCardData,
    DashboardRowData, EventFormMember, MemberChipData, SensorRowData, TodoColumnData,
    TodoItemData, ToggleEntityData, WeekDayColumnData, WeekEventData, WifiNetworkData,
};

/// The local UTC offset in whole seconds east of UTC, re-derived once per
/// second by the clock tick in `main` (see [`refresh_local_offset`]) and read
/// by everything that needs to convert between UTC and wall-clock time via
/// [`local_offset`].
///
/// This is a mutable global rather than a value computed once in `main`
/// because the device has no RTC: it boots at the kernel epoch (1970) and a
/// *background* NTP sync (`/etc/init.d/S45ntp`) corrects the clock some
/// seconds or minutes later, racing this app's own startup. A single
/// startup-time lookup therefore routinely resolved to the wrong DST bucket
/// (EST instead of EDT, say) and then never got re-checked, so the on-screen
/// clock stayed an hour off indefinitely -- until the process restarted.
static LOCAL_OFFSET_SECONDS: AtomicI32 = AtomicI32::new(0);

/// The most recently derived local UTC offset. Defaults to UTC until
/// [`refresh_local_offset`] has succeeded once.
fn local_offset() -> UtcOffset {
    UtcOffset::from_whole_seconds(LOCAL_OFFSET_SECONDS.load(Ordering::Relaxed))
        .unwrap_or(UtcOffset::UTC)
}

/// Asks the C library what the local UTC offset is *at the given instant*.
///
/// Why `libc::localtime_r` and not `time::UtcOffset::current_local_offset()`:
/// the `time` crate documents its local-offset lookup as unsound to call once
/// the process is multithreaded, which is why the original code called it
/// exactly once at the very top of `main`, before the tokio runtime existed.
/// That restriction is specifically about `time`'s *own* implementation -- it
/// reads the `TZ` environment variable and `/etc/localtime` itself, and can
/// race a concurrent `setenv`/fork+exec. It is not a property of the
/// underlying platform call.
///
/// POSIX requires `localtime_r` to be thread-safe (it is the reentrant
/// variant, and both glibc and musl take an internal lock over their cached
/// TZ state), so it is sound to call from the UI thread with tokio workers
/// running -- which is exactly what re-deriving the offset after startup
/// requires. `tm_gmtoff` is a glibc/musl extension present on Linux and
/// exposed by the `libc` crate; it already accounts for DST at `at`, which is
/// the whole point: passing the *current* timestamp is what makes a 1970 ->
/// 2026 clock correction move us from the wrong DST bucket to the right one.
///
/// The one-time reading of the TZ database (`/etc/localtime`, or `TZ`) is
/// done lazily by the first `localtime_r` call, and both implementations hold
/// their own lock across it (glibc: `tzset_lock` in `__tz_convert`; musl:
/// `LOCK(lock)` in `do_tzset`), so even that is safe concurrently. `main`
/// nonetheless triggers it deliberately, before the tokio runtime is created,
/// so the initialisation happens while the process is single-threaded and
/// every later call is pure arithmetic over cached zone rules -- which is
/// also what makes calling this once per second from the clock tick cheap.
///
/// (`libc::tzset` is not an option: the `libc` crate only binds it on
/// Windows. It isn't needed given the above.)
fn system_utc_offset(at: OffsetDateTime) -> Option<UtcOffset> {
    let timestamp = at.unix_timestamp() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `timestamp` is a valid `time_t` and `tm` is a valid, writable,
    // correctly-sized `struct tm` that outlives the call. `localtime_r`
    // writes only through the pointers given (that is what distinguishes it
    // from `localtime`), and is documented thread-safe.
    let result = unsafe { libc::localtime_r(&timestamp, &mut tm) };
    if result.is_null() {
        return None;
    }
    // Offsets are bounded by +/-26h, so the i32 narrowing can't lose data.
    UtcOffset::from_whole_seconds(tm.tm_gmtoff as i32).ok()
}

/// The zone's current abbreviation (`EDT`, `GMT`, `AEST`), for showing in
/// Settings that daylight saving is being handled rather than ignored.
///
/// `tm_zone` points into libc's own static zone data, which stays valid until
/// the zone is re-read -- and this app never re-reads it in-process (changing
/// the zone restarts the app instead; see `timezone.rs`). The string is copied
/// out immediately regardless rather than being held onto.
fn system_timezone_abbreviation(at: OffsetDateTime) -> Option<String> {
    let timestamp = at.unix_timestamp() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: same contract as `system_utc_offset` above -- valid `time_t`,
    // valid writable `tm` outliving the call, and `localtime_r` is the
    // thread-safe reentrant variant.
    let result = unsafe { libc::localtime_r(&timestamp, &mut tm) };
    if result.is_null() || tm.tm_zone.is_null() {
        return None;
    }
    // SAFETY: non-null per the check above, and libc guarantees a
    // NUL-terminated static string here.
    let abbreviation = unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) };
    abbreviation.to_str().ok().filter(|s| !s.is_empty()).map(|s| s.to_string())
}

/// Re-derives the local UTC offset and publishes it to
/// [`LOCAL_OFFSET_SECONDS`]. Returns `None` (and warns, once per process) if
/// the platform couldn't answer, leaving the previous value in place.
fn refresh_local_offset() -> Option<UtcOffset> {
    match system_utc_offset(OffsetDateTime::now_utc()) {
        Some(offset) => {
            let seconds = offset.whole_seconds();
            let previous = LOCAL_OFFSET_SECONDS.swap(seconds, Ordering::Relaxed);
            if previous != seconds {
                tracing::info!(
                    previous_seconds = previous,
                    new_seconds = seconds,
                    "local UTC offset changed (clock corrected, or a DST transition)"
                );
            }
            Some(offset)
        }
        None => {
            // Once per process only: this is called every second by the
            // clock tick, and a permanent failure (no TZ database on the
            // target at all) would otherwise emit a warning per second
            // forever.
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    "localtime_r() could not resolve the local UTC offset -- falling back to UTC. \
                     The clock and calendar will be wrong by the local offset until this starts \
                     working. Check that /etc/localtime exists and points into /usr/share/zoneinfo."
                );
            }
            None
        }
    }
}

/// Whether the calendar's displayed date should follow the wall clock's
/// "today" changing, and to what.
///
/// Two situations produce a change: an ordinary midnight rollover, and the
/// device's clock being corrected by NTP from the kernel epoch to the real
/// date (it has no RTC, so every boot starts in 1970 -- which is why the grid
/// could sit on "January 1970" until someone manually tapped "Today").
///
/// The rule is deliberately narrow: only follow if the view was showing the
/// *old* today, i.e. the user hadn't navigated anywhere. Yanking the grid out
/// from under someone who had deliberately paged to next month would be worse
/// than the bug.
fn reference_date_after_today_changed(
    reference_date: Date,
    previous_today: Date,
    new_today: Date,
) -> Option<Date> {
    if previous_today == new_today {
        return None;
    }
    if reference_date == previous_today {
        Some(new_today)
    } else {
        None
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `--version` is answered before *anything* else -- before the tracing
    // subscriber, before reading config.toml, before any Slint/DRM
    // initialisation. That ordering is load-bearing, not tidiness: the
    // updater's install preflight (see `update::install`) runs the
    // freshly-downloaded binary with this flag while the currently-running app
    // still holds DRM master, and it must therefore never touch the display,
    // the input devices, or the config file. Getting exit code 0 and a
    // matching version line out of it is what proves the download is the right
    // architecture, links against a compatible libc and
    // libinput/libudev/libxkbcommon, and isn't truncated -- all before the
    // atomic swap happens.
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", update::version_line());
        return;
    }

    tracing_subscriber::fmt::init();

    // First offset lookup. Deliberately here, at the very top of `main`,
    // while the process is still single-threaded (the tokio runtime is
    // created much further down): this is the call that makes libc read the
    // TZ database, so getting it out of the way now means every later lookup
    // from the clock tick is pure arithmetic over cached zone rules. Unlike
    // the `time` crate's `UtcOffset::current_local_offset()` this replaces,
    // it is also *sound* to call again later -- see `system_utc_offset`,
    // which is the whole reason the offset can now self-correct after NTP
    // fixes the clock instead of being frozen for the life of the process.
    if refresh_local_offset().is_none() {
        tracing::warn!(
            "no local UTC offset available at startup, continuing in UTC -- the 1s clock tick \
             will keep retrying and pick it up if it becomes available"
        );
    }
    let today = OffsetDateTime::now_utc().to_offset(local_offset()).date();

    // First non-flag argument, so that `--version` (handled above) and any
    // future flag can't be mistaken for the config path.
    let config_path = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| "config.toml".into());
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
    let empty_grids = build_calendar_grids(local_offset(), today, &[], &[]);
    apply_calendar_grids(&app, empty_grids);
    let (empty_todos, _, empty_chips) =
        build_todo_model(&config.family, &vec![Vec::new(); config.family.len()]);
    app.set_todo_columns(empty_todos);
    app.set_members(slint::ModelRc::new(slint::VecModel::from(empty_chips)));
    apply_family_roster(&app, &config.family);

    // Parental PIN lock -- fully local, no HA/network involved, so it's
    // loaded synchronously right here rather than through run_ha_sync. "Is
    // a PIN configured" is just "does the hash file exist", not a
    // separately-tracked flag that could drift out of sync with it.
    let pin_hash_path: Rc<String> = Rc::new(config.pin_hash_path.clone());
    let pin_hash: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(load_pin_hash(&pin_hash_path)));
    app.set_pin_configured(pin_hash.borrow().is_some());

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

    // Declared here rather than beside the Wi-Fi callbacks further down
    // because the keyboard's Done handler needs them: a passphrase typed on
    // the on-screen keyboard is what completes a join.
    let wifi_settings = wifi::Settings::from_env();
    let wifi_networks: Arc<Mutex<Vec<wifi::Network>>> = Arc::new(Mutex::new(Vec::new()));
    let wifi_busy = Arc::new(AtomicBool::new(false));
    // The event-creation form's title, edited via the keyboard (a separate
    // modal on top of the form) -- reset to the default each time a new
    // slot/day/"+" is tapped, in `open_event_form`.
    let event_form_title: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    // Which step of a PIN setup/change/disable/unlock flow is in progress
    // (see PinFlow), and the digits typed so far for it -- same "Rust owns
    // the buffer" reasoning as `keyboard_buffer` (backspace needs to
    // remove the last character, which Slint's expression language can't
    // do). Both UI-thread-only, like everything else in this group --
    // hashing/file I/O for the PIN is synchronous and local, no tokio
    // worker thread ever touches these.
    let pin_flow: Rc<RefCell<Option<PinFlow>>> = Rc::new(RefCell::new(None));
    let pin_buffer: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    // Parental PIN lock: every entry point just seeds `pin_flow` with the
    // right starting step and opens the pad; `on_pin_digit_pressed` (the
    // one handler that actually knows how to advance/finish every flow)
    // does the rest. All synchronous, local file I/O -- no rt_handle
    // anywhere in this feature, unlike almost everything else here.
    {
        let app_weak = app.as_weak();
        let pin_flow = pin_flow.clone();
        let pin_buffer = pin_buffer.clone();
        app.on_pin_nav_requested(move |page| {
            let Some(app) = app_weak.upgrade() else { return };
            *pin_flow.borrow_mut() = Some(PinFlow::UnlockForNav(page));
            open_pin_pad(&app, &pin_buffer, "Enter PIN");
        });
    }
    {
        let app_weak = app.as_weak();
        let pin_flow = pin_flow.clone();
        let pin_buffer = pin_buffer.clone();
        app.on_pin_setup_requested(move || {
            let Some(app) = app_weak.upgrade() else { return };
            *pin_flow.borrow_mut() = Some(PinFlow::SetupFirst);
            open_pin_pad(&app, &pin_buffer, "Set up a new PIN");
        });
    }
    {
        let app_weak = app.as_weak();
        let pin_flow = pin_flow.clone();
        let pin_buffer = pin_buffer.clone();
        app.on_pin_change_requested(move || {
            let Some(app) = app_weak.upgrade() else { return };
            *pin_flow.borrow_mut() = Some(PinFlow::ChangeVerifyCurrent);
            open_pin_pad(&app, &pin_buffer, "Enter current PIN");
        });
    }
    {
        let app_weak = app.as_weak();
        let pin_flow = pin_flow.clone();
        let pin_buffer = pin_buffer.clone();
        app.on_pin_disable_requested(move || {
            let Some(app) = app_weak.upgrade() else { return };
            *pin_flow.borrow_mut() = Some(PinFlow::DisableVerify);
            open_pin_pad(&app, &pin_buffer, "Enter current PIN to turn off lock");
        });
    }
    {
        let app_weak = app.as_weak();
        let pin_flow = pin_flow.clone();
        let pin_buffer = pin_buffer.clone();
        app.on_pin_pad_cancelled(move || {
            let Some(app) = app_weak.upgrade() else { return };
            *pin_flow.borrow_mut() = None;
            close_pin_pad(&app, &pin_buffer);
        });
    }
    {
        let app_weak = app.as_weak();
        let pin_buffer = pin_buffer.clone();
        app.on_pin_backspace_pressed(move || {
            let Some(app) = app_weak.upgrade() else { return };
            pin_buffer.borrow_mut().pop();
            app.set_pin_pad_digit_count(pin_buffer.borrow().len() as i32);
            app.set_pin_pad_error("".into());
        });
    }
    {
        let app_weak = app.as_weak();
        let pin_flow = pin_flow.clone();
        let pin_buffer = pin_buffer.clone();
        let pin_hash = pin_hash.clone();
        let pin_hash_path = pin_hash_path.clone();
        app.on_pin_digit_pressed(move |digit| {
            let Some(app) = app_weak.upgrade() else { return };
            {
                let mut buf = pin_buffer.borrow_mut();
                if buf.len() < 4 {
                    buf.push_str(&digit);
                }
            }
            let count = pin_buffer.borrow().len() as i32;
            app.set_pin_pad_digit_count(count);
            if count < 4 {
                return;
            }

            let entered = pin_buffer.borrow().clone();
            let is_correct = |candidate: &str| pin_hash.borrow().as_deref() == Some(hash_pin(candidate).as_str());
            // Restart the same step on a wrong/mismatched entry rather
            // than bouncing back to Settings -- clears the buffer/shows an
            // error but keeps the pad open so retrying doesn't need
            // another tap on Settings' button.
            let retry = |app: &AppWindow, pin_buffer: &Rc<RefCell<String>>, error: &str| {
                pin_buffer.borrow_mut().clear();
                app.set_pin_pad_digit_count(0);
                app.set_pin_pad_error(error.into());
            };

            // Bound to a `let` first, not `match pin_flow.borrow_mut().take() { ... }`
            // directly -- a `RefMut` created in a `match` scrutinee stays
            // borrowed for the entire match (same temporary-lifetime
            // extension pitfall this codebase already hit once with a
            // `MutexGuard` in an `if let` scrutinee), so every arm below
            // that also does `pin_flow.borrow_mut()` would panic with
            // "already borrowed" the instant it ran.
            let flow = pin_flow.borrow_mut().take();
            match flow {
                Some(PinFlow::UnlockForNav(page)) => {
                    if is_correct(&entered) {
                        app.set_pin_unlocked(true);
                        app.set_current_page(page);
                        close_pin_pad(&app, &pin_buffer);
                    } else {
                        *pin_flow.borrow_mut() = Some(PinFlow::UnlockForNav(page));
                        retry(&app, &pin_buffer, "Incorrect PIN");
                    }
                }
                Some(PinFlow::SetupFirst) => {
                    *pin_flow.borrow_mut() = Some(PinFlow::SetupConfirm(entered));
                    retry(&app, &pin_buffer, "");
                    app.set_pin_pad_prompt("Confirm new PIN".into());
                }
                Some(PinFlow::SetupConfirm(first)) => {
                    if entered != first {
                        *pin_flow.borrow_mut() = Some(PinFlow::SetupFirst);
                        retry(&app, &pin_buffer, "PINs didn't match -- try again");
                        app.set_pin_pad_prompt("Set up a new PIN".into());
                    } else if let Err(err) = save_pin_hash(&pin_hash_path, &entered) {
                        tracing::warn!(%err, "failed to save PIN");
                        *pin_flow.borrow_mut() = Some(PinFlow::SetupFirst);
                        retry(&app, &pin_buffer, "Couldn't save PIN, try again");
                        app.set_pin_pad_prompt("Set up a new PIN".into());
                    } else {
                        *pin_hash.borrow_mut() = Some(hash_pin(&entered));
                        app.set_pin_configured(true);
                        close_pin_pad(&app, &pin_buffer);
                    }
                }
                Some(PinFlow::ChangeVerifyCurrent) => {
                    if is_correct(&entered) {
                        *pin_flow.borrow_mut() = Some(PinFlow::ChangeNew);
                        retry(&app, &pin_buffer, "");
                        app.set_pin_pad_prompt("Enter new PIN".into());
                    } else {
                        *pin_flow.borrow_mut() = Some(PinFlow::ChangeVerifyCurrent);
                        retry(&app, &pin_buffer, "Incorrect PIN");
                    }
                }
                Some(PinFlow::ChangeNew) => {
                    *pin_flow.borrow_mut() = Some(PinFlow::ChangeConfirm(entered));
                    retry(&app, &pin_buffer, "");
                    app.set_pin_pad_prompt("Confirm new PIN".into());
                }
                Some(PinFlow::ChangeConfirm(new_pin)) => {
                    if entered != new_pin {
                        *pin_flow.borrow_mut() = Some(PinFlow::ChangeNew);
                        retry(&app, &pin_buffer, "PINs didn't match -- try again");
                        app.set_pin_pad_prompt("Enter new PIN".into());
                    } else if let Err(err) = save_pin_hash(&pin_hash_path, &entered) {
                        tracing::warn!(%err, "failed to save PIN");
                        *pin_flow.borrow_mut() = Some(PinFlow::ChangeNew);
                        retry(&app, &pin_buffer, "Couldn't save PIN, try again");
                        app.set_pin_pad_prompt("Enter new PIN".into());
                    } else {
                        *pin_hash.borrow_mut() = Some(hash_pin(&entered));
                        close_pin_pad(&app, &pin_buffer);
                    }
                }
                Some(PinFlow::DisableVerify) => {
                    if is_correct(&entered) {
                        if let Err(err) = remove_pin_hash(&pin_hash_path) {
                            tracing::warn!(%err, "failed to remove PIN file");
                        }
                        *pin_hash.borrow_mut() = None;
                        app.set_pin_configured(false);
                        // Already legitimately in this area -- don't
                        // immediately lock ourselves back out for having
                        // just turned the lock off.
                        app.set_pin_unlocked(true);
                        close_pin_pad(&app, &pin_buffer);
                    } else {
                        *pin_flow.borrow_mut() = Some(PinFlow::DisableVerify);
                        retry(&app, &pin_buffer, "Incorrect PIN");
                    }
                }
                // Shouldn't happen (the pad shouldn't be open without a
                // flow), but don't leave it stuck open if it does.
                None => close_pin_pad(&app, &pin_buffer),
            }
        });
    }

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
        let live_rest = live_rest.clone();
        let rt_handle = rt_handle.clone();
        let todo_uids = todo_uids.clone();
        let keyboard_buffer = keyboard_buffer.clone();
        let keyboard_target = keyboard_target.clone();
        let wifi_settings = wifi_settings.clone();
        let wifi_networks = wifi_networks.clone();
        let wifi_busy = wifi_busy.clone();
        let event_form_title = event_form_title.clone();
        let reference_date = reference_date.clone();
        let family_state = family_state.clone();
        let weather_entities = weather_entities.clone();
        let dashboard_sections = dashboard_sections.clone();
        app.on_keyboard_done(move || {
            let Some(app) = app_weak.upgrade() else { return };
            app.set_keyboard_open(false);
            let raw = keyboard_buffer.borrow().clone();
            let text = raw.trim().to_string();
            *keyboard_buffer.borrow_mut() = String::new();
            app.set_keyboard_text("".into());
            let Some(target) = keyboard_target.borrow_mut().take() else { return };

            match target {
                // Handled before the emptiness check below because a Wi-Fi
                // passphrase is the one target where surrounding whitespace is
                // legal and has to survive -- trimming it would silently turn a
                // correct password into an inexplicable "wrong password".
                KeyboardTarget::WifiPassword { ssid, security } => {
                    if raw.is_empty() {
                        return;
                    }
                    run_wifi_connect(
                        &rt_handle,
                        &app_weak,
                        &wifi_settings,
                        &wifi_networks,
                        &wifi_busy,
                        ssid,
                        Some(raw),
                        security,
                    );
                    return;
                }
                _ if text.is_empty() => return,
                _ => {}
            }

            match target {
                // Already handled above, which returns for this variant.
                KeyboardTarget::WifiPassword { .. } => {
                    unreachable!("Wi-Fi passphrases are handled before this match")
                }
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
                    let app_weak = app_weak.clone();
                    let live_rest = live_rest.clone();
                    let todo_uids = todo_uids.clone();
                    let ref_date = *reference_date.lock().unwrap();
                    let family = family_state.lock().unwrap().clone();
                    let weather_entities = weather_entities.clone();
                    let dashboard_sections = dashboard_sections.clone();
                    rt_handle.spawn(async move {
                        // No optimistic UI insert here (unlike the toggle
                        // callback) -- adding a task doesn't have a
                        // client-side uid to give it yet. Previously this
                        // relied entirely on the new item's own
                        // state_changed push (see run_ha_sync) to reflect
                        // it, "within moments" -- but that push is a
                        // best-effort WS message, not guaranteed, and on a
                        // miss the only fallback was the 5-minute poll
                        // (confirmed happening on real hardware: added from
                        // this app, showed up on another device's HA app
                        // right away, but didn't show up back here for
                        // several minutes). Refreshing directly after,
                        // same as event creation and every dashboard
                        // control already do, doesn't depend on the push
                        // succeeding at all for the common case of adding
                        // from this app itself.
                        if let Err(err) = add_todo_item(&client, &entity_id, &text).await {
                            tracing::warn!(%err, "failed to add HA todo item");
                        }
                        let rest = live_rest.lock().unwrap().clone();
                        if let Some(rest) = rest {
                            refresh_calendar_and_todos(
                                &rest,
                                &client,
                                &family,
                                local_offset(),
                                ref_date,
                                &app_weak,
                                &todo_uids,
                                &weather_entities,
                                &dashboard_sections,
                            )
                            .await;
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
            let now = OffsetDateTime::now_utc().to_offset(local_offset());
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
            let start = start_naive.assume_offset(local_offset());
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
                        local_offset(),
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
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset(), new_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
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
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset(), new_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
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
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset(), new_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
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
            let new_date = OffsetDateTime::now_utc().to_offset(local_offset()).date();
            *reference_date.lock().unwrap() = new_date;
            navigate(&app_weak, new_date);
            let family = family_state.lock().unwrap().clone();
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset(), new_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
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
            spawn_refresh(&rt_handle, &live_rest, &live_client, &family, local_offset(), ref_date, &app_weak, &todo_uids, &weather_entities, &dashboard_sections);
        });
    }

    // Dashboard controls: each flips the relevant bit of the on-screen
    // model immediately (set_dashboard_*_optimistically), *then* fires its
    // `call_service` and refreshes -- but only the dashboard page (see
    // refresh_dashboard_only), not the full calendar+todos+weather refresh
    // event creation uses. Two separate latency fixes stacked on top of
    // each other: the full refresh was doing a REST calendar fetch, a todo
    // fetch, and 3 sequential weather calls before it ever got to the one
    // get_states() the dashboard actually needed (3-4s to reflect a toggle
    // HA itself applies almost instantly); even after that fix, the
    // remaining single WS round trip was still visibly laggy on real
    // touchscreen hardware, hence the optimistic flip on top. If the
    // `call_service` call actually fails, the refresh right after corrects
    // the guess.
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
            if let Some(app) = app_weak.upgrade() {
                set_dashboard_entity_on_optimistically(&app, &entity_id, on);
            }
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
            if let Some(app) = app_weak.upgrade() {
                set_dashboard_group_on_optimistically(&app, &ids, on);
            }
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
            if let Some(app) = app_weak.upgrade() {
                set_dashboard_climate_mode_optimistically(&app, &entity_id, &mode);
            }
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
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        let dashboard_sections = dashboard_sections.clone();
        app.on_dashboard_climate_temp_delta(move |entity_id, delta| {
            let Some(client) = live_client.lock().unwrap().clone() else {
                tracing::warn!("not connected to HA, can't change thermostat temperature");
                return;
            };
            let entity_id = entity_id.to_string();
            // Computed from the card's own cached min/max/current-target
            // (set on every dashboard fetch -- see build_dashboard_cards)
            // rather than a fresh REST read of the current setpoint first,
            // which used to make every +/- tap a 2-round-trip operation
            // (read, then write) before the refresh afterward even
            // started -- confirmed slow on real touchscreen hardware.
            let Some(new_target) =
                app_weak.upgrade().and_then(|app| set_dashboard_climate_target_optimistically(&app, &entity_id, delta))
            else {
                tracing::warn!(entity = %entity_id, "no cached climate card to adjust temperature from");
                return;
            };
            let app_weak = app_weak.clone();
            let dashboard_sections = dashboard_sections.clone();
            rt_handle.spawn(async move {
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

    // ---- In-app software update -------------------------------------------
    // Deliberately configured from constants + environment variables rather
    // than `config.toml`: that file is `deny_unknown_fields` and a parse
    // failure is a hard `exit(1)` before any window exists, so a rollback to
    // a binary predating a new updater key would crash-loop with the rollback
    // already spent. See the module docs in update.rs.
    let update_settings = Arc::new(update::Settings::from_env());
    // A staged `<target>.new` left over from an install that failed or was
    // interrupted. ~20 MB on a small rootfs, so it goes at every startup; the
    // supervisor does the same before each spawn, since the app isn't
    // necessarily what runs next.
    update::cleanup_stale_staging(&update_settings);
    // One check or install at a time. Shared with the periodic task, so it's
    // an atomic rather than a `Cell` -- the manual button lives on the UI
    // thread and the periodic check lives on a tokio worker.
    let update_busy = Arc::new(AtomicBool::new(false));
    // The manifest a successful check found, so the Install button knows what
    // it is installing without re-fetching.
    let update_latest: Arc<Mutex<Option<update::Manifest>>> = Arc::new(Mutex::new(None));

    app.set_update_version_line(format!("Skylight HA {}", update::CURRENT_VERSION).into());
    app.set_update_build_line(format!("build {}", update::GIT_SHA).into());
    apply_update_state(&app, &update::State::load(&update_settings), false);
    {
        let update_settings = update_settings.clone();
        let update_busy = update_busy.clone();
        let update_latest = update_latest.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        app.on_update_check_requested(move || {
            let update_settings = update_settings.clone();
            let update_busy = update_busy.clone();
            let update_latest = update_latest.clone();
            let app_weak = app_weak.clone();
            rt_handle.spawn(async move {
                run_one_update_check(&update_settings, &app_weak, &update_busy, &update_latest)
                    .await;
            });
        });
    }
    {
        let update_settings = update_settings.clone();
        let update_busy = update_busy.clone();
        let update_latest = update_latest.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        app.on_update_install_requested(move || {
            let Some(manifest) = update_latest.lock().unwrap().clone() else {
                tracing::warn!("install tapped with no manifest in hand; ignoring");
                return;
            };
            // `swap` rather than load-then-store: two taps in quick succession
            // on a touchscreen are common, and the second must lose.
            if update_busy.swap(true, Ordering::SeqCst) {
                tracing::info!("an update check or install is already running; ignoring the tap");
                return;
            }
            let update_settings = update_settings.clone();
            let update_busy = update_busy.clone();
            let app_weak = app_weak.clone();
            rt_handle.spawn(async move {
                run_update_install(&update_settings, &app_weak, &update_busy, manifest).await;
            });
        });
    }

    // --- Wi-Fi ----------------------------------------------------------
    refresh_wifi_status(&rt_handle, &app.as_weak(), &wifi_settings);

    {
        let wifi_settings = wifi_settings.clone();
        let wifi_networks = wifi_networks.clone();
        let wifi_busy = wifi_busy.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        app.on_wifi_manage_requested(move || {
            let Some(app) = app_weak.upgrade() else { return };
            app.set_wifi_message("".into());
            app.set_wifi_view_open(true);
            // Scan straight away: opening the manager is itself the request to
            // see what's nearby.
            run_wifi_scan(&rt_handle, &app_weak, &wifi_settings, &wifi_networks, &wifi_busy);
        });
    }
    {
        let wifi_settings = wifi_settings.clone();
        let wifi_networks = wifi_networks.clone();
        let wifi_busy = wifi_busy.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        app.on_wifi_rescan(move || {
            run_wifi_scan(&rt_handle, &app_weak, &wifi_settings, &wifi_networks, &wifi_busy);
        });
    }
    {
        let app_weak = app.as_weak();
        app.on_wifi_view_closed(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_wifi_view_open(false);
                app.set_wifi_message("".into());
            }
        });
    }
    {
        let wifi_settings = wifi_settings.clone();
        let wifi_networks = wifi_networks.clone();
        let wifi_busy = wifi_busy.clone();
        let keyboard_target = keyboard_target.clone();
        let keyboard_buffer = keyboard_buffer.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        app.on_wifi_network_selected(move |index| {
            let Some(app) = app_weak.upgrade() else { return };
            let Some(network) = wifi_networks.lock().unwrap().get(index as usize).cloned() else {
                return;
            };
            if !network.security.supported() {
                app.set_wifi_message(
                    "WPA-Enterprise networks need credentials this screen can't collect.".into(),
                );
                return;
            }
            if !network.security.needs_password() {
                run_wifi_connect(
                    &rt_handle,
                    &app_weak,
                    &wifi_settings,
                    &wifi_networks,
                    &wifi_busy,
                    network.ssid,
                    None,
                    network.security,
                );
                return;
            }
            // Secured: collect the passphrase on the existing on-screen
            // keyboard, and finish the job in `on_keyboard_done`.
            *keyboard_buffer.borrow_mut() = String::new();
            *keyboard_target.borrow_mut() = Some(KeyboardTarget::WifiPassword {
                ssid: network.ssid.clone(),
                security: network.security,
            });
            app.set_keyboard_text("".into());
            app.set_keyboard_prompt(format!("Password for {}", network.ssid).into());
            app.set_keyboard_open(true);
        });
    }

    // Keeps the Settings card and the Wi-Fi header honest without polling the
    // control socket when nobody is looking at either.
    // Function-scoped, like `clock_timer` further down: a dropped Slint timer
    // stops, and this has to keep ticking for the life of the window.
    let wifi_status_timer = slint::Timer::default();
    {
        let wifi_settings = wifi_settings.clone();
        let rt_handle = rt_handle.clone();
        let app_weak = app.as_weak();
        wifi_status_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_secs(5),
            move || {
                let Some(app) = app_weak.upgrade() else { return };
                let watching =
                    app.get_wifi_view_open() || app.get_current_page() == ui::Page::Settings;
                if watching {
                    refresh_wifi_status(&rt_handle, &app_weak, &wifi_settings);
                }
            },
        );
    }

    // --- Time zone ------------------------------------------------------
    //
    // Everything here is UI-thread-only (Rc/RefCell, no spawning): reading the
    // tz database and rewriting a symlink are both local filesystem work that
    // finishes in microseconds, unlike the update feature's network calls.
    let timezone_settings = timezone::Settings::from_env();
    // Loaded on first open rather than at startup -- ~40KB of tab files that
    // nothing needs until someone actually taps "Change time zone".
    let tz_catalog: Rc<RefCell<Option<timezone::Catalog>>> = Rc::new(RefCell::new(None));
    let tz_level = Rc::new(RefCell::new(TzLevel::Continent));
    // The selection each visible row maps to (a continent, a country code, or
    // a zone name, depending on the level).
    let tz_payloads: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    apply_timezone_card(&app, &timezone_settings);

    // Re-renders whichever level `tz_level` currently holds.
    let show_tz_level = {
        let timezone_settings = timezone_settings.clone();
        let tz_catalog = tz_catalog.clone();
        let tz_level = tz_level.clone();
        let tz_payloads = tz_payloads.clone();
        move |app: &AppWindow| {
            let mut catalog_slot = tz_catalog.borrow_mut();
            let catalog = catalog_slot.get_or_insert_with(|| {
                timezone::Catalog::load(&timezone_settings)
            });
            let level = tz_level.borrow().clone();
            let (labels, payloads) = tz_level_rows(catalog, &level);
            let current = timezone::current_zone(&timezone_settings);
            apply_tz_level(app, catalog, &level, current.as_deref(), &payloads, &labels);
            *tz_payloads.borrow_mut() = payloads;
        }
    };

    {
        let show_tz_level = show_tz_level.clone();
        let tz_level = tz_level.clone();
        let app_weak = app.as_weak();
        app.on_timezone_change_requested(move || {
            let Some(app) = app_weak.upgrade() else { return };
            *tz_level.borrow_mut() = TzLevel::Continent;
            show_tz_level(&app);
            app.set_tz_picker_open(true);
        });
    }
    {
        let app_weak = app.as_weak();
        app.on_tz_picker_cancelled(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_tz_picker_open(false);
            }
        });
    }
    {
        let show_tz_level = show_tz_level.clone();
        let tz_level = tz_level.clone();
        let app_weak = app.as_weak();
        app.on_tz_picker_back(move || {
            let Some(app) = app_weak.upgrade() else { return };
            // Scoped so the borrow is released before `show_tz_level` takes
            // its own -- the same RefCell-held-across-a-call hazard that once
            // crashed the PIN pad on its fourth digit.
            {
                let mut level = tz_level.borrow_mut();
                *level = tz_level_after_back(&level);
            }
            show_tz_level(&app);
        });
    }
    {
        let timezone_settings = timezone_settings.clone();
        let show_tz_level = show_tz_level.clone();
        let tz_catalog = tz_catalog.clone();
        let tz_level = tz_level.clone();
        let tz_payloads = tz_payloads.clone();
        let app_weak = app.as_weak();
        app.on_tz_picker_row_selected(move |index| {
            let Some(app) = app_weak.upgrade() else { return };
            let Some(payload) = tz_payloads.borrow().get(index as usize).cloned() else {
                tracing::warn!(index, "time zone row tapped with no payload; ignoring");
                return;
            };

            // Decided with every borrow released before anything acts on it.
            let next = {
                let level = tz_level.borrow().clone();
                let catalog_slot = tz_catalog.borrow();
                match catalog_slot.as_ref() {
                    Some(catalog) => tz_level_after_selection(catalog, &level, &payload),
                    None => {
                        tracing::warn!("time zone row tapped before the catalog loaded; ignoring");
                        return;
                    }
                }
            };

            match next {
                TzSelection::Descend(level) => {
                    *tz_level.borrow_mut() = level;
                    show_tz_level(&app);
                }
                TzSelection::Choose { zone, label } => {
                    apply_timezone_and_restart(&app, &timezone_settings, &zone, &label);
                }
            }
        });
    }

    // Proof of life, and the app's half of the rollback contract: the
    // supervisor treats a `pending` marker that is still present when the
    // child dies as a failed update, so something has to clear it once the
    // new binary has demonstrably worked. A *Slint* timer is the right
    // instrument because it only fires if the event loop is genuinely
    // running -- much stronger evidence than "the process hasn't exited",
    // which a binary wedged before `app.run()` would also satisfy.
    //
    // Deliberately not gated on a successful HA connection: WiFi or HA being
    // briefly down is routine here and says nothing about whether this binary
    // works, and gating on it would roll back perfectly good releases during
    // a router reboot.
    {
        let update_settings = update_settings.clone();
        slint::Timer::single_shot(std::time::Duration::from_secs(60), move || {
            update::commit_pending(&update_settings);
        });
    }

    // The 1s clock tick does three jobs, not one:
    //   1. redraw the clock/date text (what it always did),
    //   2. re-derive the local UTC offset, so a clock corrected by NTP after
    //      startup (this board has no RTC -- it boots in 1970) stops being
    //      displayed in the wrong DST bucket, and
    //   3. notice the calendar day actually changing -- either an ordinary
    //      midnight rollover or that same NTP correction -- and move the
    //      calendar's `reference_date` with it, rather than leaving the grid
    //      parked on the boot-time date until someone taps "Today".
    //
    // These all belong on the same timer because they're all "what time does
    // this device think it is", and the offset lookup has to happen somewhere
    // that runs repeatedly rather than once at startup.
    let clock_weak = app.as_weak();
    let clock_timer = slint::Timer::default();
    // Cloned here rather than reusing the bindings below, because
    // `run_ha_sync` takes ownership of all of these a few lines further down.
    let clock_reference_date = reference_date.clone();
    let clock_rt_handle = rt_handle.clone();
    let clock_live_rest = live_rest.clone();
    let clock_live_client = live_client.clone();
    let clock_family_state = family_state.clone();
    let clock_todo_uids = todo_uids.clone();
    let clock_weather_entities = weather_entities.clone();
    let clock_dashboard_sections = dashboard_sections.clone();
    // What this tick believed "today" was last time round. Seeded with the
    // startup value, which on a fresh boot is very likely 1970-01-01.
    let mut last_today = today;
    clock_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || {
            // Cheap: `localtime_r` works off libc's cached zone rules after
            // the one-time `tzset()` in `main`, so this is arithmetic, not a
            // filesystem read, every second.
            refresh_local_offset();
            let offset = local_offset();
            let now = OffsetDateTime::now_utc().to_offset(offset);

            if let Some(app) = clock_weak.upgrade() {
                app.set_clock_text(format!("{:02}:{:02}", now.hour(), now.minute()).into());
                app.set_date_text(format!("{}", now.date()).into());
                // Deliberately not touching `month_label` here -- it tracks
                // `reference_date` (whatever's being navigated/viewed), not
                // wall-clock "now". It *is* updated below, but only on the
                // rare tick where `reference_date` itself moves.
            }

            let new_today = now.date();
            if new_today == last_today {
                return;
            }
            let previous_today = last_today;
            last_today = new_today;
            tracing::info!(
                previous_today = %previous_today,
                new_today = %new_today,
                "wall-clock date changed (midnight rollover, or the clock was corrected)"
            );

            // `build_calendar_grids` already recomputes "today" itself for
            // *highlighting*, so the highlight was never stuck -- what was
            // stuck is the grid's own reference/display date, which is this.
            let Some(new_date) = ({
                let guard = clock_reference_date.lock().unwrap();
                reference_date_after_today_changed(*guard, previous_today, new_today)
            }) else {
                return;
            };
            *clock_reference_date.lock().unwrap() = new_date;
            navigate(&clock_weak, new_date);
            let family = clock_family_state.lock().unwrap().clone();
            spawn_refresh(
                &clock_rt_handle,
                &clock_live_rest,
                &clock_live_client,
                &family,
                offset,
                new_date,
                &clock_weak,
                &clock_todo_uids,
                &clock_weather_entities,
                &clock_dashboard_sections,
            );
        },
    );

    rt.spawn(run_ha_sync(
        config,
        app.as_weak(),
        live_client,
        live_rest,
        todo_uids,
        reference_date,
        family_state,
        weather_entities,
        dashboard_sections,
    ));

    // Same shape as `run_ha_sync`: a detached task on the same runtime that
    // talks to the UI only through `slint::invoke_from_event_loop`. It can
    // fail as much as it likes without affecting anything else.
    rt.spawn(run_update_checks(
        update_settings,
        app.as_weak(),
        update_busy,
        update_latest,
    ));

    app.run().expect("event loop error");
}

/// The periodic update check.
///
/// Lives in the Rust app rather than in `crond` -- a deliberate departure from
/// this project's usual "system automation is shell + cron" norm, forced by the
/// target image: busybox here is built without HTTPS support, there is no
/// `curl` or `openssl`, and `/etc/ssl/certs` is empty, so a shell script on
/// this device physically cannot fetch a manifest. This binary already carries
/// rustls and a bundled root store for Home Assistant.
///
/// Never installs anything. The most it does is set a dot on the Settings nav
/// item.
async fn run_update_checks(
    settings: Arc<update::Settings>,
    app_weak: slint::Weak<AppWindow>,
    busy: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<update::Manifest>>>,
) {
    if !settings.periodic_enabled {
        tracing::info!(
            "periodic update checks are off (SKYLIGHT_UPDATE_DISABLE); the Settings button still works"
        );
        return;
    }
    tracing::info!(
        first_check_in_secs = settings.first_check_delay.as_secs(),
        interval_secs = settings.check_interval.as_secs(),
        base_url = %settings.base_url,
        "update checks armed"
    );

    // The first check waits out the documented cold-boot settling window
    // (WiFi association, then DHCP, then the NTP correction off the 1970
    // kernel epoch -- 30s to a minute, inconsistently). Checking sooner would
    // mostly just record a failure.
    tokio::time::sleep(settings.first_check_delay).await;
    loop {
        run_one_update_check(&settings, &app_weak, &busy, &latest).await;
        // Jitter so a fleet, or one device that reboots on a schedule, doesn't
        // hit GitHub in lockstep.
        tokio::time::sleep(settings.check_interval + update::jitter(settings.check_interval)).await;
    }
}

/// One check: mark busy, fetch, record, push to the UI. Shared by the periodic
/// task and the Settings button so the two can't diverge.
async fn run_one_update_check(
    settings: &update::Settings,
    app_weak: &slint::Weak<AppWindow>,
    busy: &Arc<AtomicBool>,
    latest: &Arc<Mutex<Option<update::Manifest>>>,
) {
    if busy.swap(true, Ordering::SeqCst) {
        tracing::info!("an update check or install is already running; skipping this one");
        return;
    }
    push_update_status(app_weak, settings, "Checking for updates...", true);

    let outcome = match update::http_client() {
        Ok(http) => update::check(settings, &http).await,
        Err(err) => Err(update::Error::Http(err)),
    };
    busy.store(false, Ordering::SeqCst);

    // Every failure here is soft by design: a wall appliance whose WiFi is
    // occasionally down must not treat "couldn't reach GitHub" as anything
    // more than a line of text in Settings.
    match &outcome {
        Ok(update::CheckOutcome::ClockNotReady) => {
            *latest.lock().unwrap() = None;
            // No state was persisted (this isn't a real check), so say so
            // directly rather than going through `apply_update_state`.
            push_update_status(
                app_weak,
                settings,
                "Waiting for the clock to be set before checking.",
                false,
            );
            return;
        }
        Ok(update::CheckOutcome::Available(manifest)) => {
            tracing::info!(version = %manifest.version, "an update is available");
            *latest.lock().unwrap() = Some(manifest.clone());
        }
        Ok(other) => {
            tracing::info!(?other, "update check finished with nothing to install");
            *latest.lock().unwrap() = None;
        }
        Err(err) => {
            tracing::warn!(%err, "update check failed (soft -- will retry)");
            *latest.lock().unwrap() = None;
        }
    }

    let state = update::State::load(settings);
    let app_weak = app_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app_weak.upgrade() {
            apply_update_state(&app, &state, false);
        }
    });
}

/// Download, verify, preflight, swap, then show the overlay and exit so the
/// supervisor respawns against the new binary.
async fn run_update_install(
    settings: &update::Settings,
    app_weak: &slint::Weak<AppWindow>,
    busy: &Arc<AtomicBool>,
    manifest: update::Manifest,
) {
    let version = manifest.version.clone();
    tracing::info!(%version, "starting update install");

    let progress = {
        let app_weak = app_weak.clone();
        move |phase: update::Phase| {
            let text = match phase {
                update::Phase::Downloading { received, total } => {
                    let percent = received.checked_mul(100).and_then(|n| n.checked_div(total)).unwrap_or(0);
                    format!("Downloading... {percent}%")
                }
                update::Phase::Verifying => "Verifying download...".to_string(),
                update::Phase::Preflight => "Checking the new version runs...".to_string(),
                update::Phase::Installing => "Installing...".to_string(),
            };
            let app_weak = app_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    app.set_update_status_text(text.into());
                    app.set_update_busy(true);
                }
            });
        }
    };

    let result = match update::http_client() {
        Ok(http) => update::install(settings, &http, &manifest, &progress).await,
        Err(err) => Err(update::Error::Http(err)),
    };

    match result {
        Ok(()) => {
            // Note `busy` is deliberately *not* cleared: the binary on disk is
            // no longer the one running, so there is nothing sensible left to
            // do in this process but exit.
            let app_weak = app_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = app_weak.upgrade() else {
                    // Without a window there's no frame to paint, so nothing
                    // is gained by waiting.
                    std::process::exit(0);
                };
                app.set_update_status_text("Restarting...".into());
                app.set_update_overlay_detail(
                    format!("Installing version {version} and restarting. This takes a few seconds.")
                        .into(),
                );
                app.set_update_overlay_open(true);
                // Give Slint a chance to actually *paint* the overlay before
                // the process goes away: this is a DRM/KMS app, so whatever
                // frame is on screen when it exits is what stays on screen
                // until the respawn draws over it. Without this the user sees
                // a frozen dashboard, which is indistinguishable from the
                // crash this feature is supposed to avoid looking like.
                slint::Timer::single_shot(std::time::Duration::from_millis(1500), || {
                    tracing::info!("exiting so skylight-supervise respawns the new binary");
                    std::process::exit(0);
                });
            });
        }
        Err(err) => {
            tracing::error!(%err, %version, "update install failed");
            busy.store(false, Ordering::SeqCst);
            let message = format!("Update failed: {}", update::short_error(&err));
            let app_weak = app_weak.clone();
            let settings_snapshot = settings.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    // Re-apply the persisted state first so the version/notes
                    // lines and the Install button come back, then overwrite
                    // just the status line with why it failed.
                    apply_update_state(&app, &update::State::load(&settings_snapshot), false);
                    app.set_update_status_text(message.into());
                }
            });
        }
    }
}

/// Pushes one status line (and the busy flag) without touching anything else.
/// Used for the transient "Checking..."/"Waiting for the clock" states that
/// aren't worth persisting.
fn push_update_status(
    app_weak: &slint::Weak<AppWindow>,
    settings: &update::Settings,
    text: &str,
    busy: bool,
) {
    let text = text.to_string();
    let settings = settings.clone();
    let app_weak = app_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app_weak.upgrade() {
            // Keep the rest of the card consistent (version line, install
            // button) with whatever was last persisted.
            apply_update_state(&app, &update::State::load(&settings), busy);
            app.set_update_status_text(text.into());
        }
    });
}

/// Which level of the timezone drill-down is on screen.
///
/// Rust owns this rather than the markup so the grouping logic stays in
/// `timezone.rs` where it is unit-tested, and the picker stays a dumb list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TzLevel {
    Continent,
    Country { continent: String },
    Zone { continent: String, country_code: String, country_name: String },
}

/// The rows for a level: what to display, and the payload each row selects.
///
/// Returned as parallel vectors because the label and the thing it selects
/// genuinely differ at every level -- "United States" selects `US`, "Eastern
/// (most areas)" selects `America/New_York`.
fn tz_level_rows(catalog: &timezone::Catalog, level: &TzLevel) -> (Vec<String>, Vec<String>) {
    match level {
        TzLevel::Continent => {
            let continents = catalog.continents();
            (continents.clone(), continents)
        }
        TzLevel::Country { continent } => {
            let countries = catalog.countries(continent);
            (
                countries.iter().map(|c| c.name.clone()).collect(),
                countries.iter().map(|c| c.code.clone()).collect(),
            )
        }
        TzLevel::Zone { continent, country_code, .. } => {
            let zones = catalog.zones(continent, country_code);
            (
                zones.iter().map(|z| z.label.clone()).collect(),
                zones.iter().map(|z| z.name.clone()).collect(),
            )
        }
    }
}

/// What tapping a row should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TzSelection {
    Descend(TzLevel),
    /// The zone to apply, plus the label the user actually tapped -- so the
    /// confirmation can say "Eastern - New York" rather than surprising them
    /// with a tz name the row never mentioned.
    Choose { zone: String, label: String },
}

/// Where a tap on `payload` takes the drill-down from `level`.
///
/// Pure and separate from the callback so the two non-obvious rules here are
/// actually testable: a country with exactly one zone is chosen immediately
/// rather than making the user confirm a single-item list, and a tap on the
/// leaf level is a choice rather than a descent.
fn tz_level_after_selection(
    catalog: &timezone::Catalog,
    level: &TzLevel,
    payload: &str,
) -> TzSelection {
    match level {
        TzLevel::Continent => {
            TzSelection::Descend(TzLevel::Country { continent: payload.to_string() })
        }
        TzLevel::Country { continent } => {
            let zones = catalog.zones(continent, payload);
            if let [only] = zones.as_slice() {
                return TzSelection::Choose {
                    zone: only.name.clone(),
                    label: only.label.clone(),
                };
            }
            let country_name = catalog
                .countries(continent)
                .into_iter()
                .find(|c| c.code == payload)
                .map(|c| c.name)
                .unwrap_or_else(|| payload.to_string());
            TzSelection::Descend(TzLevel::Zone {
                continent: continent.clone(),
                country_code: payload.to_string(),
                country_name,
            })
        }
        TzLevel::Zone { continent, country_code, .. } => {
            let label = catalog
                .zones(continent, country_code)
                .into_iter()
                .find(|zone| zone.name == payload)
                .map(|zone| zone.label)
                .unwrap_or_else(|| payload.to_string());
            TzSelection::Choose { zone: payload.to_string(), label }
        }
    }
}

/// One level up. The top level is its own parent, so a stray Back tap at the
/// root is inert rather than closing the picker unexpectedly.
fn tz_level_after_back(level: &TzLevel) -> TzLevel {
    match level {
        TzLevel::Continent => TzLevel::Continent,
        TzLevel::Country { .. } => TzLevel::Continent,
        TzLevel::Zone { continent, .. } => TzLevel::Country { continent: continent.clone() },
    }
}

/// Pushes one level of the drill-down into the picker's properties.
fn apply_tz_level(
    app: &AppWindow,
    catalog: &timezone::Catalog,
    level: &TzLevel,
    current_zone: Option<&str>,
    payloads: &[String],
    labels: &[String],
) {
    let (title, breadcrumb, leaf) = match level {
        TzLevel::Continent => ("Select a region", String::new(), false),
        TzLevel::Country { continent } => ("Select a country", continent.clone(), false),
        TzLevel::Zone { continent, country_name, .. } => {
            ("Select a time zone", format!("{continent} \u{203a} {country_name}"), true)
        }
    };

    // Highlight the row that leads to (or is) the zone currently in effect, at
    // every level -- so drilling down shows where you already are rather than
    // only revealing it on the last screen. `locate` answers which continent
    // and country the current zone lives under.
    let located = current_zone.and_then(|zone| catalog.locate(zone));
    let wanted: Option<String> = match level {
        TzLevel::Continent => located.map(|(continent, _)| continent),
        TzLevel::Country { .. } => located.map(|(_, country_code)| country_code),
        TzLevel::Zone { .. } => current_zone.map(|zone| zone.to_string()),
    };
    let selected = wanted
        .and_then(|wanted| payloads.iter().position(|p| *p == wanted))
        .map(|i| i as i32)
        .unwrap_or(-1);

    app.set_tz_picker_title(title.into());
    app.set_tz_picker_breadcrumb(breadcrumb.into());
    app.set_tz_picker_leaf(leaf);
    app.set_tz_picker_selected_index(selected);
    app.set_tz_picker_can_go_back(!matches!(level, TzLevel::Continent));
    app.set_tz_picker_rows(slint::ModelRc::new(slint::VecModel::from(
        labels.iter().map(SharedString::from).collect::<Vec<_>>(),
    )));
}

/// Fills the Settings card: which zone is set, and what it currently resolves
/// to (abbreviation + offset), which is how the user can see DST being applied.
fn apply_timezone_card(app: &AppWindow, settings: &timezone::Settings) {
    let zone = timezone::current_zone(settings);
    app.set_timezone_name(
        zone.clone().unwrap_or_else(|| "System default".to_string()).into(),
    );

    let now = OffsetDateTime::now_utc();
    let offset = local_offset();
    let hours = i32::from(offset.whole_hours());
    let minutes = (i32::from(offset.whole_minutes()) - hours * 60).abs();
    let offset_text = if minutes == 0 {
        format!("UTC{hours:+}")
    } else {
        format!("UTC{hours:+}:{minutes:02}")
    };
    let detail = match system_timezone_abbreviation(now) {
        Some(abbreviation) => format!("{abbreviation} - {offset_text}"),
        None => offset_text,
    };
    app.set_timezone_detail(detail.into());
}

/// Renders scan results for the list, marking whichever one is in use.
fn wifi_network_model(
    networks: &[wifi::Network],
    connected_ssid: Option<&str>,
) -> Vec<WifiNetworkData> {
    networks
        .iter()
        .map(|network| WifiNetworkData {
            ssid: network.ssid.clone().into(),
            bars: i32::from(network.bars()),
            secured: network.security.needs_password(),
            supported: network.security.supported(),
            connected: connected_ssid == Some(network.ssid.as_str()),
        })
        .collect()
}

fn set_wifi_networks(app: &AppWindow, networks: &[wifi::Network], connected_ssid: Option<&str>) {
    app.set_wifi_networks(slint::ModelRc::new(slint::VecModel::from(wifi_network_model(
        networks,
        connected_ssid,
    ))));
}

/// Opens both control connections: one for commands, one attached for events.
///
/// Separate because an event arriving mid-command would otherwise be read as
/// that command's reply -- the same split `wpa_cli` uses.
fn wifi_connect_ctrl(settings: &wifi::Settings) -> wifi::Result<(wifi::Ctrl, wifi::Ctrl)> {
    let commands = wifi::Ctrl::connect(settings)?;
    let events = wifi::Ctrl::connect(settings)?;
    events.attach()?;
    Ok((commands, events))
}

/// Pushes the current connection state into the Settings card and the Wi-Fi
/// view's header.
fn refresh_wifi_status(rt_handle: &tokio::runtime::Handle, app_weak: &slint::Weak<AppWindow>, settings: &wifi::Settings) {
    let settings = settings.clone();
    let app_weak = app_weak.clone();
    rt_handle.spawn_blocking(move || {
        let status = wifi::Ctrl::connect(&settings).and_then(|ctrl| wifi::status(&ctrl));
        let (line, ssid) = match status {
            Ok(status) => (status.summary(), status.ssid.clone()),
            // Not an error worth shouting about: on the dev machine there is
            // no control socket at all, and on the device wpa_supplicant may
            // simply not be up yet.
            Err(err) => {
                tracing::debug!(%err, "wifi status unavailable");
                ("Wi-Fi status unavailable".to_string(), None)
            }
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = app_weak.upgrade() else { return };
            app.set_wifi_status(line.into());
            // Re-mark the connected row without re-scanning.
            let networks = app.get_wifi_networks();
            let refreshed: Vec<WifiNetworkData> = networks
                .iter()
                .map(|mut row| {
                    row.connected = ssid.as_deref() == Some(row.ssid.as_str());
                    row
                })
                .collect();
            app.set_wifi_networks(slint::ModelRc::new(slint::VecModel::from(refreshed)));
        });
    });
}

/// Scans for networks and publishes the results.
fn run_wifi_scan(
    rt_handle: &tokio::runtime::Handle,
    app_weak: &slint::Weak<AppWindow>,
    settings: &wifi::Settings,
    networks: &Arc<Mutex<Vec<wifi::Network>>>,
    busy: &Arc<AtomicBool>,
) {
    // `swap` not load-then-store: double taps are routine on a touchscreen and
    // the second one must lose, same as the update buttons.
    if busy.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(app) = app_weak.upgrade() {
        app.set_wifi_busy(true);
        app.set_wifi_message("".into());
    }

    let settings = settings.clone();
    let networks = networks.clone();
    let busy = busy.clone();
    let app_weak = app_weak.clone();
    // spawn_blocking, not spawn: these are synchronous socket round trips that
    // wait seconds for a scan, and they must not sit on a tokio worker that
    // the HA client also needs.
    rt_handle.spawn_blocking(move || {
        let result = wifi_connect_ctrl(&settings).and_then(|(ctrl, events)| {
            let found = wifi::scan(&ctrl, &events, wifi::SCAN_TIMEOUT)?;
            let status = wifi::status(&ctrl).unwrap_or_default();
            Ok((found, status))
        });
        busy.store(false, Ordering::SeqCst);

        match result {
            Ok((found, status)) => {
                *networks.lock().unwrap() = found.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = app_weak.upgrade() else { return };
                    app.set_wifi_busy(false);
                    app.set_wifi_status(status.summary().into());
                    set_wifi_networks(&app, &found, status.ssid.as_deref());
                    if found.is_empty() {
                        app.set_wifi_message("No networks found nearby.".into());
                    }
                });
            }
            Err(err) => {
                tracing::warn!(%err, "wifi scan failed");
                let message = format!("Could not scan: {err}");
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = app_weak.upgrade() else { return };
                    app.set_wifi_busy(false);
                    app.set_wifi_message(message.into());
                });
            }
        }
    });
}

/// Joins a network, reporting precisely why if it doesn't work.
///
/// The password is never logged, here or in `wifi.rs`.
fn run_wifi_connect(
    rt_handle: &tokio::runtime::Handle,
    app_weak: &slint::Weak<AppWindow>,
    settings: &wifi::Settings,
    networks: &Arc<Mutex<Vec<wifi::Network>>>,
    busy: &Arc<AtomicBool>,
    ssid: String,
    password: Option<String>,
    security: wifi::Security,
) {
    if busy.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(app) = app_weak.upgrade() {
        app.set_wifi_busy(true);
        app.set_wifi_message(format!("Connecting to {ssid}...").into());
    }

    let settings = settings.clone();
    let networks = networks.clone();
    let busy = busy.clone();
    let app_weak = app_weak.clone();
    rt_handle.spawn_blocking(move || {
        let result = wifi_connect_ctrl(&settings).and_then(|(ctrl, events)| {
            wifi::connect(
                &ctrl,
                &events,
                &ssid,
                password.as_deref(),
                security,
                wifi::CONNECT_TIMEOUT,
            )?;
            Ok(wifi::status(&ctrl).unwrap_or_default())
        });
        busy.store(false, Ordering::SeqCst);

        let known = networks.lock().unwrap().clone();
        match result {
            Ok(status) => {
                tracing::info!(%ssid, "joined wifi network");
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = app_weak.upgrade() else { return };
                    app.set_wifi_busy(false);
                    app.set_wifi_message(format!("Connected to {ssid}.").into());
                    app.set_wifi_status(status.summary().into());
                    set_wifi_networks(&app, &known, status.ssid.as_deref());
                });
            }
            Err(err) => {
                // Deliberately specific: telling a typo apart from an
                // out-of-range AP is the entire reason this talks to the
                // control socket rather than rewriting wpa_supplicant.conf.
                let message = match err {
                    wifi::Error::WrongPassword => {
                        format!("Wrong password for {ssid}. The previous network has been restored.")
                    }
                    wifi::Error::ConnectTimeout => format!(
                        "Could not connect to {ssid} in time. The previous network has been restored."
                    ),
                    other => format!("Could not connect to {ssid}: {other}"),
                };
                tracing::warn!(%ssid, "wifi connect failed: {message}");
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = app_weak.upgrade() else { return };
                    app.set_wifi_busy(false);
                    app.set_wifi_message(message.into());
                });
            }
        }
    });
}

/// Points the system at `zone`, then shows a curtain and exits so the
/// supervisor restarts the app into it.
///
/// The restart is not laziness: musl caches the zone by the `TZ` *string* and
/// never re-stats `/etc/localtime`, and the only in-process escape --
/// `setenv` -- is unsound in a multithreaded process. See the header of
/// `timezone.rs` for the full reasoning (and `system_utc_offset` above for the
/// same hazard in its original form).
///
/// Reuses the update feature's overlay rather than adding a near-identical
/// one: it is already exactly "a full-screen explanation shown for a beat
/// before `exit(0)`", which is precisely this situation too.
fn apply_timezone_and_restart(
    app: &AppWindow,
    settings: &timezone::Settings,
    zone: &str,
    label: &str,
) {
    match timezone::apply(settings, zone) {
        Ok(()) => {
            app.set_tz_picker_open(false);
            app.set_update_overlay_title("Changing time zone...".into());
            app.set_update_overlay_detail(
                format!("Switching to {label} and restarting. This takes a few seconds.").into(),
            );
            app.set_update_overlay_open(true);
            // Same 1.5s as the updater, and for the same reason: a DRM/KMS
            // process leaves its last frame on screen when it exits, so the
            // explanation has to actually get painted first or the restart
            // just looks like a freeze.
            slint::Timer::single_shot(std::time::Duration::from_millis(1500), || {
                tracing::info!("exiting so skylight-supervise respawns in the new time zone");
                std::process::exit(0);
            });
        }
        Err(err) => {
            tracing::error!(%err, zone, "could not change the time zone");
            app.set_tz_picker_open(false);
            app.set_timezone_detail(format!("Could not change time zone: {err}").into());
        }
    }
}

/// Renders a persisted [`update::State`] into the Settings card's properties.
///
/// All the "what should this say" logic is here rather than in `.slint`
/// because it has to fold together six mutually-exclusive situations, and Rust
/// already has to decide between them to write `state.json` at all.
fn apply_update_state(app: &AppWindow, state: &update::State, busy: bool) {
    app.set_update_available(state.available);
    app.set_update_installable(state.installable());
    app.set_update_busy(busy);
    app.set_update_notes(state.notes.clone().unwrap_or_default().into());
    if let Some(version) = &state.latest_version {
        app.set_update_install_label(format!("Install {version}").into());
    }
    app.set_update_status_text(update_status_line(state, busy).into());
}

/// The single line under the version in the Software Update card.
///
/// Split out from `apply_update_state` purely so it can be unit-tested without
/// a live `AppWindow`.
fn update_status_line(state: &update::State, busy: bool) -> String {
    if busy {
        return "Checking for updates...".to_string();
    }
    let latest = state.latest_version.as_deref().unwrap_or("?");
    if state.blocked {
        return format!(
            "Version {latest} was installed but failed to start, so it won't be offered again."
        );
    }
    if state.requires_reflash {
        return format!(
            "Version {latest} is available, but needs a manual SD-card reflash rather than an \
             in-app update."
        );
    }
    if state.available {
        return format!("Version {latest} is available.");
    }
    match (&state.last_check_error, state.last_check_epoch) {
        (Some(err), _) => format!("Last check failed: {err}"),
        (None, Some(epoch)) => {
            format!("Up to date. Last checked {}.", format_check_time(epoch, local_offset()))
        }
        (None, None) => "Not checked yet.".to_string(),
    }
}

/// Renders a stored check timestamp in local time.
///
/// Callers pass the current [`local_offset`] (the self-correcting one the clock
/// tick maintains), so a check recorded before NTP fixed the clock isn't
/// rendered in the wrong DST bucket afterwards. Taken as an argument rather
/// than read from the global purely so this is testable without writing to
/// `LOCAL_OFFSET_SECONDS`, which the offset tests also read.
///
/// A value the `time` crate can't represent falls back to the raw epoch rather
/// than being hidden -- it's diagnostic text either way.
fn format_check_time(epoch: u64, offset: UtcOffset) -> String {
    // `try_from`, not `as i64`: an absurd stored value would otherwise wrap to
    // a negative timestamp and render as a plausible-looking 1969 date instead
    // of being recognised as nonsense.
    match i64::try_from(epoch).map_err(|_| ()).and_then(|secs| {
        OffsetDateTime::from_unix_timestamp(secs).map_err(|_| ())
    }) {
        Ok(when) => {
            let local = when.to_offset(offset);
            format!("{} {}", local.date(), format_time_12h(local))
        }
        Err(_) => format!("at {epoch}"),
    }
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

fn open_pin_pad(app: &AppWindow, pin_buffer: &Rc<RefCell<String>>, prompt: &str) {
    pin_buffer.borrow_mut().clear();
    app.set_pin_pad_digit_count(0);
    app.set_pin_pad_error("".into());
    app.set_pin_pad_prompt(prompt.into());
    app.set_pin_pad_open(true);
}

fn close_pin_pad(app: &AppWindow, pin_buffer: &Rc<RefCell<String>>) {
    pin_buffer.borrow_mut().clear();
    app.set_pin_pad_open(false);
    app.set_pin_pad_error("".into());
    app.set_pin_pad_digit_count(0);
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
    /// Joining a secured network. Carries what the password is *for*, since
    /// the scan list can be rescanned (and reordered) while the keyboard is up.
    WifiPassword { ssid: String, security: wifi::Security },
}

/// Drives the parental PIN pad's multi-step flows -- see `pin_flow` in
/// `main`. Every variant is handled by the same `on_pin_digit_pressed`,
/// which matches on this to decide what a completed 4-digit entry means.
enum PinFlow {
    /// Entering the PIN to unlock Dashboard/Settings nav -- carries which
    /// page was actually tapped, so a correct entry can switch straight to
    /// it.
    UnlockForNav(ui::Page),
    /// First entry of a brand-new PIN (no existing one to check against --
    /// any 4 digits are accepted and become the tentative PIN).
    SetupFirst,
    /// Re-entry to confirm a new PIN; holds the first attempt.
    SetupConfirm(String),
    /// Must prove the current PIN before changing it.
    ChangeVerifyCurrent,
    ChangeNew,
    /// Re-entry to confirm the new PIN when changing; holds the new one.
    ChangeConfirm(String),
    /// Must prove the current PIN before turning the lock off entirely.
    DisableVerify,
}

/// Hex SHA-256 of `pin`. Proportionate to the actual threat model here (a
/// curious kid, not an attacker trying to brute-force a 4-digit code) --
/// better than plaintext on disk without pretending this is real auth.
fn hash_pin(pin: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pin.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn load_pin_hash(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn save_pin_hash(path: &str, pin: &str) -> std::io::Result<()> {
    std::fs::write(path, hash_pin(pin))
}

fn remove_pin_hash(path: &str) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
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

    let (primary_entity, backfill_entity) = {
        let entities = weather_entities.lock().unwrap();
        (entities.primary.clone(), entities.backfill.clone())
    };

    // Dashboard refresh is deliberately *not* joined with the fetches
    // below -- it's fired off as its own detached task instead, updating
    // the Dashboard page independently whenever it's ready rather than
    // gating (or being gated by) calendar/todo/weather. This matters
    // because `get_states()` (what the dashboard needs) turned out to be
    // wildly variable on a real instance -- confirmed live: ~460ms once,
    // 17.5s for 538 entities another time -- and with everything joined
    // together (the previous version of this function), that one slow
    // fetch held up pushing calendar/todo/weather updates that were ready
    // in under a second, which is what several "feels slow"/"doesn't
    // update" reports from real touchscreen testing actually traced back
    // to. `refresh_dashboard_only` already does its own don't-push-on-
    // failure and empty-sections handling.
    {
        let client = client.clone();
        let dashboard_sections = dashboard_sections.clone();
        let app_weak = app_weak.clone();
        tokio::spawn(async move {
            refresh_dashboard_only(&client, &dashboard_sections, &app_weak).await;
        });
    }

    // Fetching returns plain (Send) data -- `CalendarEvent`/`TodoItem` are
    // ordinary serde structs. Building the actual Slint models has to
    // happen below, *inside* `invoke_from_event_loop`: `ModelRc` is
    // `Rc`-based (not `Send`), so it can't be constructed on a tokio worker
    // thread and handed across into that closure.
    //
    // These three fetches are independent of each other, so they run
    // concurrently via `tokio::join!` rather than one after another as
    // this used to -- with a several-member family plus 3 weather calls,
    // that was a dozen-plus round trips stacked one after another.
    let (per_member_events, (per_member_todos, connection_alive), (weather, forecast_today, backfill)) = tokio::join!(
        fetch_calendar_events(rest, family, range_start, range_end),
        fetch_todos(client, family),
        fetch_weather(rest, client, primary_entity.as_deref(), backfill_entity.as_deref()),
    );

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
/// The 3 weather calls (current state, forecast, backfill state) as one
/// unit, run concurrently against each other via `tokio::join!` -- same
/// "independent fetches shouldn't be sequential" fix as the top-level
/// join in `refresh_calendar_and_todos`, which is this function's only
/// caller. `None` per-field on fetch failure or an unresolved entity, same
/// don't-flash-to-placeholder meaning as everywhere else weather is
/// handled.
async fn fetch_weather(
    rest: &RestClient,
    client: &Client,
    primary_entity: Option<&str>,
    backfill_entity: Option<&str>,
) -> (Option<EntityState>, Option<DailyForecast>, Option<EntityState>) {
    let weather = async {
        match primary_entity {
            Some(entity_id) => match rest.entity_state(entity_id).await {
                Ok(state) => Some(state),
                Err(err) => {
                    tracing::warn!(entity = %entity_id, %err, "failed to fetch weather entity state");
                    None
                }
            },
            None => None,
        }
    };
    // A day's high/low isn't a plain state attribute on modern HA weather
    // entities -- it needs its own service call (see
    // Client::weather_daily_forecast). WS, not REST: the forecast service
    // isn't exposed over the REST API.
    let forecast = async {
        match primary_entity {
            Some(entity_id) => match client.weather_daily_forecast(entity_id).await {
                Ok(days) => days.into_iter().next(),
                Err(err) => {
                    tracing::warn!(entity = %entity_id, %err, "failed to fetch weather forecast");
                    None
                }
            },
            None => None,
        }
    };
    let backfill = async {
        match backfill_entity {
            Some(entity_id) => match rest.entity_state(entity_id).await {
                Ok(state) => Some(state),
                Err(err) => {
                    tracing::warn!(entity = %entity_id, %err, "failed to fetch weather backfill entity state");
                    None
                }
            },
            None => None,
        }
    };
    tokio::join!(weather, forecast, backfill)
}

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
/// (see `fetch_todos`, and `Client::wait_closed` in the inner select) rather
/// than connecting exactly once for the life of the process --
/// `ha_client::Client` doesn't reconnect itself by design (its own doc
/// comment says so explicitly), so something has to.
///
/// No `local_offset` parameter: it used to take one, captured once at spawn
/// time, which on this RTC-less board meant a whole session's worth of
/// calendar range calculations could be pinned to whatever DST bucket was
/// current before NTP fixed the clock. It reads `local_offset()` fresh per
/// refresh instead.
#[allow(clippy::too_many_arguments)]
async fn run_ha_sync(
    config: Config,
    app_weak: slint::Weak<AppWindow>,
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

    // Built once, outside the reconnect loop, and cloned per iteration. It
    // used to be constructed fresh on every reconnect, which threw away the
    // underlying `reqwest::Client`'s connection pool and rustls
    // configuration each time -- i.e. it paid for a fresh TLS handshake (and
    // re-parsed the bundled webpki root store) precisely when the network
    // had just come back and things were already slow. Nothing about it
    // depends on the WS connection: `RestClient` is stateless over
    // base_url + token, both of which are fixed for the process, and
    // `reqwest::Client` is designed to be long-lived and shared. Cloning is
    // cheap -- it's an `Arc` internally, so all clones share the one pool.
    let rest = RestClient::new(&config.ha.base_url, &token);

    loop {
        let client = ha_client::connect_with_backoff(
            &config.ha.base_url,
            &token,
            std::time::Duration::from_secs(30),
        )
        .await;
        tracing::info!("connected to Home Assistant");
        *live_client.lock().unwrap() = Some(client.clone());
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
                // The real liveness signal. This resolves exactly when
                // ha-client's actor task has exited -- which now includes
                // "the keepalive ping went unanswered" (see `PING_INTERVAL`
                // in connection.rs), the case that previously left the app
                // sitting on stale data forever behind a silently-dropped
                // socket.
                //
                // The `RecvError::Closed` arm below was supposed to be this
                // check and structurally could not fire: a broadcast channel
                // only closes once *every* sender drops, and `live_client`
                // holds a `Client` clone (hence a sender) for the whole
                // session. It's kept anyway -- it costs nothing and is
                // correct if it ever does happen -- but it is no longer what
                // detects a dead connection.
                _ = client.wait_closed() => Wake::ConnectionDead,
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
                    // refreshing anyway is the safe default. (See the
                    // `wait_closed` arm above for why the `Closed` case below
                    // can't actually be relied on.)
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Wake::RelevantStateChange,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => Wake::ConnectionDead,
                },
            };
            if matches!(wake, Wake::Irrelevant) {
                continue;
            }
            if matches!(wake, Wake::ConnectionDead) {
                tracing::warn!("HA connection is dead, reconnecting");
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
                local_offset(),
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

/// One REST call per member (per calendar, for members with more than
/// one), all running concurrently via `JoinSet` rather than one after
/// another -- with a 6-member family this used to mean 6+ sequential
/// round trips before the calendar page could update at all, easily
/// stacking into seconds. Order in the result doesn't matter (downstream,
/// `build_calendar_grids` buckets everything by date using each tuple's
/// own `usize` member-index, not Vec position), unlike `fetch_todos`
/// below.
async fn fetch_calendar_events(
    rest: &RestClient,
    family: &[FamilyMember],
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Vec<(usize, slint::Color, Vec<CalendarEvent>)> {
    let mut set = tokio::task::JoinSet::new();
    for (index, member) in family.iter().enumerate() {
        if member.calendar_entities.is_empty() {
            continue;
        }
        let rest = rest.clone();
        let entities = member.calendar_entities.clone();
        let color = parse_hex_color(&member.color);
        set.spawn(async move {
            // A member can have more than one calendar linked; events from
            // all of them are merged and shown in this member's single
            // color -- nothing downstream needs to know which specific
            // calendar an event came from.
            let mut events = Vec::new();
            for entity in &entities {
                match rest.calendar_events(entity, start, end).await {
                    Ok(fetched) => events.extend(fetched),
                    Err(err) => {
                        tracing::warn!(entity = %entity, %err, "failed to fetch calendar events");
                    }
                }
            }
            (index, color, events)
        });
    }
    let mut out = Vec::new();
    while let Some(result) = set.join_next().await {
        if let Ok(item) = result {
            out.push(item);
        }
    }
    out
}

/// Same concurrency treatment as `fetch_calendar_events`, one WS call per
/// member. Unlike calendar events, order *does* matter here --
/// `build_todo_model` zips `family` against this result positionally --
/// so results go into a pre-sized `Vec` by index rather than however
/// `JoinSet` happens to complete them.
///
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
    let mut set = tokio::task::JoinSet::new();
    for (index, member) in family.iter().enumerate() {
        let client = client.clone();
        let entity = member.todo_entity.clone();
        set.spawn(async move {
            match entity {
                Some(entity) => match client.todo_items(&entity).await {
                    Ok(items) => (index, items, true),
                    Err(err) => {
                        let alive = !matches!(err, ha_client::connection::Error::Closed);
                        tracing::warn!(entity = %entity, %err, "failed to fetch todo items");
                        (index, Vec::new(), alive)
                    }
                },
                None => (index, Vec::new(), true),
            }
        });
    }
    let mut out: Vec<Vec<TodoItem>> = vec![Vec::new(); family.len()];
    let mut connection_alive = true;
    while let Some(result) = set.join_next().await {
        if let Ok((index, items, alive)) = result {
            out[index] = items;
            if !alive {
                connection_alive = false;
            }
        }
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

/// Flips one toggle-group entity's `is_on` (and recomputes its card's
/// `group_is_on`) directly in the model already on screen, so a tap shows
/// a response immediately rather than waiting on the round trip to HA and
/// back through `refresh_dashboard_only` -- on real touchscreen hardware
/// that round trip was visibly laggy even though HA itself applies the
/// change almost instantly. `refresh_dashboard_only` (already called
/// right after this in every caller) still runs and corrects this guess
/// if the actual `call_service` fails.
fn set_dashboard_entity_on_optimistically(app: &AppWindow, entity_id: &str, on: bool) {
    let rows = app.get_dashboard_rows();
    for row_idx in 0..rows.row_count() {
        let Some(row) = rows.row_data(row_idx) else { continue };
        let cards = row.cards;
        for card_idx in 0..cards.row_count() {
            let Some(mut card) = cards.row_data(card_idx) else { continue };
            if card.kind.as_str() != "toggle_group" {
                continue;
            }
            let entities = card.toggle_entities.clone();
            let mut touched = false;
            for i in 0..entities.row_count() {
                let Some(mut entity) = entities.row_data(i) else { continue };
                if entity.entity_id.as_str() == entity_id {
                    entity.is_on = on;
                    entities.set_row_data(i, entity);
                    touched = true;
                }
            }
            if touched {
                card.group_is_on =
                    (0..entities.row_count()).any(|i| entities.row_data(i).is_some_and(|e| e.is_on));
                cards.set_row_data(card_idx, card);
                return; // entity_ids are unique -- no need to keep scanning
            }
        }
    }
}

/// Same idea as `set_dashboard_entity_on_optimistically` but for the
/// section header switch -- flips every entity in whichever card contains
/// them.
fn set_dashboard_group_on_optimistically(app: &AppWindow, entity_ids: &[String], on: bool) {
    let rows = app.get_dashboard_rows();
    for row_idx in 0..rows.row_count() {
        let Some(row) = rows.row_data(row_idx) else { continue };
        let cards = row.cards;
        for card_idx in 0..cards.row_count() {
            let Some(mut card) = cards.row_data(card_idx) else { continue };
            if card.kind.as_str() != "toggle_group" {
                continue;
            }
            let entities = card.toggle_entities.clone();
            let mut touched = false;
            for i in 0..entities.row_count() {
                let Some(mut entity) = entities.row_data(i) else { continue };
                if entity_ids.iter().any(|id| id == entity.entity_id.as_str()) {
                    entity.is_on = on;
                    entities.set_row_data(i, entity);
                    touched = true;
                }
            }
            if touched {
                card.group_is_on = on;
                cards.set_row_data(card_idx, card);
            }
        }
    }
}

/// Same idea again, for a climate card's mode buttons.
fn set_dashboard_climate_mode_optimistically(app: &AppWindow, entity_id: &str, mode: &str) {
    let rows = app.get_dashboard_rows();
    for row_idx in 0..rows.row_count() {
        let Some(row) = rows.row_data(row_idx) else { continue };
        let cards = row.cards;
        for card_idx in 0..cards.row_count() {
            let Some(mut card) = cards.row_data(card_idx) else { continue };
            if card.kind.as_str() == "climate" && card.climate_entity_id.as_str() == entity_id {
                card.climate_mode = mode.into();
                // climate_current ("Cool · 71°") wasn't being touched here
                // before -- the mode button itself updated (it reads
                // climate_mode directly), but the status line above it
                // kept showing the old mode until the real refresh landed.
                // Keeps whatever temperature portion was already there
                // (that reading hasn't changed, only the mode has) by
                // splitting on the " · " build_dashboard_cards' Climate
                // arm always joins with.
                let temp_part = card.climate_current.as_str().split_once(" · ").map(|(_, t)| t.to_string());
                card.climate_current = match temp_part {
                    Some(temp) => format!("{} · {temp}", capitalize_first(mode)).into(),
                    None => capitalize_first(mode).into(),
                };
                cards.set_row_data(card_idx, card);
                return;
            }
        }
    }
}

/// Computes the new setpoint straight from what's already cached on the
/// card (see `climate-target-value`/`climate-min`/`climate-max` on
/// `DashboardCardData`), updates the displayed value immediately, and
/// hands back the computed number so the caller can send exactly that to
/// `climate.set_temperature` -- no REST round trip to read the current
/// setpoint back first. `None` if there's no matching card cached yet
/// (e.g. tapped before the first dashboard fetch has ever completed).
fn set_dashboard_climate_target_optimistically(app: &AppWindow, entity_id: &str, delta: i32) -> Option<f64> {
    let rows = app.get_dashboard_rows();
    for row_idx in 0..rows.row_count() {
        let Some(row) = rows.row_data(row_idx) else { continue };
        let cards = row.cards;
        for card_idx in 0..cards.row_count() {
            let Some(mut card) = cards.row_data(card_idx) else { continue };
            if card.kind.as_str() == "climate" && card.climate_entity_id.as_str() == entity_id {
                let current = card.climate_target_value as f64;
                let min = card.climate_min as f64;
                let max = card.climate_max as f64;
                let new_target = (current + delta as f64).clamp(min, max);
                card.climate_target_value = new_target as f32;
                card.climate_target = format_climate_temp(new_target).into();
                cards.set_row_data(card_idx, card);
                return Some(new_target);
            }
        }
    }
    None
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
                    climate_target_value: 0.0,
                    climate_min: 0.0,
                    climate_max: 0.0,
                    climate_mode: SharedString::default(),
                    climate_modes: empty_strings(),
                    sensor_rows: empty_sensor_rows(),
                })
            }
            DashboardSection::Climate { entity } => {
                let state = *states.get(entity.as_str())?;
                let current = state.attributes.get("current_temperature").and_then(|v| v.as_f64());
                let target = state.attributes.get("temperature").and_then(|v| v.as_f64());
                let min_temp = state.attributes.get("min_temp").and_then(|v| v.as_f64()).unwrap_or(f64::MIN);
                let max_temp = state.attributes.get("max_temp").and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
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
                    // Falls back to min_temp (never NaN/garbage) when the
                    // entity has no active setpoint (e.g. an AC that's
                    // currently off) -- a +/- tap in that state is an edge
                    // case HA itself may just reject, but this keeps the
                    // cached value sane either way.
                    climate_target_value: target.unwrap_or(min_temp) as f32,
                    climate_min: min_temp as f32,
                    climate_max: max_temp as f32,
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
                    climate_target_value: 0.0,
                    climate_min: 0.0,
                    climate_max: 0.0,
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

    /// A miniature tz database shaped like the real `zone1970.tab`:
    /// the US has several zones, Britain exactly one.
    fn tz_catalog() -> timezone::Catalog {
        timezone::parse(
            "US\t+404251-0740023\tAmerica/New_York\tEastern (most areas)\n\
             US\t+415100-0873900\tAmerica/Chicago\tCentral (most areas)\n\
             US\t+394606-0860929\tAmerica/Indiana/Indianapolis\tEastern - IN (most areas)\n\
             GB\t+513030-0000731\tEurope/London\n",
            "GB\tBritain (UK)\nUS\tUnited States\n",
        )
    }

    #[test]
    fn tapping_a_region_descends_to_its_countries() {
        let next = tz_level_after_selection(&tz_catalog(), &TzLevel::Continent, "America");
        assert_eq!(
            next,
            TzSelection::Descend(TzLevel::Country { continent: "America".into() })
        );
    }

    /// The point of the shortcut: Britain has one zone, so picking the country
    /// has already picked the zone. Making the user confirm a one-item list
    /// would be a pure extra tap.
    #[test]
    fn a_country_with_a_single_zone_is_chosen_without_another_tap() {
        let next = tz_level_after_selection(
            &tz_catalog(),
            &TzLevel::Country { continent: "Europe".into() },
            "GB",
        );
        assert_eq!(
            next,
            TzSelection::Choose { zone: "Europe/London".into(), label: "London".into() }
        );
    }

    #[test]
    fn a_country_with_several_zones_descends_to_them() {
        let next = tz_level_after_selection(
            &tz_catalog(),
            &TzLevel::Country { continent: "America".into() },
            "US",
        );
        assert_eq!(
            next,
            TzSelection::Descend(TzLevel::Zone {
                continent: "America".into(),
                country_code: "US".into(),
                country_name: "United States".into(),
            })
        );
    }

    #[test]
    fn tapping_a_zone_chooses_it() {
        let level = TzLevel::Zone {
            continent: "America".into(),
            country_code: "US".into(),
            country_name: "United States".into(),
        };
        let next = tz_level_after_selection(&tz_catalog(), &level, "America/New_York");
        assert_eq!(
            next,
            TzSelection::Choose {
                zone: "America/New_York".into(),
                // The label follows the tap, so the restart message names the
                // same thing the row did.
                label: "Eastern - New York".into(),
            }
        );
    }

    #[test]
    fn back_goes_up_one_level_and_the_top_level_is_its_own_parent() {
        let zone = TzLevel::Zone {
            continent: "America".into(),
            country_code: "US".into(),
            country_name: "United States".into(),
        };
        let country = tz_level_after_back(&zone);
        assert_eq!(country, TzLevel::Country { continent: "America".into() });
        assert_eq!(tz_level_after_back(&country), TzLevel::Continent);
        // A stray Back at the root shouldn't close the picker or panic.
        assert_eq!(tz_level_after_back(&TzLevel::Continent), TzLevel::Continent);
    }

    /// Rows and their payloads must stay index-aligned -- the callback looks
    /// the tapped row's payload up positionally.
    #[test]
    fn row_labels_and_payloads_line_up_at_every_level() {
        let catalog = tz_catalog();
        let (labels, payloads) = tz_level_rows(&catalog, &TzLevel::Continent);
        assert_eq!(labels, payloads, "regions select themselves");
        assert_eq!(labels, vec!["America", "Europe"]);

        let (labels, payloads) =
            tz_level_rows(&catalog, &TzLevel::Country { continent: "America".into() });
        assert_eq!(labels, vec!["United States"]);
        assert_eq!(payloads, vec!["US"], "the country row selects its code, not its name");

        let (labels, payloads) = tz_level_rows(
            &catalog,
            &TzLevel::Zone {
                continent: "America".into(),
                country_code: "US".into(),
                country_name: "United States".into(),
            },
        );
        // The extra Indiana row in the fixture collapses into Eastern rather
        // than becoming a second, indistinguishable "Eastern" choice.
        assert_eq!(labels, vec!["Central - Chicago", "Eastern - New York"]);
        assert_eq!(payloads, vec!["America/Chicago", "America/New_York"]);
    }

    fn weather_state(state: &str, attributes: serde_json::Value) -> EntityState {
        EntityState { entity_id: "weather.home".into(), state: state.into(), attributes, last_updated: None }
    }

    /// A path under the OS temp dir, unique per test run via the PID plus a
    /// caller-given tag -- avoids collisions between tests that both
    /// exercise the pin-hash file without needing a test-only tempfile
    /// crate dependency.
    fn temp_pin_path(tag: &str) -> String {
        std::env::temp_dir().join(format!("skylight-ha-test-pin-{}-{tag}.secret", std::process::id())).display().to_string()
    }

    fn date(y: i32, m: Month, d: u8) -> Date {
        Date::from_calendar_date(y, m, d).unwrap()
    }

    #[test]
    fn follows_an_ordinary_midnight_rollover() {
        let yesterday = date(2026, Month::September, 25);
        let today = date(2026, Month::September, 26);
        assert_eq!(
            reference_date_after_today_changed(yesterday, yesterday, today),
            Some(today),
            "the view was on 'today', so it should move with it"
        );
    }

    #[test]
    fn follows_the_ntp_correction_off_the_kernel_epoch() {
        // The real-hardware case: no RTC, so the process starts in 1970 and
        // seeds both `reference_date` and the clock tick's `last_today` with
        // it. Once S45ntp lands, the grid has to stop showing January 1970
        // without anyone tapping "Today".
        let epoch = date(1970, Month::January, 1);
        let real = date(2026, Month::September, 26);
        assert_eq!(
            reference_date_after_today_changed(epoch, epoch, real),
            Some(real)
        );
    }

    #[test]
    fn does_not_yank_the_view_the_user_navigated_to() {
        let yesterday = date(2026, Month::September, 25);
        let today = date(2026, Month::September, 26);
        let browsing = date(2026, Month::December, 24);
        assert_eq!(
            reference_date_after_today_changed(browsing, yesterday, today),
            None,
            "deliberately paged-to dates must survive a midnight rollover"
        );
    }

    #[test]
    fn does_nothing_when_the_day_has_not_changed() {
        let today = date(2026, Month::September, 26);
        assert_eq!(reference_date_after_today_changed(today, today, today), None);
        // Also the case where the user navigated *and* nothing changed.
        let elsewhere = date(2026, Month::October, 1);
        assert_eq!(
            reference_date_after_today_changed(elsewhere, today, today),
            None
        );
    }

    /// Guards the mechanism itself, not a specific zone: whatever
    /// `localtime_r` reports has to be a legal `UtcOffset` and has to agree
    /// with what `local_offset()` publishes after a refresh.
    #[test]
    fn derives_and_publishes_a_plausible_local_offset() {
        let refreshed = refresh_local_offset().expect("localtime_r should work on the test host");
        assert_eq!(refreshed, local_offset(), "refresh must publish what it returns");
        let seconds = refreshed.whole_seconds();
        assert!(
            (-26 * 3600..=26 * 3600).contains(&seconds),
            "offset out of the range any real timezone can have: {seconds}s"
        );
    }

    /// The DST-bucket half of the bug: the offset must be derived *for a
    /// given instant*, not once for the life of the process. Checked against
    /// a fixed zone so it holds regardless of the test host's own timezone.
    #[test]
    fn offset_is_instant_dependent_across_a_dst_boundary() {
        // Safe here: `set_var` is only unsound with concurrent readers, and
        // Rust's test harness runs each `#[test]` on its own thread but this
        // is the only test that touches TZ -- and it restores it immediately.
        // (If this ever becomes flaky, mark it `#[ignore]`; it documents the
        // property either way.)
        let original = std::env::var("TZ").ok();
        std::env::set_var("TZ", "America/New_York");

        // 2026-01-15 12:00Z -> EST (-5), 2026-07-15 12:00Z -> EDT (-4).
        let winter = date(2026, Month::January, 15).midnight().assume_utc();
        let summer = date(2026, Month::July, 15).midnight().assume_utc();
        let winter_offset = system_utc_offset(winter);
        let summer_offset = system_utc_offset(summer);

        match original {
            Some(tz) => std::env::set_var("TZ", tz),
            None => std::env::remove_var("TZ"),
        }

        let (w, s) = match (winter_offset, summer_offset) {
            (Some(w), Some(s)) => (w, s),
            // No TZ database on the build host -- nothing to assert.
            _ => return,
        };
        assert_ne!(
            w, s,
            "the same lookup must give different offsets either side of a DST transition; \
             getting the same one back is exactly the frozen-offset bug"
        );
        assert_eq!(w.whole_hours(), -5, "January in America/New_York is EST");
        assert_eq!(s.whole_hours(), -4, "July in America/New_York is EDT");
    }

    #[test]
    fn hashes_pin_deterministically_and_distinguishes_different_pins() {
        assert_eq!(hash_pin("1234"), hash_pin("1234"));
        assert_ne!(hash_pin("1234"), hash_pin("4321"));
        // Not stored in plaintext -- the hash shouldn't just be the PIN.
        assert_ne!(hash_pin("1234"), "1234");
    }

    #[test]
    fn saves_loads_and_removes_pin_hash_file() {
        let path = temp_pin_path("roundtrip");
        let _ = std::fs::remove_file(&path); // in case a previous run left it behind

        assert_eq!(load_pin_hash(&path), None, "no file yet -- no PIN configured");

        save_pin_hash(&path, "1234").unwrap();
        let loaded = load_pin_hash(&path).expect("hash should load after saving");
        assert_eq!(loaded, hash_pin("1234"));

        remove_pin_hash(&path).unwrap();
        assert_eq!(load_pin_hash(&path), None, "removed -- no PIN configured again");
        // Removing an already-absent file is not an error (Settings'
        // "Turn Off" flow shouldn't fail if the file is somehow already gone).
        assert!(remove_pin_hash(&path).is_ok());
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

    /// The Software Update card's single status line. Six mutually exclusive
    /// situations fold into it, which is exactly why the decision lives in
    /// Rust rather than in `.slint` expressions -- and why it's worth pinning
    /// down here.
    #[test]
    fn describes_every_update_state() {
        let never = update::State::default();
        assert_eq!(update_status_line(&never, false), "Not checked yet.");
        assert_eq!(
            update_status_line(&never, true),
            "Checking for updates...",
            "busy wins over everything else"
        );

        let failed = update::State {
            last_check_epoch: Some(1_760_000_000),
            last_check_error: Some("could not reach GitHub".into()),
            ..update::State::default()
        };
        assert_eq!(update_status_line(&failed, false), "Last check failed: could not reach GitHub");

        let current = update::State {
            last_check_epoch: Some(1_760_000_000),
            latest_version: Some("0.1.0".into()),
            ..update::State::default()
        };
        assert!(update_status_line(&current, false).starts_with("Up to date. Last checked "));

        let available = update::State {
            last_check_epoch: Some(1_760_000_000),
            latest_version: Some("0.9.0".into()),
            available: true,
            ..update::State::default()
        };
        assert_eq!(update_status_line(&available, false), "Version 0.9.0 is available.");

        let reflash = update::State { requires_reflash: true, ..available.clone() };
        assert!(
            update_status_line(&reflash, false).contains("manual SD-card reflash"),
            "a reflash-only release has to say so, since no Install button appears"
        );
        assert!(!reflash.installable());

        let blocked = update::State { available: false, blocked: true, ..available.clone() };
        assert!(update_status_line(&blocked, false).contains("failed to start"));
        assert!(!blocked.installable());
    }

    /// A stored check timestamp renders in local time rather than UTC, using
    /// the same self-correcting offset as the clock.
    #[test]
    fn formats_a_check_timestamp_in_local_time() {
        let edt = UtcOffset::from_whole_seconds(-4 * 3600).unwrap();
        // 2025-10-09T08:53:20Z -> 04:53 EDT.
        assert_eq!(format_check_time(1_760_000_000, edt), "2025-10-09 4:53 AM");
        assert_eq!(format_check_time(1_760_000_000, UtcOffset::UTC), "2025-10-09 8:53 AM");
        assert_eq!(format_check_time(u64::MAX, edt), format!("at {}", u64::MAX));
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
        // Cached so a +/- tap can compute the new setpoint without a REST
        // round trip to read it back first -- see
        // set_dashboard_climate_target_optimistically.
        assert_eq!(card.climate_target_value, 75.0);
        assert_eq!(card.climate_min, 50.0);
        assert_eq!(card.climate_max, 99.0);
    }

    #[test]
    fn climate_card_falls_back_to_min_temp_when_no_active_setpoint() {
        // The exact shape confirmed live for the Sunroom AC while off --
        // no "temperature" key at all (not even null), unlike the other
        // thermostats which always have one.
        let states = [entity_state(
            "climate.window_ac",
            "off",
            serde_json::json!({
                "friendly_name": "Sunroom AC",
                "hvac_modes": ["off", "cool"],
                "current_temperature": 71,
                "min_temp": 45,
                "max_temp": 95,
            }),
        )];
        let by_id: std::collections::HashMap<&str, &EntityState> =
            states.iter().map(|s| (s.entity_id.as_str(), s)).collect();
        let sections = vec![DashboardSection::Climate { entity: "climate.window_ac".into() }];

        let cards = build_dashboard_cards(&sections, &by_id);
        let card = &cards[0];
        assert_eq!(card.climate_target, "", "no active setpoint -- nothing to show in the stepper");
        assert_eq!(card.climate_target_value, 45.0, "falls back to min_temp, not 0 or NaN");
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
            climate_target_value: 0.0,
            climate_min: 0.0,
            climate_max: 0.0,
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
