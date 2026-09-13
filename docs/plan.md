# Skylight-style HA Dashboard — Rust/musl on Pi Zero 2 W

## Context

You want a family-calendar-first dashboard (Skylight Calendar-style) that runs as a
static `musl` Rust binary on a minimal custom Linux distro on a Raspberry Pi Zero 2 W,
talking to Home Assistant over the local network for its data (calendars, todo lists,
weather, and eventually Lovelace-style entity cards). You like the card-based,
glassmorphism look and HA WebSocket-driven live updates of `oyvhov/tunet`
(React/Vite dashboard, 302★, GPLv3), but want a native Rust binary instead of a
browser/Node stack, since the target hardware is a single-core-tier 512MB board with
no desktop environment.

Decisions already made with you: Slint for the UI toolkit, bare framebuffer/KMS-DRM
(no X11/Wayland compositor) as the display stack, and a touchscreen for input. This
plan lays out why Slint fits that combination, the crate architecture, the HA
integration approach, the default calendar/task view, and a phased build order that
front-loads the riskiest unknown (does a Rust GUI actually render and take touch input
on bare KMS on this exact board) before investing in app design.

## Toolkit confirmation: Slint

Slint is the right call here and I don't see a better alternative for this specific
combination (bare KMS/DRM + touch + 512MB ARM board + "make it look nice"):

- It ships a dedicated `linuxkms` backend (DRM/KMS + libinput evdev touch) built for
  exactly this class of embedded target — no compositor required.
- It has a software renderer (`renderer-software`) for when GPU acceleration isn't
  worth the complexity, plus an optional GPU-accelerated path if the VideoCore IV
  (OpenGL ES2 only) turns out to be worth using.
- `.slint` markup files are declarative and support live-reload during development,
  which matters for iterating on "make it look nice" without a full recompile+flash
  cycle each time.
- Alternatives considered: Iced's primary backend is wgpu (GPU-first, weaker fit for
  this GPU and for bare KMS); egui/LVGL-via-Rust-bindings would both need more DIY
  glue for KMS+touch and are less pleasant for building custom card widgets.
- **Caveat to flag**: Slint is GPLv3 / paid-commercial / royalty-free-license
  (not MIT). Fine for a personal open-source project; only becomes a real decision
  point if you ever want to sell devices running closed-source firmware.
