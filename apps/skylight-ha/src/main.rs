use dashboard_config::Config;
use slint::ComponentHandle;
use time::{OffsetDateTime, UtcOffset};
use ui::{AppWindow, CalendarDayData, TodoColumnData, TodoItemData};

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
    app.set_calendar_weeks(build_month_weeks(local_offset));
    app.set_todo_columns(build_empty_todo_columns(&config));

    app.on_todo_item_toggled(move |col, item| {
        // Wired up to ha_client::Client::todo_update_item once the HA sync
        // task (below) hands the UI a live client handle.
        tracing::debug!(col, item, "todo item toggled (not yet wired to HA)");
    });

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
                app.set_month_label(format!("{}", now.month()).into());
            }
        },
    );

    let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
    let _guard = rt.enter();
    rt.spawn(run_ha_sync(config));

    app.run().expect("event loop error");
}

/// Connects to HA and keeps state flowing. Phase 1 scope: just proves the
/// connection comes up and logs it. Phase 2 pushes states/events into the UI
/// via `slint::invoke_from_event_loop`.
async fn run_ha_sync(config: Config) {
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
    let _ = client;
}

/// Builds a real Sun-start month grid for "today" with no events yet (HA
/// wiring lands in a later phase) — a real calendar shape, just not yet
/// populated with data from Home Assistant.
fn build_month_weeks(local_offset: UtcOffset) -> slint::ModelRc<slint::ModelRc<CalendarDayData>> {
    let today = OffsetDateTime::now_utc().to_offset(local_offset).date();
    let first_of_month = today.replace_day(1).expect("day 1 is always valid");
    let lead_days = first_of_month.weekday().number_days_from_sunday();
    let mut cursor = first_of_month - time::Duration::days(lead_days as i64);

    let mut weeks = Vec::with_capacity(6);
    for _ in 0..6 {
        let mut week = Vec::with_capacity(7);
        for _ in 0..7 {
            week.push(CalendarDayData {
                day_number: cursor.day() as i32,
                in_current_month: cursor.month() == today.month(),
                is_today: cursor == today,
                events: slint::ModelRc::default(),
            });
            cursor += time::Duration::days(1);
        }
        weeks.push(slint::ModelRc::new(slint::VecModel::from(week)));
    }
    slint::ModelRc::new(slint::VecModel::from(weeks))
}

/// One empty todo column per configured family member, so the roster is
/// visible immediately even before HA returns any real todo items.
fn build_empty_todo_columns(config: &Config) -> slint::ModelRc<TodoColumnData> {
    let columns = config
        .family
        .iter()
        .map(|member| TodoColumnData {
            member_name: member.name.clone().into(),
            member_color: parse_hex_color(&member.color),
            items: slint::ModelRc::new(slint::VecModel::from(Vec::<TodoItemData>::new())),
        })
        .collect::<Vec<_>>();
    slint::ModelRc::new(slint::VecModel::from(columns))
}

fn parse_hex_color(hex: &str) -> slint::Color {
    let hex = hex.trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16).unwrap_or(0x6c8dfa);
    let [_, r, g, b] = value.to_be_bytes();
    slint::Color::from_rgb_u8(r, g, b)
}
