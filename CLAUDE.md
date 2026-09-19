# Skylight HA — project notes

A family-calendar-and-tasks Home Assistant dashboard, built as a Rust/Slint
binary that runs directly on bare framebuffer/DRM-KMS (no X11/Wayland) on a
Raspberry Pi Zero 2 W, with a custom minimal Buildroot distro underneath it.

Full architecture/phasing plan: `docs/plan.md`.

## One machine now (2026-09-08 consolidation)

Development is consolidated onto a **single Pop!_OS 24.04 machine**. It used
to be split (a Windows machine for app dev, this one for Buildroot); that
Windows machine is retired. On this box:

- App repo: `/home/me/Claude/Skylight` (remote
  `https://github.com/siesta5787/skylight-ha.git`, branch `master`). Both
  `backend-winit` desktop iteration and `backend-linuxkms` native touch
  testing happen here.
- Buildroot source tree: `~/buildroot` — a **separate** checkout, NOT this
  repo, still used only for the Pi OS image (see Buildroot section below).
- `config.toml` and `ha-token.secret` are both `.gitignore`d — recreate on
  a fresh clone (`cp config.example.toml config.toml`, fill in real values;
  `ha-token.secret` is a single line with the token). The `config.toml`
  here currently has placeholder HA values and no real token → runs with
  placeholder data, which is fine for rendering/touch work.
- Getting the built binary onto the Pi is `scp` over the LAN. **Don't route
  `ha-token.secret` through any cloud sync** — it's a genuine HA long-lived
  access token.

### Environment setup already done on this machine

rustup stable toolchain; `gh` in `~/.local/bin` (authed as `siesta5787`);
apt packages `libinput-dev libudev-dev libxkbcommon-dev libxkbcommon-x11-dev
libgbm-dev libdrm-dev libseat-dev libxcb1-dev libfontconfig-dev cmake
evtest`; user `me` added to the `input` and `video` groups (needed for
`backend-linuxkms-noseat` to open `/dev/input/*` and `/dev/dri/*` directly
without root — takes effect after a full logout/login, else run the binary
with `sudo`).

## Dev loop — pick the right one for what you're testing

1. **`backend-winit` desktop (fastest, default choice)**: `cargo run -p
   skylight-ha -- config.toml` opens a normal window on the Pop!_OS
   desktop. Use this for UI layout, calendar rendering, HA WebSocket/REST
   logic, config schema, todo interactions — anything that isn't
   specifically about touch input or the display driver. Seconds of
   iteration instead of hours. (This is the default feature set; no extra
   flags needed.)
2. **`backend-linuxkms` native on this machine**: `cargo build --release -p
   skylight-ha --no-default-features -F ui/backend-linuxkms`, then run
   `./target/release/skylight-ha config.toml` from a raw VT (`Ctrl+Alt+F3`,
   log in at the text console — KMS needs exclusive DRM master, and the
   desktop Wayland compositor on tty1 holds it otherwise). `Ctrl+Alt+F1`
   switches back. Same evdev/libinput code path as the real Pi, no
   cross-compilation, no Buildroot, no SD card flashing. **This is the
   right tool for touch/display-driver bugs** — `backend-winit` cannot
   reproduce them at all, since it's a completely different input pipeline
   (winit windowing/mouse events vs. raw evdev + libinput + DRM). The
   USB touchscreen on this machine enumerates as `/dev/input/event13`
   ("Jieli Technology USB Composite Device", MT-B, `INPUT_PROP_DIRECT`);
   `event14` is its stylus interface.