- **Renderer choice given "depend on nothing on the system"**: default to Slint's
  `renderer-software` path over its GPU/EGL path. The GPU path would need the
  distro's Mesa/DRM userspace libs present and ABI-matched at runtime; the
  software renderer only needs the DRM/KMS kernel interface (which is the kernel,
  not a system library we'd be depending on) and draws everything itself. This
  keeps the binary genuinely self-contained rather than implicitly coupled to
  whatever GPU stack the distro happens to ship.

## Architecture

Cargo workspace:

```
skylight-ha/
  crates/
    ha-client/          # HA WebSocket + REST client
    dashboard-config/    # config schema + loader (Lovelace-inspired)
    ui/                  # Slint components (.slint + Rust glue)
  apps/
    skylight-ha/         # bin crate wiring it all together
```

**`ha-client`**
- WebSocket connection to `/api/websocket` for auth, `get_states`, and live
  `state_changed` subscriptions (push updates for `todo.*`, `sensor.*`,
  `weather.*`, person/presence, etc.) — auth via a Long-Lived Access Token
  (generated in the HA user profile) sent as `{"type":"auth","access_token":...}`.
- REST calls for the HA Calendar API (`GET /api/calendar/{entity_id}?start=...&end=...`)
  — calendar event ranges are **not** pushed over the WS event bus, so this needs
  periodic/range-based polling (e.g. refetch the visible month on load, on
  navigation, and every N minutes) rather than pure push.
- Todo interactions (checking off a chore from the dashboard) via WS commands
  `todo/item/list`, `todo/item/update`, `todo/item/move`.
- Reconnect-with-backoff, since a Pi on wifi *will* drop HA's connection sometimes.
- Publishes normalized state onto a `tokio::sync::broadcast` channel the UI layer
  subscribes to.

**`dashboard-config`**
- TOML config: HA base URL, token (loaded from file with restricted permissions,
  never embedded in the binary), a roster of **family members** (name, color,
  their `todo.*` entity id, optionally their calendar entity id), and a list of
  *views*, each a grid of *cards* (`Calendar`, `TodoList`, `Weather`, `Clock`,
  `EntityTile`, `Media`, ...) — deliberately a simplified, typed echo of
  Lovelace's dashboard/view/card model rather than a full YAML-card engine. The
  `TodoList` card renders one column/tab per roster member rather than a single
  merged list.

**`ui`**
- `.slint` files per card type (`calendar-card.slint`, `todo-card.slint`,
  `weather-card.slint`, `clock-card.slint`, `tile-card.slint`) plus
  `views/home.slint` for the default view and a `theme.slint` global for palette
  (family-member colors, dark/light).
- Async HA updates cross into the Slint event loop via
  `slint::invoke_from_event_loop` (Slint runs its own loop; ha-client runs on
  tokio).

## Default view (Skylight-style)

- **Shell**: a left icon sidebar (Home/Dashboard, Calendar, Tasks, Photos,
  Settings) plus a top bar (family name, clock/date, weather chip,
  per-family-member chips showing their todo completion ratio) sitting above
  whichever page is active.
- **Calendar page**: switchable between the four standard calendar views —
  **Month** (grid, one dot per event, today highlighted), **Week** and
  **Day** (hourly time-axis grid, touch-scrollable, events positioned/sized
  by actual start/end time), and **Agenda** (flat chronological list) — via a
  small segmented control. One color per family calendar/person throughout.
  Date *navigation* (prev/next week, jumping to an arbitrary day) isn't
  built yet: Month always shows the current month, Week/Day always show the
  current week/today, Agenda always shows upcoming events from today.
- **Tasks page**: todo/chore lists organized **per family member**, not one
  flat list — each person gets a column/tab (name, avatar/color) backed by
  their own `todo.*` entity (HA supports multiple todo lists, e.g.
  `todo.chores_alice`, `todo.chores_bob`), with checkable items that push
  updates back to HA. Interactive, not just a readout, since it's a family
  workflow tool.
- **Dashboard and Photos pages**: exist as nav entries now but are inert
  placeholders. Dashboard is meant to become a customizable view of HA
  entities (lights, fans, ...) — `dashboard-config`'s `Card::EntityTile` /
  `Card::Weather` variants already anticipate this, just not wired to a page
  yet. Photos is meant to eventually show an Immich library.
- Secondary HA-entity views beyond Dashboard (media, energy — tunet-style
  tiles) are architected for via the card/view system but **not built in
  v1**.

## Phased build order

1. **Feasibility spike (do first — highest risk item)**: cross-compile a Slint
   "hello world" using the `linuxkms` backend for the target triple, run it
   directly on the custom distro on real Pi Zero 2 W hardware (no X/Wayland),
   confirm DRM/KMS output *and* touchscreen input via libinput/evdev actually
   work on this board before any app-layer work starts.
2. **`ha-client`**: WS auth + reconnect, `get_states`, event subscription, REST
   calendar fetch, todo list/update. Verified via a throwaway CLI against a real
   HA instance — no UI yet.
3. **Static Home view**: hardcode the Slint calendar+todo UI wired to live
   `ha-client` data (no config system yet) to prove the full data→render path.
4. **Config-driven views**: introduce `dashboard-config`, externalize entity
   bindings/colors/card choice, add a second view to prove the card system
   generalizes beyond the hardcoded case.
5. **Polish**: theming, family color coding, transitions, offline/error states
   (cache last-known data, show a reconnect banner instead of blanking),
   bundled fonts/icons.
6. **Packaging**: static `musl` binary (via `cross` or `cargo-zigbuild`),
   systemd/OpenRC service that starts the binary straight onto the KMS surface
   at boot, logging, and a simple update path (drop-in binary replacement).

## Concerns / open items

- **Arch/bitness**: `aarch64-unknown-linux-musl` only, 64-bit-only distro as
  confirmed — no 32-bit build target, no dual-arch tooling to maintain.
- **Fully self-contained, nothing borrowed from the system**: static musl binary
  (no libc dependency), software-rendered UI (no dependency on system
  Mesa/EGL/DRM userspace libs), fonts embedded/bundled into the binary rather
  than read from a system fontconfig setup, and any icons/images baked in at
  build time rather than loaded from distro paths at runtime. The only things
  the binary talks to at runtime are the kernel (KMS/evdev) and the network
  (HA's API) — nothing else on the OS is assumed to exist.
- **RAM budget**: 512MB shared with the OS. Keep calendar data windowed (don't
  cache many months) and size bundled image/font assets for the actual panel
  resolution to avoid the self-contained-asset approach bloating memory use.
- **Calendar polling cadence**: since calendar events are REST-polled rather than
  pushed, pick a refresh interval that balances freshness against HA/network load
  (e.g. on navigation + every 5–15 min).
- **Slint license**: GPLv3 by default — fine for this project as planned, flag if
  scope ever shifts toward closed-source or selling hardware.

## Verification

- Phase 1 (spike): visual confirmation on real hardware — image renders over KMS,
  a touch on the panel is logged with correct coordinates. This is inherently
  manual; there's no automated way to verify framebuffer output.
- Phase 2 (`ha-client`): CLI smoke test against a real/dev HA instance —
  states populate, a `state_changed` event over WS updates the local cache,
  killing/restoring network triggers reconnect.
- Phase 3+ (UI): manual on-device testing is required for anything visual/touch
  — automated tests can cover `ha-client` parsing/reconnect logic and
  `dashboard-config` schema loading, but the actual "does it look nice and
  respond to touch" verification has to happen on the Pi.
