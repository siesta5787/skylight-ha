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
  It's a clone of upstream Buildroot's own repo (`gitlab.com/
  buildroot.org/buildroot.git`) — don't expect `git status`/`git log` in
  there to show project history; that's upstream's.
- **The actual board overlay lives in its own repo now**:
  `~/pizero2-buildroot` (github.com/siesta5787/pizero2-buildroot,
  private). `~/buildroot/.config`'s `BR2_ROOTFS_OVERLAY` points at
  `~/pizero2-buildroot/rootfs-overlay` (an external absolute path, not
  anything inside `~/buildroot` itself) — this used to live untracked at
  `~/buildroot/board/skylight/rootfs-overlay/`, with zero version control
  or backup, until 2026-09-27. See that repo's own README for what's in
  it and why it's separate (kept general-purpose on purpose, in case this
  Buildroot setup gets reused for a different app someday).
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

**Both commands below are prefixed with `CARGO_TARGET_DIR=target-host`.**
This is deliberate, not optional: the aarch64 Docker build (see "Building the
Pi binary" below) doesn't cross-compile with an explicit `--target` — it
just runs `cargo build` natively inside an aarch64 container, which writes
to the *same* `target/release/` this repo's plain `cargo build` would use.
Without separating them, whichever architecture built most recently
poisons the other's cache with incompatible object files, silently turning
the next build on either side into a full from-scratch rebuild (this
happened once, turned a documented ~1h incremental Pi build into a ~4h cold
one). `target-host` is `.gitignore`d; the env var only affects the host
invocation, never the Docker container (which doesn't see host env vars
unless explicitly passed with `-e`), so the Pi build's own `target/` stays
untouched by local desktop work.

1. **`backend-winit` desktop (fastest, default choice)**: `CARGO_TARGET_DIR=target-host
   cargo run -p skylight-ha -- config.toml` opens a normal window on the
   Pop!_OS desktop. Use this for UI layout, calendar rendering, HA
   WebSocket/REST logic, config schema, todo interactions — anything that
   isn't specifically about touch input or the display driver. Seconds of
   iteration instead of hours. (This is the default feature set; no extra
   flags needed.)
2. **`backend-linuxkms` native on this machine**: `CARGO_TARGET_DIR=target-host
   cargo build --release -p skylight-ha --no-default-features -F
   ui/backend-linuxkms`, then run `./target-host/release/skylight-ha
   config.toml` from a raw VT (`Ctrl+Alt+F3`, log in at the text console —
   KMS needs exclusive DRM master, and the desktop Wayland compositor on
   tty1 holds it otherwise). `Ctrl+Alt+F1` switches back. Same
   evdev/libinput code path as the real Pi, no cross-compilation, no
   Buildroot, no SD card flashing. **This is the right tool for
   touch/display-driver bugs** — `backend-winit` cannot reproduce them at
   all, since it's a completely different input pipeline (winit
   windowing/mouse events vs. raw evdev + libinput + DRM). The USB
   touchscreen on this machine enumerates as `/dev/input/event13` ("Jieli
   Technology USB Composite Device", MT-B, `INPUT_PROP_DIRECT`); `event14`
   is its stylus interface.
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
  `libxkbcommon.so`, **`libgcc_s.so.1`** (a `readelf -d` NEEDED entry on
  every build so far, on this target regardless of `panic = "abort"` --
  discovered 2026-09-27 when a quick manual `--version` check against a
  minimal 3-package Alpine container failed with `_Unwind_*: symbol not
  found`; the real device has always had it, via the musl toolchain's own
  runtime libs at `output/target/lib/libgcc_s.so.1` -- this was only ever a
  gap in ad-hoc verification containers, not a real device issue, but
  worth having written down explicitly this time), udev hwdb data (for
  touchscreen identification), xkb keymap data (`xkeyboard-config`), and
  root permission to open `/dev/dri/*` directly
  (`backend-linuxkms-noseat`, no seatd broker).
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
- **CI's artifact storage can fill up account-wide** ("Artifact storage
  quota has been hit" — happened once, wasting a full ~3h46m run right at
  the finish line). When that happens, or when iterating fast, build
  locally instead — same QEMU-emulated-Alpine approach CI uses, just run on
  this machine with `docker` + binfmt (`sudo apt install docker.io
  qemu-user-static binfmt-support` once; if the current shell session
  predates being added to the `docker` group, prefix commands with `sg
  docker -c "..."` rather than waiting for a fresh login):
  ```
  mkdir -p .cache/cargo-registry target
  timeout --kill-after=30s 3h \
  docker run --rm --platform linux/arm64 --network host \
    -v "$PWD":/workspace -w /workspace \
    -v "$PWD/.cache/cargo-registry":/root/.cargo/registry \
    -e SKYLIGHT_GIT_SHA="$(git rev-parse --short=12 HEAD)" \
    alpine:3.20 sh -c '
      set -eu
      apk add --no-cache curl gcc libinput-dev eudev-dev libxkbcommon-dev pkgconf musl-dev linux-headers
      # rustup'"'"'s own toolchain-component download (separate HTTP client from
      # the curl below, which only fetches the installer script) has hung
      # indefinitely on a stalled connection multiple times in practice, with no
      # timeout of its own -- bound each attempt and retry rather than risk
      # burning hours on one bad connection.
      i=0
      until curl --proto "=https" --tlsv1.2 -sSf --retry 3 --retry-delay 5 --max-time 120 https://sh.rustup.rs \
          | timeout 600 sh -s -- -y --profile minimal --default-toolchain stable; do
        i=$((i + 1))
        if [ "$i" -ge 5 ]; then echo "rustup install failed 5 times -- giving up" >&2; exit 1; fi
        echo "rustup install attempt $i timed out or failed, retrying in 15s..." >&2
        rm -rf "$HOME/.rustup" "$HOME/.cargo"
        sleep 15
      done
      . "$HOME/.cargo/env"
      cargo build --release -p skylight-ha \
        --no-default-features -F ui/backend-linuxkms
    '
  ```
  This is also exactly the recipe `scripts/release.sh` automates (see
  "Releasing a new version" below) -- this block is worth keeping in sync
  with that script if either one changes, they're meant to behave
  identically. `SKYLIGHT_GIT_SHA` is passed in because `build.rs` can't ask
  git for it from inside the container: no `git` binary is installed there,
  and a bind-mounted checkout would trip git's "dubious ownership" check
  even if there were. `--network host` and the outer `timeout` wrapped
  directly around `docker run` (not around some larger script) are both
  scar tissue from cutting the first real release on 2026-09-28/29: the
  default Docker bridge network was repeatedly causing connection resets/
  hangs/IO errors *inside* the emulated container specifically (apk
  extraction failures, rustup hangs) that never once reproduced testing
  this host's own network directly, which pointed at Docker's NAT layer
  fighting QEMU's syscall translation under load rather than a real
  connectivity problem -- `--network host` sidesteps that layer entirely.
  And a `timeout` wrapped around a whole *script* rather than the `docker
  run` command itself doesn't actually kill the container on a hang (a
  plain shell doesn't forward SIGTERM to a foregrounded child by default),
  which left an orphaned container running for 6 hours undetected, once,
  contending for resources with the next attempt. `docker run` attached in
  the foreground does forward SIGTERM into a real container stop request,
  so timing out `docker run` directly actually cleans up after itself.

  Output lands directly at `target/release/skylight-ha` — no artifact
  upload, no quota, no waiting on GitHub's queue. `target/` and the cargo
  registry persist on disk between runs (unlike CI's cache, no
  upload/download round trip), so a **cold** build is ~4h (matches CI's own
  timing — this is QEMU emulation tax, not something caching fixes) but an
  **incremental** build (only this workspace's own crates changed) is
  ~1-1.5h, dominated almost entirely by the final whole-program LTO
  relink (`[profile.release]` has `lto = true, codegen-units = 1` —
  deliberate, for a smaller/faster binary on the Pi's weak CPU) which has
  to redo its whole-program pass regardless of how small the change was.
  True cross-compilation (no QEMU at all) would eliminate that tax
  entirely but needs a hand-assembled aarch64-musl sysroot with
  `libinput`/`libudev`/`libxkbcommon` built for that arch — a real
  undertaking, not worth it unless build time becomes the actual
  bottleneck (see `feedback_static_vs_dynamic_libs`-style reasoning: the
  emulated-native-build approach gets Alpine's own correctly-linked
  aarch64 packages "for free").

## Releasing a new version

As of `v0.2.0` (2026-09-29, the first real release), ordinary app changes
no longer need a manual reflash at all -- see "In-app self-update" under
App features below for how the device consumes this. Reflashing an SD
card is now only needed for the very first device (already done) and for
any future release that sets `requires_reflash: true`.

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`,
   run `cargo update --workspace` (updates `Cargo.lock`'s version fields
   only, nothing else should change -- diff it to confirm), commit both.
2. `sg docker -c scripts/release.sh -m "release notes here"` (add
   `--requires-reflash` if this release needs more than a binary swap --
   see the App features entry for exactly what that means and why; add
   `--skip-build` if `target/release/skylight-ha` is already a fresh,
   correct aarch64 build you just made).
3. That one command does everything: builds aarch64 via the same local
   Docker recipe documented above, runs the binary's own `--version`
   inside a minimal emulated-aarch64 container as a preflight (catches a
   forgotten version bump or a wrong-arch build before it ever reaches a
   device), renders `manifest.json`, tags, pushes the tag, and publishes
   the GitHub Release with both assets attached.
4. Sanity-check it landed: `curl -sL https://github.com/siesta5787/
   skylight-ha/releases/latest/download/manifest.json` should show the
   new version; the device's own "Check for updates" button (Settings,
   behind the PIN gate) or its background check (every ~6h) will find it
   from there.

**Deliberately local, not CI.** `build-pi.yml` no longer builds or
publishes anything automatically (no `push:` trigger at all -- see
`scripts/release.sh`'s own comments and the section above for why: the
QEMU emulation tax is identical whether it runs on GitHub's runners or
here, so routing releases through CI only added queue time and burned
Actions minutes for zero speed benefit). It's `workflow_dispatch`-only
now, kept as a fallback build path, not part of the release flow.

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

**Connection resilience** (ha-client, 2026-09-26): the app used to be able
to freeze *permanently* on a silently-dropped WiFi link — no timeout
existed anywhere on the HA connection. `Client::call` now has a 45s
`CALL_TIMEOUT` (generous on purpose: `get_states()` has been measured
taking up to 17.5s on this real instance under load, so anything tighter
would fire spuriously). `run_actor` sends an application-level
`{"type":"ping"}` every 30s (`PING_INTERVAL`) and treats a missing `pong`
within another 30s (`PONG_TIMEOUT`) as a dead connection — necessary
because `tungstenite` doesn't enable TCP keepalive, so a link that dies
without a FIN/RST leaves the socket looking "open" at the OS level
forever, with writes silently succeeding into the kernel buffer and reads
just never returning. `RestClient` (the REST/calendar path, `rest.rs`)
gets the equivalent it never had: `.timeout(45s)`/`.connect_timeout(15s)`
on its `reqwest::ClientBuilder` instead of the no-timeout
`reqwest::Client::new()`. Also fixed: `run_actor` used to kill the whole
connection on *any* non-`Text` WebSocket frame (a server `Ping`, a `Pong`,
a `Binary` frame all looked identical to "closed" and forced an
unnecessary reconnect) — now only an actual `Close`/error/end-of-stream
breaks the loop. And the dead-connection signal the app watches for
(`Client::wait_closed()`) was rewritten to key off the command channel
closing (`cmd_tx.closed()`, which *does* fire once `run_actor`'s loop
exits for any reason) rather than the event-broadcast channel closing,
which structurally could never fire in practice since `live_client` holds
a live sender clone for the whole session.

**Clock self-correction** (main.rs, 2026-09-26): this device has no RTC,
so it boots at the kernel epoch (1970) and a background NTP sync
(`S45ntp`) corrects it some seconds-to-a-minute later, racing the app's
own startup. The original code called `time::UtcOffset::
current_local_offset()` exactly once, at the very top of `main`, and
never again — which routinely resolved the wrong DST bucket (e.g. EST
instead of EDT) from a pre-correction clock and then never re-checked, so
the on-screen time stayed an hour off for the life of the process, and the
calendar grid's `reference_date` could sit on "January 1970" until someone
manually tapped Today. Can't just call `current_local_offset()` again
later, though — the `time` crate documents that as unsound once other
threads exist (which by definition they do, later), and that's exactly
why it was only ever called once at the top. Fixed with `libc::
localtime_r` instead (`system_utc_offset` in main.rs): POSIX guarantees
the reentrant `_r` variant is thread-safe (glibc/musl both hold an
internal lock over their cached TZ state), so — unlike `time`'s own
implementation — it's sound to call every second from the existing clock
timer. `tm_gmtoff` reports the offset *for the given instant*, which is
what makes a 1970→2026 correction actually move to the right DST bucket
instead of just changing the number of a still-wrong bucket. The
day-rollover logic (`reference_date_after_today_changed`) is deliberately
narrow: it only follows "today" changing if the currently-displayed date
*was* the old today (i.e. the user hadn't navigated anywhere) — yanking
the grid out from under someone who'd paged to next month would be worse
than the original bug.

**Attempted and reverted: a Settings → Logs page** (2026-09-19,
reverted same day). The idea was an in-app viewer for recent `tracing`
output (captured via a custom `tracing_subscriber::Layer` into a ring
buffer), so on-device debugging wouldn't need SSH. Implemented as a
second `if root.showing-logs: VerticalLayout {...}` block living directly
inside `SettingsPage`'s root `Rectangle`, alongside the existing `if
!root.showing-logs: ...` block. This **rendered correctly but never
responded to touch at all** — confirmed live, repeatedly, on real
hardware. Root cause understanding stayed incomplete (no interactive
debugger on-device), but the going theory: Slint's "auto-fill the parent"
sizing for a `Rectangle`'s sole child only reliably applies to a single
*static* layout child, not two mutually-exclusive `if` alternates: even
after wrapping the two blocks in one always-present outer `VerticalLayout`
(the same fix that worked for the dashboard-cards bug), touch still didn't
work. What *is* confirmed to work, because it's the exact shape `PinPad`
already uses successfully: a full-page overlay needs to be its own
**top-level component**, always present as a sibling in `app-window.slint`
(not nested inside another page's conditional state), sized
unconditionally (`width: 100%; height: 100%;`), with only a `visible:
some-bool` toggling it — see `crates/ui/ui/pin-pad.slint`. If this gets
revisited, build it that way from the start rather than re-attempting the
nested-`if`-inside-a-page shape.

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

**In-app self-update** (`apps/skylight-ha/src/update.rs`, 2026-09-27/29):
checks GitHub Releases for a newer version and installs it without a
manual reflash. Full design writeup: `~/.claude/plans/
skylight-self-update.md`. Confirmed working end-to-end on real hardware
2026-09-29 (`v0.1.0` -> `v0.2.0` via the on-device button). Short version:

- **Binary-only.** Only `/usr/bin/skylight-ha` gets replaced -- no kernel,
  overlay, or `config.toml` changes ship this way. A release that needs
  more than that sets `requires_reflash: true` in its manifest (via
  `scripts/release.sh --requires-reflash`); the app reports it in Settings
  but refuses to install it. **Corollary: never add a new `config.toml`
  key without `#[serde(default)]`, unless the release is also marked
  `requires_reflash`.** `dashboard-config::Config` has
  `#[serde(deny_unknown_fields)]` and a parse failure is a hard `exit(1)`
  before any UI exists -- a rollback (see below) to a binary that predates
  a required new key would hit that wall with the rollback already spent.
- **Version source of truth**: `https://github.com/siesta5787/skylight-ha/
  releases/latest/download/{manifest.json,skylight-ha-aarch64-linux-musl}`
  -- the plain stable redirect, not the GitHub API (no rate limit, no
  required headers, and it auto-skips prereleases/drafts, which is the
  whole test channel: tag `v0.3.0-rc1`, publish it as a prerelease, no
  device ever sees it).
- **Trigger**: periodic background check (~3min after startup, then every
  ~6h + jitter) or the "Check for updates" button in the new Settings
  card (behind the PIN gate). **Never auto-installs** -- a wall-mounted
  display silently freezing for a few seconds to swap binaries would look
  exactly like a crash, so install always requires an explicit tap.
- **Install sequence**: stream download straight to `/usr/bin/
  skylight-ha.new` (not `/tmp`, which is tmpfs -- see the `/var/log`
  symlink note below, `rename` across filesystems would `EXDEV`), verify
  SHA-256 against the manifest (`sha2`, already a dependency from the PIN
  feature), run the new binary's own `--version` as a preflight (safe
  because `--version` is handled as the literal first statement in
  `main()`, before any Slint/DRM init -- this is the same check
  `scripts/release.sh` runs before ever publishing a release, so a broken
  build should never even get this far), save a rollback copy of the
  currently-running binary, write a `pending` marker under `/var/lib/
  skylight/update/`, `fsync`, then atomic `rename(2)` over `/usr/bin/
  skylight-ha`, then `exit(0)`. `rename(2)` swaps the directory entry, not
  the inode a still-running process has open, so this is safe to do while
  the app is live -- never write in place over the running binary
  (`ETXTBSY`, and non-atomic even if it somehow worked).
- **Restart + rollback**: `skylight-supervise` (unchanged for the happy
  path -- it just re-execs `$BIN` by path) picks up the new binary. It
  also now arms itself when it sees `pending` at spawn time, and if the
  child dies before the app clears that marker (a `slint::Timer::
  single_shot(60s, ...)` fired only if the event loop is genuinely
  running -- real proof of life, deliberately not gated on a successful
  HA connection since that's routine and unrelated to binary health),
  counts the failure. At 3 strikes it restores the saved rollback copy
  and records the failed version to a `blocked` file so the same broken
  release doesn't get reinstalled every 6 hours forever. All of this is
  in `~/pizero2-buildroot/rootfs-overlay/usr/bin/skylight-supervise` --
  boot-critical-script discipline applies (see Process gotchas below): no
  new command dependencies beyond `cat`/`cp`/`mv`/`chmod`/`rm`/`sync`/
  `logger`/`mkdir`, every addition inert if the state directory is
  absent. **The rollback path itself is still unvalidated on real
  hardware** -- everything up through a successful install/restart/commit
  has been confirmed live, but nobody has yet published a deliberately
  broken release to watch the 3-strike restore actually fire on the Pi.
  Worth doing once, since an untested rollback is worse than none.
- **Env var overrides** (`SKYLIGHT_UPDATE_BASE_URL`, `_STATE_DIR`,
  `_BINARY`, `_FIRST_CHECK_SECS`, `_INTERVAL_SECS`, `_DISABLE`) make the
  whole thing testable from `backend-winit` on the desktop against a
  throwaway local HTTP server, without touching the real device or a
  real GitHub release.
- **Disk**: this is why the rootfs is 256M now, not the original 120M
  (bumped in `pizero2-buildroot/defconfig`, applied to `~/buildroot/
  .config` and baked into the image during the 2026-09-29 reflash) --
  the live binary, a staged download, and a rollback copy need to
  coexist briefly, roughly 3x the ~20MB binary, which didn't fit in the
  original ~31MB free.

**Wi-Fi management** (`apps/skylight-ha/src/wifi.rs`, 2026-10-01): Settings
card showing the live connection (SSID, IP, signal) plus a full-screen manager
to scan and join networks, with the passphrase typed on the existing
`VirtualKeyboard`. Speaks the **wpa_ctrl protocol directly** over a Unix
datagram socket -- no `wpa_cli` subprocess (though `wpa_cli` existing on the
image still matters as *evidence* that the binary has `CONFIG_CTRL_IFACE`
compiled in; see the Buildroot note below). Two control connections, as
`wpa_cli` itself uses: one for commands, one `ATTACH`ed for events, so an
event arriving mid-command can't be mistaken for that command's reply.
- **Why the control socket and not rewriting `wpa_supplicant.conf`**: the
  config-rewrite approach needs no Buildroot change and could have shipped via
  the updater immediately, but it can only ever report "didn't connect".
  `CTRL-EVENT-SSID-TEMP-DISABLED ... reason=WRONG_KEY` and
  `CTRL-EVENT-AUTH-REJECT` let the UI say **"wrong password"** instead -- which
  matters a great deal when the password was typed on a touchscreen keyboard.
- **The safety property it's built around**: `SELECT_NETWORK` disables every
  *other* configured network, so a typo would otherwise strand a wall-mounted
  device with no keyboard. A failed attempt always does `REMOVE_NETWORK` +
  `ENABLE_NETWORK all` + `RECONNECT`; only a *successful* one is persisted with
  `SAVE_CONFIG`. Both halves have tests.
- **This removes the "re-enter WiFi credentials on the SD card after every
  reflash" ritual** -- `update_config=1` means a network joined on the
  touchscreen is saved back to `/etc/wpa_supplicant.conf`. The overlay in git
  still carries only placeholders; real credentials just get typed on the
  device now instead of hand-edited onto the card.
- Scanning collapses a mesh's BSSIDs into one row per SSID (this network has
  three eero nodes on one SSID, which are one choice, not three).
  WPA-Enterprise is shown but refused -- no UI collects EAP credentials. A
  WPA2/WPA3-transition AP is deliberately treated as **PSK**, since SAE is a
  genuine question mark on this chip (`brcmfmac.conf`'s
  `feature_disable=0x82000` includes `BRCMF_FEAT_SAE`, pushing SAE out of
  firmware into wpa_supplicant's userspace path as part of the eero handshake
  fix) -- treat an SAE failure as expected-unknown, not a bug to chase.
- Passphrases are never trimmed (surrounding whitespace is legal, and trimming
  would silently produce an inexplicable "wrong password") and never logged.
- Tested against a **fake wpa_supplicant** -- a real datagram socket serving
  canned replies and pushing events to whichever client sent `ATTACH`. Worth
  knowing if you extend it: a `connect`ed datagram socket only accepts packets
  from its connected peer, so events must come from the daemon's own socket.

**Time zone picker** (`apps/skylight-ha/src/timezone.rs`, 2026-10-01): replaces
the hardcoded `BR2_TARGET_LOCALTIME` with a browsable **continent -> country ->
zone** drill-down. Built from the full world list rather than a curated US one
at the user's request, so the project is useful to someone elsewhere.
- Driven by **`zone1970.tab` joined against `iso3166.tab`**, not a directory
  walk: `/usr/share/zoneinfo` holds ~1200 files, but `posix/` and `right/` are
  complete duplicate trees and there are many backward-compat aliases
  (`US/Eastern`). The tab files list exactly the canonical zones, already
  annotated with country codes and the human-readable disambiguations
  ("Eastern (most areas)") the third level shows. A zone shared by several
  countries is listed under each; a country with only one zone is selected
  outright rather than making the user confirm a single-item list.
- **Changing the zone rewrites `/etc/localtime` and then restarts the app**,
  which is not laziness. musl caches the zone by the `TZ` *string* and never
  re-stats the file -- verified in musl 1.2.6's own `src/time/__tz.c`, where
  `do_tzset()` early-returns whenever `getenv("TZ")` matches its cached value,
  and with `TZ` unset that value is the constant `"/etc/localtime"` forever.
  So a symlink rewrite alone cannot affect a running process. The only
  in-process escape is `setenv`, which is **unsound** once the process is
  multithreaded -- exactly the hazard `system_utc_offset` was written to avoid
  when it rejected `time::UtcOffset::current_local_offset()`, with musl's own
  `do_tzset` calling `getenv("TZ")` once a second from our clock tick as the
  other half of the race. Restarting reuses the updater's existing
  exit-and-respawn path, overlay included.
- The symlink *is* the persistence, so there's **no new state file and no new
  `config.toml` key** -- the latter would have been a trap, since
  `deny_unknown_fields` makes an update rollback to a binary predating the key
  unbootable.
- **There is deliberately no DST toggle.** tzdata switches EST/EDT on the right
  dates by itself; a manual override could only fight it. The card shows the
  current abbreviation and offset ("EDT - UTC-4") to make that visible instead.

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
- Overlay directory: **`~/pizero2-buildroot/rootfs-overlay/` — outside
  `~/buildroot` entirely, in its own repo** (see the note in "One machine
  now" above; moved there 2026-09-27, used to live untracked at
  `~/buildroot/board/skylight/rootfs-overlay/`). `~/buildroot/.config`'s
  `BR2_ROOTFS_OVERLAY` points at it by absolute path. Set via System
  configuration → Root filesystem overlay directories if it's ever lost
  from `.config` (**verify with `grep BR2_ROOTFS_OVERLAY .config`
  afterward** — same "menuconfig changes don't land" gotcha as everything
  else in this section). Contents as of 2026-09-27:
  - `etc/wpa_supplicant.conf` — `ctrl_interface=/var/run/wpa_supplicant`,
    `update_config=1`, `country=US`, and a placeholder network block. **The
    real SSID/password still never exist in this repo** — but as of 2026-10-01
    they no longer have to be hand-edited onto the card either: join the
    network from Settings → Wi-Fi on the device and `update_config=1` saves it
    back here. **The `ctrl_interface=` line is the one that broke boot once**
    (an unrecognised directive is a hard parse error, so wpa_supplicant simply
    refuses to start, which presents exactly like a boot hang). It is only
    valid because the package was rebuilt and verified by artifact — see the
    Process gotcha below, and the comments in the file itself.
  - `etc/init.d/S39ethernet`, `etc/init.d/S40wifi` — bring up `eth0`/`wlan0`
    respectively: link up, (wlan0 only) `iw dev wlan0 set power_save off`
    (brcmfmac on this chip drops connections repeatedly without this —
    confirmed root cause, not a hypothesis) + `wpa_supplicant`, then in
    both scripts a `udhcpc` invocation via `start-stop-daemon -b -x
    /sbin/udhcpc -- -f -i $IFACE -t 0 -T 5 -S -v` — see the DHCP-timing fix
    below for why this replaced a plain `udhcpc -i $IFACE -b`.
  - `usr/share/udhcpc/default.script.d/10-default-route-metric` — see the
    "two default routes" fix below.
  - `etc/init.d/S45ntp` — one-shot-but-retried NTP time sync; see below.
  - `etc/init.d/S46wifidiag` — periodic (5 min) WiFi signal/error-counter +
    ping logger to `/var/log/messages`, added while chasing the WiFi
    reliability issues below. Still there, still useful as an ongoing
    health log even though the bugs it was built to diagnose are fixed.
  - `etc/cron/crontabs/root` — the connectivity self-healing watchdog; see
    below.
  - `etc/default/syslogd` — `SYSLOGD_ARGS`, log rotation sizing; see below.
  - `etc/modprobe.d/brcmfmac.conf` — `feature_disable=0x82000`, the
    permanent WiFi handshake fix; see below.
  - `usr/bin/skylight-supervise` + `etc/init.d/S99skylight` — crash-recovery
    respawn wrapper around the app (see below), **plus (2026-09-29) the
    in-app self-updater's rollback logic** — arms itself when it sees a
    `pending` update marker, restores the last-known-good binary after 3
    failed spawns in a row. See "In-app self-update" under App features
    above for the full design. `S99skylight` also (2026-09-30) silences the
    framebuffer console before handing over the display — see the next
    bullet.
  - **Kernel console no longer paints over the app — fix verified live
    2026-09-30, but NOT on the device yet.** Symptom: plugging or unplugging
    any USB device made `fbcon` repaint the *entire* retained console (the
    whole boot log, down to the login prompt) straight over the running
    dashboard — one `usb 1-1: USB disconnect` printk was enough, because
    fbcon redraws everything it has retained, not just the new line. Fixed
    in `S99skylight`'s `start()` by setting `console_loglevel` to 1 and
    unbinding fbcon from the VT layer (`/sys/class/vtconsole/*/bind`,
    matched on the `name` file rather than assuming `vtcon1`; kernel has
    `CONFIG_VT_HW_CONSOLE_BINDING=y`, verified in the built kernel's own
    `.config`). `stop()` restores both, so `/etc/init.d/S99skylight stop`
    hands the console back over SSH. Deliberately NOT done via
    `quiet`/`loglevel=` on the kernel cmdline, so console output *during*
    boot — the main way boot hangs have actually been diagnosed here, twice
    — stays fully intact; only the post-boot window goes quiet. Nothing is
    lost for diagnostics: syslogd still writes `/var/log/messages` and
    `dmesg` reads the ring buffer, neither of which console loglevel
    affects. **Confirmed working on real hardware by testing the two writes
    live over SSH** (no reboot needed — both are instantly reversible),
    but it's an overlay change, so **the in-app updater cannot deliver it**
    — it lands on the device at the next image build + reflash. Anything
    else needing a reflash should be bundled into that same build.
  - `usr/bin/skylight-ha` — the app binary.
  - `etc/skylight/config.toml` and `etc/skylight/ha-token.secret` — both
    baked into the overlay, but **`ha-token.secret` in the repo itself is
    only ever a placeholder** (currently 22 bytes — nowhere near a real ~200
    char HA long-lived token). Same as the WiFi credentials: the real token
    only exists hand-edited onto the physical SD card, re-entered after
    every reflash. Don't trust a past note here that said this was "baked
    in permanently" — that was never true of the real secret value, only
    of the fact that a file exists at that path.
- **Image builds cleanly and reliably**: Buildroot's own config is fully
  sorted (musl toolchain, eudev, wpa_supplicant w/ WPA3+ctrl_iface+cli
  *enabled in Buildroot's `.config`* — see the Process gotcha below about
  why that alone doesn't mean much —, `brcmfmac_sdio-firmware-rpi`, kernel
  `CONFIG_USB_RTL8152=y`). `cd ~/buildroot && make` after updating the
  overlay (new binary copied into `usr/bin/skylight-ha` +
  `chmod 755`, or any overlay file edited) reliably produces a fresh,
  correct `sdcard.img` — this has been done many times over this session
  without a single Buildroot-level failure; every incident traced back to
  overlay *content* (a boot-critical script bug), never the build system
  itself.
- **Keeping the overlay's binary current**: after any Rust change, rebuild
  aarch64 locally (see "Building the Pi binary" above), `cp` the result
  into `~/pizero2-buildroot/rootfs-overlay/usr/bin/skylight-ha`, `chmod 755`
  it, `md5sum` both copies to confirm the copy landed correctly, *then*
  `make` in `~/buildroot`. As of 2026-09-27 the overlay's binary/config
  are current with everything described in this doc (all of App
  features, all of the fixes below) — this note itself is the thing
  liable to go stale, not the image.
- **eero mesh WiFi — RESOLVED (2026-09-25/26), permanent fix.** Association
  always succeeded, but the WPA handshake never appeared to complete and
  the link dropped ~10s later — across all three mesh BSSIDs, and (this
  was the key clue that ruled out eero specifically) identically against a
  plain phone hotspot too. The `Disconnect event of DFS AP` message and
  the regulatory-domain hypothesis below were both dead ends. **Actual root
  cause**, found by reading the driver's own decision logic
  (`brcmf_is_linkup`/`brcmf_notify_connect_status` in
  `drivers/net/wireless/broadcom/brcm80211/brcmfmac/cfg80211.c`) against
  live `wpa_supplicant.log`/`/var/log/messages` captures: this chip's
  firmware completes the entire WPA-PSK 4-way handshake *itself*
  (`PSK_SUP` event, `status=6` = `BRCMF_E_STATUS_FWSUP_COMPLETED`, a real
  success) and the driver correctly reports `Linkup` to the kernel's
  wireless stack — but `wpa_supplicant` 2.12 never logs
  `CTRL-EVENT-CONNECTED` and the link drops anyway. This matches a known
  upstream `wpa_supplicant` ≥2.11 regression: it waits for an
  `NL80211_CMD_PORT_AUTHORIZED` event before considering a
  firmware-offloaded connection complete, but `brcmfmac` only ever sends
  that event for Fast BSS Transition roams, not normal connections — so
  `wpa_supplicant` waits forever for an authorization signal that's never
  coming, on *any* AP. Fix: `options brcmfmac feature_disable=0x82000` in
  `etc/modprobe.d/brcmfmac.conf` — `0x82000` = `BIT(13)` `BRCMF_FEAT_FWSUP`
  `| BIT(19)` `BRCMF_FEAT_SAE` (verified against this exact kernel's own
  `feature.h`, not assumed from a value quoted online), disabling the
  firmware-offload handshake for both WPA2-PSK and WPA3-SAE and forcing
  the traditional path where `wpa_supplicant` itself processes EAPOL
  frames in userspace, sidestepping the broken `PORT_AUTHORIZED`
  dependency entirely. **This is a permanent fix, keep it.** (A "WiFi
  diagnostics" subsection used to live here documenting the verbose
  `brcmfmac` debug logging and `S10syslog` added to find this bug — both
  were scaffolding, not the fix, and have since been removed now that
  their job is done; see the log-rotation entry a bit further down.)
  `BR2_PACKAGE_WIRELESS_REGDB` is still enabled (harmless, and a real gap
  regardless — the kernel does require a signed regdb for `country=US` to
  apply — just wasn't actually this bug).

### DHCP timing — RESOLVED (2026-09-26)

Separate, unrelated bug found *after* the WiFi handshake fix above: even
with a rock-solid WiFi link (confirmed live — signal -34 to -43 dBm the
whole time, healthy 65-72 Mbit/s bitrates, steadily climbing RX/TX
counters with no resets, 0% ping packet loss throughout), the device
ended up on most boots with an IPv6 address (via SLAAC, no server round
trip needed) but **no IPv4 address at all** — `udhcpc` just never got a
lease. HA data and NTP both failed as a result, but not because of IPv6
itself: the router's DNS server is only reachable via IPv4
(`nameserver 192.168.0.1` in `resolv.conf`, no IPv6 DNS ever advertised),
so no IPv4 route meant no DNS meant nothing could resolve, regardless of
which address family a hostname would otherwise resolve to. A static DHCP
reservation on the router "fixed" it 2/2 clean boots, pointing at DHCP
request timing/reliability rather than the router being unable to serve
this device at all.

Two wrong turns before the real fix, worth remembering:
- First theory: `udhcpc`'s foreground DISCOVER phase (3 tries, ~9-12s by
  default) could fire before `wpa_supplicant` finished associating,
  wasting the whole budget on a not-yet-up link. Attempted fix: a
  `wpa_cli -i wlan0 status` polling loop in `S40wifi`, waiting for
  `wpa_state=COMPLETED` before calling `udhcpc`. **This broke boot
  completely, every single time** — turned out `wpa_cli` doesn't even
  exist on this image (`CONFIG_CTRL_IFACE=y` was set in Buildroot's
  top-level `.config`, but `wpa_supplicant`'s *own* build `.config` still
  had it commented out because the package was never actually rebuilt
  after that symbol changed — see the Process gotcha below, this is a
  deeper version of the already-known "menuconfig didn't land" trap), and
  worse, adding `ctrl_interface=` to `wpa_supplicant.conf` made
  `wpa_supplicant` itself refuse to start at all (an unrecognized config
  directive is a hard parse error when `CONFIG_CTRL_IFACE` isn't
  compiled in). **Do not re-add `ctrl_interface`/`wpa_cli` usage** without
  first confirming `output/target/usr/sbin/wpa_cli` actually exists.
  Reverted immediately back to the prior working `S40wifi`.
  - **Don't try `wpa_cli`/`ping6`/`nslookup <name>` (no server arg)/`ip -s
    link`/`timeout <cmd>` on this image without checking first** — none of
    them exist or work as expected in this busybox/wpa_supplicant build.
    `nslookup` specifically needs the server passed explicitly
    (`nslookup <name> <server-ip>`) since it can't parse the (perfectly
    valid) inline comments `udhcpc`'s own hook script writes into
    `resolv.conf` (`nameserver 192.168.0.1 # eth0`).
- Second theory (also wrong, but harmlessly so): `busybox udhcpc -b`
  "gives up after 3 tries". **It doesn't** — confirmed directly in
  `networking/udhcp/dhcpc.c`: after the foreground DISCOVER phase fails,
  `-b` backgrounds and then keeps retrying *indefinitely* on a ~29s cycle.
  So neither "add a fixed delay" nor "give it more foreground retries"
  (the two fixes being considered before this was caught) would have
  addressed a *persistent* DHCP failure — the bug was never a one-time
  missed window.
- **Actual fix**: replaced the `udhcpc` invocation in both `S39ethernet`
  and `S40wifi` with `start-stop-daemon -b -x /sbin/udhcpc -- -f -i
  "$IFACE" -t 0 -T 5 -S -v` — `-t 0` retries DISCOVER forever on a tight
  5s cycle instead of ever falling into the slower ~29s post-failure loop,
  and moving the backgrounding to `start-stop-daemon` (rather than
  `udhcpc`'s own internal fork, which only happens *after* the ~10s
  foreground phase) means both init scripts return immediately instead of
  blocking boot for ~10s each. `-S` logs DISCOVER/OFFER/lease activity to
  syslog — actual visibility into what's happening, which didn't exist
  before. No live capture of a *failing* boot with this in place yet, so
  the precise mechanism of the original persistent failures (bad timing
  vs. something router/mesh-side rejecting/ignoring requests) is still
  technically unconfirmed — but the fix is unconditionally more robust
  regardless of which it was.
- **Self-healing backstop**: `etc/cron/crontabs/root` (busybox `crond`,
  already running via Buildroot's own `S50crond`) checks once a minute for
  an IPv4 default route and restarts `S40wifi` if there isn't one. Covers
  this whole class of problem generally (a router reboot, `brcmfmac`
  silently dropping the link at 3am, a lease that never renews), not just
  this specific bug — for an unattended wall appliance this matters more
  than having pinned down the exact original mechanism.
- **Known remaining rough edge, accepted for now**: even with all of the
  above, a boot can still take something like 30s-1min to get both the
  clock and HA data fully settled (confirmed by the user across several
  reboots — inconsistent, not every boot). Believed to be inherent
  variability in how long the very first WiFi-association-then-DHCP dance
  takes on a cold boot (it's varied throughout this whole project), plus
  `crond`'s watchdog only running once a minute (busybox `crond`'s actual
  granularity floor) rather than checking sooner. Nothing currently
  auto-recovers *faster* than that; the app's own reconnect logic and the
  clock self-correction (see App features above) mean it always gets
  there without user intervention, just not always quickly. Revisit if it
  becomes annoying enough to matter — a tighter boot-time polling loop
  (separate from the ongoing `crond` job) is the likely next step, not
  yet built.
- Three smaller companion fixes from the same investigation:
  - **Log rotation, and `S10syslog` was dead code all along.** The
    `brcmfmac` debug logging (`debug=0xd404`) used to find the WiFi
    handshake bug above was so verbose it rotated `/var/log/messages`
    away every few minutes (busybox `syslogd`'s default is `-s 200 -b 1`,
    ~400KB total). `etc/init.d/S10syslog` — added early on to try to fix
    this by starting `syslogd`/`klogd` earlier in boot — turned out to be
    dead code the whole time: Buildroot's own `S01syslogd`/`S02klogd`
    (not part of this overlay, run earlier regardless) already start the
    real daemons, so `S10syslog`'s own `start-stop-daemon` call just
    matched an already-running process and did nothing; its `-O
    /var/log/messages` flag was never actually applied. **Deleted
    entirely.** Rotation is now sized the supported way: `etc/default/
    syslogd`'s `SYSLOGD_ARGS="-s 2000 -b 5"`, sourced by Buildroot's own
    `S01syslogd` (confirmed by reading that script directly — it does
    `[ -r "/etc/default/$DAEMON" ] && . "/etc/default/$DAEMON"` with
    `DAEMON=syslogd`) — ~12MB of history instead of ~400KB. The noisy
    `debug=0xd404` itself is also gone now that its job (finding the
    handshake bug) is done, leaving only the permanent
    `feature_disable=0x82000` in `brcmfmac.conf`.
  - **Two default routes when both `eth0` and `wlan0` have leases**
    (common during testing, with Ethernet plugged in as a fallback
    alongside WiFi): Buildroot's own `udhcpc` hook script only ever
    touches the interface whose lease just changed, never removing the
    *other* interface's default route, so both ended up at metric 0 with
    non-deterministic egress selection — a plausible contributor to
    "works over Ethernet, intermittently fails over WiFi" observations.
    Fixed via `usr/share/udhcpc/default.script.d/10-default-route-metric`
    (a hook script, confirmed via reading the real `default.script` that
    Buildroot does source every executable file in that directory after
    its own work) — gives `eth0` a lower/preferred metric (100) than
    `wlan0` (600) so they stop fighting.
  - **Crash recovery**: `panic = "abort"` (deliberate, not changing that)
    plus a DRM/KMS process leaving its last frame on screen after exit
    meant any panic used to freeze the wall-mounted display indefinitely
    with zero indication anything was wrong, no way to recover short of a
    power cycle. `S99skylight` now launches `usr/bin/skylight-supervise` (a
    small respawn-loop wrapper) instead of `skylight-ha` directly —
    auto-restarts within 2s of any exit, logs to
    `/var/log/skylight-ha.log`. Chose a plain shell loop over busybox
    `inittab`'s `respawn` mechanism specifically to avoid having to copy
    Buildroot's own generated `/etc/inittab` into the overlay just to add
    one line (which would mean keeping it in sync with Buildroot's version
    by hand forever).

### NTP / clock — RESOLVED (2026-09-20 boot-side, 2026-09-26 app-side)

This device has no RTC, so every boot starts at the kernel epoch (1970).
Two-layer fix, boot-side and app-side, and both ended up mattering:

- **Boot-side**: `etc/init.d/S45ntp` runs `ntpd -n -q` in the background
  (so it can't block `dropbear`/`skylight-ha` from starting — an earlier,
  blocking, single-attempt version of this script did exactly that and
  had to be fixed), first polling for a default route (up to 30x2s) since
  `S40wifi` backgrounds its own `udhcpc` call and networking may not be
  up yet when this script runs, then retrying the actual `ntpd` sync up
  to 5 times with a 5s gap. Still not airtight on its own — see the "known
  remaining rough edge" note above — which is exactly why the app-side fix
  below matters as much as it does.
- **Timezone**: hardcoded via Buildroot's own `BR2_TARGET_TZ_INFO=y` +
  `BR2_TARGET_LOCALTIME="America/New_York"` (`BR2_TARGET_TZ_ZONELIST`
  left at `"default"`, installs the common set, a few MB). This is a
  first-class Buildroot mechanism (`system/Config.in`), not a hand-rolled
  `/etc/localtime` symlink. **The user wants this eventually exposed as a
  Settings-UI picker rather than hardcoded** — deliberately deferred, not
  forgotten; scope for that is real (needs the app to handle changing
  timezone *while running*, which given `time`'s local-offset soundness
  constraints — see the clock self-correction entry under App features —
  isn't just "call the lookup again").
- **App-side**: see "Clock self-correction" under App features above —
  this is what actually makes the boot-side timing variability tolerable.
  Even if NTP takes a while (or the first couple of `S45ntp` attempts
  fail and `crond`'s watchdog eventually gets WiFi re-associated), the app
  now re-derives its own displayed offset every second and corrects
  `reference_date` on whatever tick the clock actually jumps, rather than
  being frozen at whatever was true when the process started.

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
- **A symbol appearing set in Buildroot's top-level `.config` does not
  mean the package that owns it was actually rebuilt with it.** This is a
  deeper version of the "menuconfig changes don't land in `.config`"
  gotcha above — this time the setting genuinely *was* in `.config`
  (`BR2_PACKAGE_WPA_SUPPLICANT_CTRL_IFACE=y`), but `wpa_supplicant`
  itself hadn't been rebuilt since before that was set, so its *own*
  build-time `.config` still had `#CONFIG_CTRL_IFACE=y` (commented out)
  and the resulting binary genuinely had no `wpa_cli` and no control
  socket support — directly caused a boot-breaking incident (see the
  DHCP-timing section above). **Always verify against the actual built
  artifact**, not just Buildroot's `.config`: `ls output/target/...` for
  the file you expect, or check the specific package's own `.config`
  under `output/build/<pkg>-<ver>/`, or its `.stamp_built` timestamp
  against when you changed the symbol. If a symbol changed after a
  package was last built, force a rebuild explicitly (`make
  <pkg>-dirclean <pkg>`), don't assume `make` alone will notice.
- **Boot-critical `init.d` script changes are high-risk — test more
  conservatively than feels necessary.** Two real incidents this session:
  a `wpa_cli`-based change killed `wpa_supplicant` outright (config parse
  error → refuses to start) and looked identical to a boot hang, and
  earlier, a Slint UI restructuring broke touch across the *entire* app,
  not just the new feature being added. In both cases the safe recovery
  was reverting to the exact prior working state and getting the user
  unblocked *first*, diagnosing calmly afterward — not iterating further
  on a device that's currently stuck. `sh -n` every shell script before
  it goes anywhere near a rebuild; don't introduce a new command
  dependency (`wpa_cli`, `timeout`, `ping6`, `ip -s link` — none of which
  exist/work as expected on this busybox build) without confirming it's
  actually present in `output/target/` first.

## Feedback / working-style notes

- Dynamic linking of `libinput`/`libudev`/`libxkbcommon` (rather than
  building them static, or vendoring) is the deliberately agreed approach,
  not a compromise to revisit — see `feedback_static_vs_dynamic_libs`
  memory. When one person controls both the binary and the target distro,
  the usual "version mismatch" risk of dynamic linking doesn't apply, and
  these libraries have runtime *data* dependencies (hwdb, xkb data) that
  static linking wouldn't solve anyway.
- **Reaching for an Opus-model subagent for high-stakes review/
  implementation, when asked, paid off clearly** (2026-09-26/27): an
  independent Opus review of the WiFi/DHCP boot chain and the app's
  connection-handling code both corrected a wrong diagnosis (the "udhcpc
  gives up after 3 tries" theory was actually just false — it retries
  forever) and found the *real* reason a prior fix broke boot (`wpa_cli`
  genuinely doesn't exist on the image, not a hang), plus surfaced several
  additional real bugs unprompted (the connection-freeze-on-silent-drop
  issue, the `/var/log` symlink-to-tmpfs documentation error). A follow-up
  Opus agent then implemented the full fix set directly, and every claim
  it made was independently re-verified (against `busybox`/
  `wpa_supplicant` source, `sh -n`, `cargo build`/`test`) before trusting
  it — worth doing again for similarly concurrency-sensitive or
  boot-critical work, but the verification step is still what actually
  builds confidence, not the model choice alone.
- The general "verify against the actual current state of the system —
  build artifacts, live device behavior, source code — rather than trust
  docs, memory, or a config symbol in isolation" approach has repeatedly
  been what actually solved things this project (the font panic, the
  touch-misdiagnosis, the `get_states()` timing, the WiFi handshake bug,
  the DHCP timing bug, the `/var/log` correction above). Keep defaulting
  to it.
