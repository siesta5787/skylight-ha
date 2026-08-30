# Skylight HA

A calendar-and-tasks family dashboard for Home Assistant, built as a static
Rust binary that runs directly on bare framebuffer/KMS-DRM (no X11/Wayland)
on a Raspberry Pi Zero 2 W. See `docs/plan.md` for the full architecture and
rationale.

## Layout

- `crates/ha-client` — Home Assistant WebSocket + REST client.
- `crates/dashboard-config` — TOML config schema (HA connection, family
  roster, views/cards).
- `crates/ui` — Slint UI components (`.slint` markup + generated Rust).
- `apps/skylight-ha` — the binary that wires it all together.

## Dev loop (desktop, not the Pi)

```
cp config.example.toml config.toml
echo "your-long-lived-access-token" > ha-token.secret
cargo run -p skylight-ha -- config.toml
```

This uses Slint's default `backend-winit`, so it opens a normal desktop
window — useful for iterating on layout/data-binding without touching
hardware.

## Building for the Pi

Target is `aarch64-unknown-linux-musl` (64-bit only). The on-device build
swaps to the `backend-linuxkms` UI feature (bare DRM/KMS + libinput evdev
touch, still the software renderer) instead of `backend-winit`:

```
cargo build --release --target aarch64-unknown-linux-musl \
  -p skylight-ha --no-default-features -F ui/backend-linuxkms
```

Built via `.github/workflows/build-pi.yml` on GitHub Actions: a QEMU-emulated
aarch64 Alpine container, since Alpine's `apk` packages are musl-native
(no manual cross-sysroot assembly needed for libinput/libudev). Alpine
doesn't ship *static* builds of libinput/libudev/libxkbcommon though, so
`.cargo/config.toml` disables `crt-static` for this target: the binary is
still self-contained for everything that matters (our code, fonts, all
pure-Rust dependencies) but dynamically links musl's own libc plus those
three small hardware-input libraries — all of which any musl-based distro
already provides as its foundation. Expect a from-scratch CI run to take
multiple hours (QEMU emulation of a large dependency tree); the GH Actions
cache didn't reliably speed up repeat runs in practice (each took 1-3.5h
regardless), so budget for that every time the workflow changes.

**Status: builds successfully.** Trigger via `gh workflow run build-pi.yml`,
grab the `skylight-ha-aarch64-linux-musl` artifact once it finishes
(`gh run download <run-id> -n skylight-ha-aarch64-linux-musl`). Verified as a
real `aarch64` ELF binary (`file` reports `ELF 64-bit LSB pie executable, ARM
aarch64 ... dynamically linked, interpreter /lib/ld-musl-aarch64.so.1`).

Runtime dependencies the target distro must provide (not yet set up, no
device tested against yet):
- musl's own libc + dynamic loader (`ld-musl-aarch64.so.1`) — standard on
  any musl-based distro.
- `libinput.so`, `libudev.so`/`eudev.so`, `libxkbcommon.so`.
- udev's hardware database (hwdb/rules) so libinput can identify the
  touchscreen, and xkb keymap data (normally under `/usr/share/X11/xkb`) so
  libxkbcommon can load a keyboard layout — these are runtime *data*
  dependencies, separate from the linked code, needed regardless of how the
  binary links against the libraries.
- Permission to open `/dev/dri/*` directly (e.g. running as root), since the
  build uses Slint's `backend-linuxkms-noseat` (no seatd-style broker).

## Status

Early scaffold, verified running (desktop dev window, `backend-winit`): the
month calendar renders with real dates but no HA events yet, a live clock and
date update every second, and a todo column renders per configured family
member (currently empty — HA data isn't wired in yet). The `ha-client` crate
connects and authenticates against a real HA instance but its data isn't
pushed into the UI yet. The `aarch64-musl` release binary builds
successfully in CI (see above) but hasn't been run on a Pi yet. See
`docs/plan.md`'s phased build order for what's next — the very next real
milestone is actually running the binary over bare KMS with touch on real
Pi Zero 2 W hardware, before more app-layer work.

Known rough edges (functional, not blocking): the default window size is a
little too small to show every calendar row and both todo columns without
clipping — real sizing needs to match the target display's actual
resolution, which isn't chosen yet. The header clock is left-packed next to
the date rather than pushed to the right edge, working around a Slint
software-renderer quirk with stretch/alignment-positioned elements on this
dev machine (see `docs/plan.md` history / project memory for details) —
revisit during the polish phase.
