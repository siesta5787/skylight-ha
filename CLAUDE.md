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
  probe (all debug scaffolding removed except the tap-counter box, kept in
  the bottom-right corner as a standing touch sanity check). This should be
  considered resolved on the Pi too, pending final hardware validation —
  the same panel, same `backend-linuxkms-noseat` code path, same libinput
  version behavior.

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
  - Still needed: `etc/skylight/config.toml` (and optionally
    `ha-token.secret`) baked into the overlay — so far only tested via live
    `scp` to the running device, not yet made permanent.
- **eero mesh WiFi issue (unresolved, deprioritized)**: association
  consistently succeeds but the WPA handshake times out, across all three
  mesh BSSIDs identically. Kernel/driver logs a `Disconnect event of DFS
  AP` even though this chip is 2.4GHz-only (no real DFS band) — suspected
  mislabeled driver message actually reflecting eero's mesh
  Channel-Switch-Announcement handling confusing `brcmfmac`. Not a
  password/PMF issue (both explicitly ruled out). USB Ethernet is the
  working network path for now.

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