3. **Real Pi hardware** — only needed for final validation once both of the
   above look right, or for anything genuinely Pi-specific (WiFi chip
   firmware, the actual touchscreen's real-world behavior, boot process).

## Building the Pi binary (CI)

- `aarch64-unknown-linux-musl`, built via `.github/workflows/build-pi.yml`
  on GitHub Actions (QEMU-emulated aarch64 Alpine container).
- **Not fully static.** `crt-static` is disabled for this target — Alpine
  doesn't ship static builds of `libinput`/`libudev`/`libxkbcommon`, so the
  binary dynamically links musl's own libc plus those three small
  hardware-input libraries. This was a deliberate, deliberate-tradeoff
  decision (see Feedback below), not an accident.
- Runtime deps the target distro must provide: musl's loader
  (`ld-musl-aarch64.so.1`), `libinput.so`, `libudev.so`(via `eudev`),
  `libxkbcommon.so`, udev hwdb data (for touchscreen identification), xkb
  keymap data (`xkeyboard-config`), and root permission to open
  `/dev/dri/*` directly (`backend-linuxkms-noseat`, no seatd broker).
- **Each CI run takes ~2-3.5h regardless of caching** — this is documented
  from repeated real runs, don't expect a "just rebuilt recently" run to be
  faster.
- **Commit and push before triggering the workflow.** `gh workflow run
  build-pi.yml` builds from whatever's on the GitHub remote, not local
  files — triggering it with uncommitted/unpushed local changes wastes a
  full run rebuilding the old code (this happened once, cost 2h21m for
  nothing).
- Download the artifact: `gh run download <run-id> -n
  skylight-ha-aarch64-linux-musl -D dist`. Compare file size/BuildID against
  the previous artifact as a sanity check that the new build actually
  differs when you expect it to.

## App features built since the initial calendar/tasks MVP

Quick orientation for a fresh session -- not a full history (see `git log`
for that), just what exists, where, and the non-obvious facts.

**Weather widget** (top bar, single compact line pinned against the
clock): `crates/ui/ui/top-bar.slint`'s `WeatherWidget` +
`crates/ui/ui/weather-icon.slint`. Config: top-level (not nested under
`[ha]`) `weather_entity` (condition/temp/wind/forecast) and
`weather_backfill_entity` (humidity/pressure, only consulted if the
primary doesn't report them) -- both optional, auto-discovered from HA's
`weather.*` entities if unset. **Must be placed before the first `[table]`
header in `config.toml`** -- TOML has no table-reset until the next
header, so a bare `key = value` after `[ha]` silently becomes
`ha.weather_entity` instead (this bit once already). On this instance,
`weather.home` (the integration actually added) has no humidity/pressure
attributes at all; `weather.forecast_home` (HA's default Met.no forecast)
does -- hence the primary/backfill split. Today's high/low needs the
`weather.get_forecasts` service with `return_response: true` (see
`Client::weather_daily_forecast` in ha-client) -- it isn't a plain state
attribute on modern HA weather entities. No percentage-chance-of-rain
field exists on either entity here; what's shown is a precipitation
*amount* (inches), matching what HA's own more-info dialog shows.

**Dashboard page** (lights/fans/climate/sensor cards): two-phase plan.
- *Phase 1 (done)*: config-driven via `dashboard-config::DashboardSection`
  (`ToggleGroup`/`Climate`/`SensorGroup`), set with `[[dashboard]]` blocks
  in `config.toml`, in render order. Consecutive same-`kind` blocks share
  a horizontal row (`group_dashboard_rows` in main.rs) -- reorder
  `config.toml` to control what sits next to what (e.g. Lights
  immediately followed by Fans -> side by side).
- *Phase 2 (not started)*: extend the `skylight-family` HA integration
  (github.com/siesta5787/skylight-family, sibling repo, separate
  Python/HACS deploy) with a new "Dashboard Section" subentry type -- same
  `ConfigSubentryFlow` pattern it already uses for family members
  (confirmed by reading its `config_flow.py`/`sensor.py`), exposing
  `sensor.skylight_dashboard_*` entities. `discover_dashboard_sections` in
  main.rs already expects that exact shape and returns `None` until it
  exists -- no Rust/Slint rework needed when Phase 2 lands.
- Controls are on/off only for lights/fans (no brightness/speed -- out of
  scope, matches the reference Lovelace screenshots this was modeled on),
  climate mode buttons (only for whichever of off/fan_only/cool/heat the
  entity's own `hvac_modes` actually supports) plus a +/- temperature
  stepper. All writes go through `Client::call_service` (ha-client/
  connection.rs), a generic domain/service/entity_ids/data wrapper added
  because dashboard controls needed it four different ways.
- Every dashboard control updates the on-screen model *optimistically*
  (synchronously, the instant the tap fires -- `set_dashboard_entity_on_
  optimistically`/`_group_on_optimistically`/`_climate_mode_
  optimistically`/`_climate_target_optimistically` in main.rs) before the
  network call even starts; the real `call_service` + a
  `refresh_dashboard_only` afterward reconciles if the guess was wrong.

**Parental PIN lock** (Settings page): fully local/on-device, no HA
involved. 4-digit PIN, SHA-256 hashed (not plaintext -- proportionate to
the actual threat model of "a curious kid", not real auth) into
`config.toml`'s `pin_hash_path` (default `pin.secret`, already covered by
`.gitignore`'s `*.secret`). "Is a PIN configured" == "does that file
exist" -- no separate flag that could drift out of sync with it. Gates the
Dashboard/Settings nav buttons; **re-locks the moment you navigate away**
to Calendar/Tasks/Photos (confirmed-with-user behavior) -- moving
*between* Dashboard and Settings themselves does not re-lock, one PIN
entry covers "being in that area". One `PinFlow` state machine in main.rs
(`pin_flow: Rc<RefCell<Option<PinFlow>>>`) drives every setup/change/
disable/unlock flow, paired with a dedicated numeric `PinPad` component
(`crates/ui/ui/pin-pad.slint`) -- not the general `VirtualKeyboard`.

## Known app-level bugs (fixed or open)

- **Fixed — font panic on the real device**: Slint's software renderer
  needs a font, and the target has zero system fonts/fontconfig by design.
  It panics (`i-slint-renderer-software/fonts/systemfonts.rs`, `unwrap()`
  on `None`) instead of erroring gracefully. Fix is **not**
  `slint::register_font_from_memory` — that free function doesn't exist in
  Slint 1.17.1 (the text stack moved to a `fontique`-based system that
  needs a live window/renderer handle, not callable from early `main()`).
  The actual fix, and Slint's own recommended approach, is compile-time
  embedding in `.slint` markup: `import
  "../assets/fonts/Inter-Regular.ttf";` at the top of `app-window.slint`,
  plus `default-font-family: "Inter";` on the root `Window`. Font lives at
  `crates/ui/assets/fonts/Inter-Regular.ttf` (OFL-licensed, from Google's
  font repo).
- **Fatal vs. non-fatal config loading**: `Config::load()` (parsing
  `config.toml` itself) happens synchronously before the window is created
  — failure there `exit(1)`s immediately, no UI at all. The HA token load
  (`ha-token.secret`) happens later in an async task and only logs an error
  on failure ("dashboard will show placeholder data only") — it does not
  crash. So `config.toml` must exist and parse to test anything, but the
  real token isn't needed just to validate rendering/touch.
- Both `config.toml`'s own default path and `ha.token_path` inside it are
  relative to the process's **current working directory**, not to the
  config file's own location. `S99skylight` passes `/etc/skylight/config.toml`
  explicitly as an argument to sidestep CWD ambiguity — keep
  `token_path` as an absolute path in `config.toml` for the same reason.
- **Resolved (2026-09-10/11) — "touch not registering" was a misdiagnosis,
  not an input-pipeline bug.** The original symptom (touchscreen sends
  valid evdev events, app opens the device node, but nothing reacts to
  touch) was reproduced locally via `backend-linuxkms` on this machine with
  the same USB touchscreen and root-caused by vendoring Slint's linuxkms
  backend with logging added at every stage (libinput device enumeration,
  raw→transformed touch coordinates, `process_touch_input` return value).
  Every stage checked out: `libinput` reports `cap_touch=true` for the
  panel, coordinates transform correctly from the device's native `0–4096`
  range onto the `1920x1080` render surface (the "coordinate-calibration
  mismatch" hypothesis was wrong — the mapping is exact), and
  `process_touch_input` returns `accepted=Some(true)` with the click
  callback firing. **The actual cause: the UI had nothing tappable where
  anyone was touching.** The only `TouchArea` in the whole app was on
  per-todo-item rows (`todo-card.slint`), and with no `ha-token.secret`
  the todo columns render with zero items — so every tap landed on dead
  space (calendar cells, clock, headers had no `TouchArea` at all). Fix:
  calendar day cells now have their own `TouchArea` (`calendar-card.slint`
  `DayCell`) wired through `CalendarCard.day-selected(week, day)` to
  `AppWindow.calendar-day-selected` to `main.rs`'s
  `on_calendar_day_selected`, confirmed working end-to-end via `evtest` +
  a vendored/instrumented Slint backend + a temporary on-screen tap-counter
  probe (the tap-counter box was later removed along with the rest of that
  debug scaffolding once the full sidebar/calendar/dashboard redesign gave
  the app plenty of real tappable surface -- confirmed working via touch
  on real hardware multiple times since via `backend-linuxkms` on this dev
  machine's USB touchscreen; not yet validated on the actual Pi).
- **`backend-linuxkms-noseat` doesn't cooperate with VT switching** --
  confirmed live: with the app running from a raw VT (Dev loop #2 above),
  Ctrl+Alt+F1 does *nothing* (not frozen -- the app keeps running fine,
  the switch request just silently stalls forever). Without a seatd/
  logind seat manager, a VT switch needs the process holding the VT to
  catch `SIGUSR1` and acknowledge release via `VT_RELDISP`; `noseat` mode
  doesn't implement that handshake, so the kernel's switch request has
  nothing to acknowledge it. **Not an issue on the real Pi** (no desktop
  session to switch back to there -- purely a dev-machine wrinkle). Here,
  the only reliable recovery is killing the process from elsewhere (`ps
  aux | grep skylight-ha`, then `kill <pid>` -- from SSH or another
  terminal, since the stuck console's own keyboard input isn't reaching
  anything useful either). After killing it, the display may stay black
  until the compositor notices and repaints -- rule out plain monitor
  power-save first (move the mouse/press a key), then try cycling VTs
  (Ctrl+Alt+F2 then Ctrl+Alt+F1) if it's still blank. `timeout <seconds>
  ./target/release/skylight-ha config.toml` is the simple preventative --
  auto-kills itself, nothing to remember mid-test.
- **Fixed -- PIN pad crashed on the 4th digit ("RefCell already
  borrowed")**: `match some_refcell.borrow_mut().take() { ... }` keeps the
  `RefMut` alive for the *entire* match (Rust extends a scrutinee's
  temporaries across all its arms) -- any arm that also borrowed the same
  `RefCell` (nearly all of them, to advance a multi-step flow) panicked
  immediately. Same class of bug as the `MutexGuard`-in-`if let` issue
  above (`fetch_calendar_events`'s history), just `RefCell`/`match`
  instead of `Mutex`/`if let` -- same fix: bind the taken value to a `let`
  first so the guard drops before the match arms run.
- **Fixed -- refresh felt slow / "doesn't update"**: `get_states()` turned
  out to be wildly variable in practice on the real instance -- measured
  live at ~460ms once, 17.5s (538 entities) another time, 30s+ (didn't
  finish within that) a third time. The old `refresh_calendar_and_todos`
  joined calendar+todos+weather+dashboard into one wait, so the *whole*
  refresh (including the calendar grid, which has nothing to do with
  `get_states()`) sat blocked behind whichever fetch was slowest that
  time. Fixed two ways: `fetch_calendar_events`/`fetch_todos` now fan out
  one task per family member via `tokio::task::JoinSet` instead of
  awaiting sequentially (todos preserves family-index order via a
  pre-sized `Vec`, since `build_todo_model` zips positionally; calendar
  events don't care about order); and the dashboard's `get_states()` fetch
  is no longer joined with calendar/todo/weather at all -- it's a fully
  detached `tokio::spawn` (`refresh_dashboard_only`) so a slow one can't
  block the others, or vice versa. **`get_states()` itself being slow is
  HA-instance-side**, not fixable in this app's code -- 538 entities is a
  lot; recorder/database load, an unhealthy integration, or general
  system load are the usual suspects if it recurs.

## Buildroot / OS image (on the Pop!_OS machine, `~/buildroot`)

- Based on `raspberrypizero2w_64_defconfig`.
- **Toolchain must be set to Buildroot-internal + musl C library before the
  first build**, not layered on after — switching later costs a
  near-full rebuild (this happened once: built the stock glibc defconfig
  first, did ~10 more menuconfig sessions, only then discovered the
  binary couldn't exec at all because the loader/libc flavor didn't
  match).
- `System configuration → /dev management` must be **eudev**, not `mdev` —
  `mdev` can't provide `libudev`, which `libinput` needs
  (`BR2_ROOTFS_DEVICE_CREATION_DYNAMIC_EUDEV=y`).
- `Toolchain → Kernel Headers` should be "Same as kernel being built"
  (`BR2_KERNEL_HEADERS_AS_KERNEL=y`) — avoids a headers-version mismatch
  entirely rather than picking a specific series by hand.
- Target packages actually needed: `eudev` (with hwdb install on),
  `libinput`, `libxkbcommon`, `xkeyboard-config` (confirmed via Buildroot's
  own `Config.in` that this has no real dependency on Xorg despite being
  menu-grouped under "X libraries" — safe to enable without pulling in an X
  server), `wpa_supplicant` (+ `nl80211` driver, + `ctrl_interface` +
  `wpa_cli`, WPA3 not currently needed), `brcmfmac_sdio-firmware-rpi` (WIFI
  suboption only — the Zero 2 W's chip is `BCM43430/1`, not the `43436`
  variant some forum threads warn about), `dropbear` (temporary SSH),
  `iw` (needed at runtime for the WiFi power-save workaround, not just
  debug), `evtest` (debug), `rpi-firmware`.
- USB Ethernet dongle chipset is **RTL8152/8153** — needs the dedicated
  `CONFIG_USB_RTL8152` kernel driver (Realtek uses a vendor-specific
  protocol, not generic CDC-ECM, so the generic driver won't work for this
  one).
- Overlay directory: `board/skylight/rootfs-overlay/` (an arbitrary name
  chosen for this project, not a Buildroot-recognized board — had to be
  manually `mkdir -p`'d, doesn't pre-exist). Set via System configuration →
  Root filesystem overlay directories. Contents so far:
  - `etc/wpa_supplicant.conf`
  - `etc/init.d/S39ethernet` — brings up `eth0` via `udhcpc`
  - `etc/init.d/S40wifi` — brings up `wlan0`: link up, **`iw dev wlan0 set
    power_save off`** (brcmfmac on this chip drops connections repeatedly
    without this — confirmed root cause, not a hypothesis), `wpa_supplicant`,
    `udhcpc`
  - `etc/init.d/S99skylight` — execs `/usr/bin/skylight-ha
    /etc/skylight/config.toml` via `start-stop-daemon`
  - `usr/bin/skylight-ha` — the app binary
  - `etc/skylight/config.toml` and `etc/skylight/ha-token.secret` — both
    now baked into the overlay permanently (this used to say "still
    needed, only tested via live scp" — that's done).
- **A full image has been built successfully**: `~/buildroot/output/
  images/` has `sdcard.img`, `rootfs.ext2`/`.ext4`, the kernel `Image`,
  and the `.dtb`, all dated **Sep 8**. Buildroot's own config is fully
  sorted (musl toolchain, eudev, wpa_supplicant w/ WPA3+ctrl_iface+cli,
  `brcmfmac_sdio-firmware-rpi`, kernel `CONFIG_USB_RTL8152=y` confirmed
  set) — nothing further needed there to produce a bootable image.
- **The baked-in binary and config.toml are stale**, though: both date
  from **Sep 6** (matching the last successful CI run,
  `gh run list --workflow=build-pi.yml`), which predates basically all of
  this session's app work (multi-member calendar events, the weather
  widget, the whole Dashboard page, the PIN lock, the refresh/latency
  fixes). Before flashing a "real" image: trigger a fresh CI run
  (`gh workflow run build-pi.yml` — commit+push first, it builds from the
  GitHub remote, not local files; ~2-3.5h regardless of caching), download
  the artifact, drop it into `board/skylight/rootfs-overlay/usr/bin/
  skylight-ha` (`chmod 755` explicitly — see Process gotchas below), bring
  the overlay's `config.toml` up to date with the real one (it's missing
  `[[dashboard]]`, `pin_hash_path`, `weather_entity`/
  `weather_backfill_entity` — none of that existed on Sep 6), then
  rebuild (`make` in `~/buildroot`) to fold the new binary/config into a
  fresh `sdcard.img`.
- **eero mesh WiFi (open, now a hard requirement — see WiFi diagnostics
  below)**: association consistently succeeds but the WPA handshake times
  out, across all three mesh BSSIDs identically. Kernel/driver logs a
  `Disconnect event of DFS AP` even though this chip is 2.4GHz-only (no
  real DFS band) — suspected mislabeled driver message actually reflecting
  eero's mesh Channel-Switch-Announcement handling confusing `brcmfmac`,
  or a regulatory-domain misclassification (no `iw reg set`/CRDA
  configured yet, so the kernel's default/unset reg domain may be feeding
  into whatever's deciding "DFS"). Not a password/PMF issue (both
  explicitly ruled out). USB Ethernet is the working network path for
  now, but WiFi is required for the actual wall-mounted deployment (no
  Ethernet run to that location).

### WiFi diagnostics added to help solve the eero issue (2026-09-18)

Four changes to the overlay/Buildroot config, purely to get enough real
data to actually root-cause the handshake timeout next time it's tested
against the eero mesh (none of this fixes it by itself):

- **`BR2_PACKAGE_WIRELESS_REGDB=y`** enabled in Buildroot's `.config`. The
  kernel here has `CONFIG_CFG80211_REQUIRE_SIGNED_REGDB=y` (confirmed in
  `output/build/linux-custom/.config`) but `wireless-regdb` — the package
  that actually provides the signed `regulatory.db`/`regulatory.db.p7s`
  the kernel needs to apply `country=US` (already set in
  `etc/wpa_supplicant.conf`) — wasn't installed. Real possibility that the
  country-code request has been silently failing to apply this whole
  time, leaving the kernel on a conservative fallback regulatory domain,
  which could easily be tangled up in whatever's making brcmfmac log a
  "DFS AP" disconnect on a chip with no real DFS band. Kernel ≥4.15 (this
  one's 6.12) loads the regdb straight from `/lib/firmware` — no CRDA
  userspace daemon needed, `BR2_PACKAGE_CRDA` was deliberately left off.
- **`etc/modprobe.d/brcmfmac.conf`** (new) sets the `brcmfmac` driver's
  `debug` module parameter to `0xd404` — bitmask for
  CONN|EVENT|INFO|FIL|SCAN (see `drivers/net/wireless/broadcom/brcm80211/
  brcmfmac/debug.h` in the kernel tree for the bit values). `EVENT` is
  specifically where the "Disconnect event of DFS AP" message itself
  comes from; `CONN`/`FIL` should show the actual auth/assoc/IOVAR
  sequence leading up to it.
- **`etc/init.d/S10syslog`** (new) starts BusyBox's `syslogd`/`klogd`
  early (before `S39ethernet`/`S40wifi`), writing to `/var/log/messages`.
  Needed because dmesg's ring buffer is bounded and can wrap during a
  boot with this much extra debug logging turned on — this persists
  everything for the whole session instead.
- **`etc/init.d/S40wifi`** — `wpa_supplicant` now launches with `-dd -t -f
  /var/log/wpa_supplicant.log` instead of `-q`. Verbose, timestamped,
  logged to its own file.

**To actually diagnose next time**: boot, let it attempt (and fail) to
join the eero mesh, then SSH in over the USB Ethernet fallback (still the
reliable path) and pull `/var/log/wpa_supplicant.log` and
`/var/log/messages`. Look for the EAPOL message sequence (which of the
4-way handshake's messages actually got exchanged before it gave up),
`brcmfmac`'s own connect/roam/event log lines around the same timestamp,
and whether the "DFS" disconnect message correlates with a specific event
(e.g. right after an eero mesh channel-switch, or right after a
particular EAPOL message).

**All of this is diagnostic-only and should come back out once the issue
is actually fixed** — `brcmfmac.conf`'s debug level and `-dd` are both
too noisy for normal operation, and `S10syslog` writing continuously to
`/var/log/messages` isn't something a production image needs either.
Checked `output/target/etc/fstab`: unlike `/tmp`/`/run`/`/dev/shm`
(tmpfs), `/var` is a plain directory on the real (`ext2`, read-write)
rootfs, not tmpfs -- so these logs *do* persist across reboots (handy for
catching an overnight retry, but also a real SD-card-wear concern for
continuous verbose logging left on long-term, another reason this is
meant to come back out once solved).

## Process gotchas worth remembering

- `chmod +x file` (no explicit `u`/`g`/`o`/`a`) can be silently suppressed
  on the "other" bits by the shell's umask — use explicit `chmod 755`
  instead, especially for anything transferred through Box, which
  separately strips the executable bit on every transfer regardless (so
  always re-`chmod` after copying onto the Linux side either way).
- Buildroot `menuconfig` changes have repeatedly not actually landed in
  `.config` (happened ~5 times this build — WiFi firmware, `wpa_supplicant`
  itself, `wpa_cli`, `ctrl_interface`, others). **Always `grep -i
  <SYMBOL> .config` immediately after any menuconfig session**, before
  spending a rebuild+reflash cycle assuming a change took. For scripted/
  reliable changes to a known symbol, appending directly to `.config` and
  running `make olddefconfig` is more reliable than fighting the ncurses
  UI.
- SSH (via `dropbear`, once networking is up) is far more reliable for
  debugging than photographing a physical touchscreen console — switch to
  it as soon as any network path (WiFi or the USB Ethernet fallback) is
  available.
- A DRM/KMS process leaves its last-rendered frame on screen after it
  exits — the display doesn't clear or hand back to the text console
  automatically. Don't infer "still running" from what's on screen; check
  `ps` (or watch whether the on-screen clock is still ticking).

## Feedback / working-style notes

- Dynamic linking of `libinput`/`libudev`/`libxkbcommon` (rather than
  building them static, or vendoring) is the deliberately agreed approach,
  not a compromise to revisit — see `feedback_static_vs_dynamic_libs`
  memory. When one person controls both the binary and the target distro,
  the usual "version mismatch" risk of dynamic linking doesn't apply, and
  these libraries have runtime *data* dependencies (hwdb, xkb data) that
  static linking wouldn't solve anyway.
