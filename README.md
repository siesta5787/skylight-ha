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

Cross-compiling `aarch64-unknown-linux-musl` from this machine needs a musl
cross linker (`cross`/Docker, or `cargo-zigbuild`) — not yet set up here.

## Status

Early scaffold, verified running (desktop dev window, `backend-winit`): the
month calendar renders with real dates but no HA events yet, a live clock and
date update every second, and a todo column renders per configured family
member (currently empty — HA data isn't wired in yet). The `ha-client` crate
connects and authenticates against a real HA instance but its data isn't
pushed into the UI yet. See `docs/plan.md`'s phased build order for what's
next — the very next real milestone is running a Slint hello-world over bare
KMS with touch on actual Pi Zero 2 W hardware, before more app-layer work.

Known rough edges (functional, not blocking): the default window size is a
little too small to show every calendar row and both todo columns without
clipping — real sizing needs to match the target display's actual
resolution, which isn't chosen yet. The header clock is left-packed next to
the date rather than pushed to the right edge, working around a Slint
software-renderer quirk with stretch/alignment-positioned elements on this
dev machine (see `docs/plan.md` history / project memory for details) —
revisit during the polish phase.
