# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

MumbledFlight is a high-fidelity spatial-audio bridge between **X-Plane 12** (specifically the
Hotstart Challenger 650 / CL650) and a **Mumble** VoIP server. It simulates a shared cockpit:
each pilot's head position/orientation, seat, cockpit zone, ACP (Audio Control Panel) switches,
doors, and xPilot radio state are read from X-Plane DataRefs and used to drive spatialized,
multi-channel Mumble audio so that voice, intercom (IC), public address (PA), and relayed radio
each behave like the real aircraft's audio system.

See `README.md` for user-facing build and install instructions. This file is the primary architecture reference.

## Workspace layout

Cargo workspace (`resolver = "2"`) with three crates:

| Crate | Type | Role |
|-------|------|------|
| `mumbled-flight-core` | lib | All the real logic: cockpit state, Mumble VoIP stack, spatial-audio math, audio I/O. Platform-agnostic except Linux PipeWire bits. |
| `mumbled-flight-cli` | bin (`mumbled-flight`) | Standalone bridge for testing/dev. Polls X-Plane's Web REST API for DataRefs; rich `--test` / `--sine` / `--file` debugging flags. |
| `mumbled-flight-plugin` | cdylib (`.xpl`) | The real X-Plane 12 plugin. Reads DataRefs in-process via the XPLM SDK; ImGui config window. Built and shipped as `MumbledFlight.xpl`. |

Both the CLI and the plugin are thin frontends that gather config + cockpit state and call the
single entry point `mumble::run_mumble_stack(...)` in core.

## Build / test / run

```bash
# Build everything
cargo build

# Build the shippable plugin (release)
cargo build --release -p mumbled-flight-plugin

# Build + run the standalone CLI bridge
cargo run -p mumbled-flight-cli -- <FLIGHT_ID> [flags]

# Tests (unit tests live in mumbled-flight-core)
cargo test -p mumbled-flight-core

# Run a single test by name
cargo test -p mumbled-flight-core stereo_gains_pan_toward_source_side
```

Unit tests currently cover the two pure modules, `spatial.rs` (gain curve, door attenuation,
position round-trip, stereo panning/falloff, fuselage AABB, point falloff, the skin-crossing
model) and `state.rs` (seat-gated control ownership, `from_int` boundaries, the f32 update path,
and a DataRef name uniqueness/round-trip guard for the "three places" convention). The audio I/O
and protocol layers are not yet tested. `cargo clippy --workspace` is expected to be **clean** —
keep it that way.

> Building/testing `mumbled-flight-core` pulls in CPAL → ALSA and PipeWire, so the Linux system
> deps below must be installed or `cargo test` fails at the `alsa-sys` build script.

### Linux build/runtime system deps
`libasound2-dev libpulse-dev libssl-dev libopus-dev libpipewire-0.3-dev libdbus-1-dev clang pkg-config`
(`libdbus-1-dev` is for the plugin's keyring/Secret Service backend)
(PipeWire is a hard dependency on Linux — radio loopback and device enumeration use the native
PW API.)

### CLI debugging (no X-Plane required)
The CLI's `--test` flag spawns a single client type with a pre-configured cockpit state and
**skips the X-Plane bridge**, so you can exercise the audio stack against a local Mumble server:

```bash
# 500 Hz sine into the Voice client at a fixed position
cargo run -p mumbled-flight-cli -- testflight --user Alice --test voice --sine --pos 2,0,-6.8

# Loop an audio file through the IC client
cargo run -p mumbled-flight-cli -- testflight --user Bob --test ic --file clip.wav

# List audio devices
cargo run -p mumbled-flight-cli -- --list-devices
```

`--sine` / `--file` shell out to **ffmpeg** as the mic source. Default server is `127.0.0.1` port `64738`
(override with `--server host` and `--port port`). Set `RUST_LOG=debug` (env_logger) for verbose tracing.

## Architecture — the big picture

### 1. Cockpit state is the single source of truth (`core/src/state.rs`)
`CockpitState` is a plain struct holding head pose, seat, role, zone, PTT switches, mic selector,
speaker/IC volumes, door positions (`door` cabin, `door_lav` lavatory, `door_main` main entry), and xPilot radio flags. It is wrapped in `Arc<Mutex<_>>` and
shared across every async client.

- `DataRefId` is the **canonical enum of every X-Plane DataRef** the app cares about. Each variant
  maps 1:1 to a DataRef name string via `DataRefId::name()`. **When adding a new DataRef, add the
  variant, its `name()` arm, AND include it in `DataRefId::all()`** — both frontends iterate
  `all()` to discover/poll DataRefs, so omitting it there silently drops the field.
- `CockpitState::update_from_float(id, val)` is the **single update path** — a raw f32 DataRef
  value mutates state here. The plugin reads f32/i32 XPLM handles and calls it directly; the CLI
  bridge converts its JSON values to f32 first (`xplane.rs::value_to_f32`). Seat-dependent gating
  uses `CockpitState::owns(id)`: ACP1/contwheel0 belong to the **left (Captain) seat**,
  ACP2/contwheel1 to the **right (First Officer) seat**. `owns()` is the one place that resolves
  "which physical control belongs to me" — paired ACP1/ACP2 arms share a single match arm gated on it.
- **Transmit-gating predicates** (`should_transmit_ic`, `should_transmit_pa`, `should_transmit_radio`) live on `CockpitState` — not in the protocol layer. If a client is keying up when it shouldn't, check here first.
- `CockpitState::f32_to_bool(v)` is `pub` — use it whenever a DataRef float needs to be interpreted as a boolean (threshold > 0.1).
- Aircraft-specific identifiers (`CL650/...`, `xpilot/...`) are CL650-coupled. Generalizing to
  other aircraft means abstracting these names.

### 2. Two ways to feed state in (the only difference between frontends)
- **Plugin** (`plugin/src/lib.rs`): registers a 20 Hz XPLM flight-loop callback. `XPluginEnable`
  resolves DataRef handles via `XPLMFindDataRef`; unresolved ones go to `pending_datarefs` and are
  retried (~every 40 ticks) because aircraft DataRefs only exist once the CL650 is loaded. Each
  tick `poll_datarefs` reads handles and calls `cs.update_from_float`. All XPLM/GL access happens
  on the X-Plane main thread; the global `PluginState` lives in a `OnceLock<Mutex<Option<_>>>`.
- **CLI** (`cli/src/xplane.rs`): polls X-Plane's **Web REST API** at
  `http://localhost:8086/api/v3` — first GETs `/datarefs` to map names→numeric IDs (filtered
  through `DataRefId::from_name`), then polls each `/datarefs/{id}/value` on a ticker. Auto-retries
  on error.

### 3. `run_mumble_stack` fans out into multiple Mumble clients (`core/src/mumble/stack.rs`)
This is the heart of the system. Types and the `MumbleStackConfig` struct are declared in
`core/src/mumble/mod.rs`; the implementation lives in `stack.rs`. Both frontends call
`run_mumble_stack` with a single `MumbleStackConfig` (state, identity, host, port, devices, radio
source, test mode, `statuses` map — see the struct for fields). One real microphone capture is
broadcast (tokio `broadcast` channel) to **four logical Mumble clients**, each a separate TLS+UDP
connection with its own username suffix and Mumble channel, spawned through the `spawn_client`
helper in `stack.rs`:

| `ClientRole` | Username | Channel | Purpose |
|--------------|----------|---------|---------|
| `Voice` | `<user>_voice` | `<flight>_ambient_fbo` / `_aircraft` | Natural in-person speech, **spatialized**, switches channel by cockpit **zone** (FBO vs in/around aircraft). |
| `Ic` | `<user>_ic` | `<flight>_ic` | Intercom. **Non-spatial**, flat equal-power stereo (simulated headphones). |
| `Pa` | `<user>_PA` | `<flight>_ambient_aircraft` | Public address. TX-only speaker. |
| `Radio` | `<user>_radio` | `<flight>_ambient_aircraft` | Relays xPilot/COM radio audio captured from a loopback device. TX-only. |

- Channels are **per-flight-ID** so partners meet by sharing the same Flight ID.
- `TestClient` lets a frontend spawn just one role (used by CLI `--test`).
- Mixers: there are two playback sinks — **ambient** (Voice + PA + Radio RX) and **IC** — routed to
  separate output devices. The Radio source is also mirrored into the IC output so pilots monitor
  COM through their IC headphones.
- The radio loopback capture stream is created **once per process** via a `OnceLock`
  (`radio_loopback_sender`) regardless of reconnects.

### 4. The Mumble protocol state machine (`core/src/mumble/voip/`)
- `client.rs` — `MumbleVoipClient` holds the *static* per-client config (role, channel, context,
  fixed test position, `spatial_width: Arc<AtomicU32>`). `run()` is the tokio `select!` event loop:
  TCP/UDP pings, zone-channel checks, inbound UDP voice, inbound TLS control messages, outbound mic
  PCM. Authentication: each client sends the optional Mumble server `password` in its
  `Authenticate` message; for the TLS identity it uses an optional user-supplied client
  certificate (`ClientCert`, PKCS#12 / `.p12`, validated eagerly on load) when provided, otherwise
  a self-signed throwaway identity per connection (`generate_temp_identity`). Server-cert
  verification: the connector sets `danger_accept_invalid_certs`/`_hostnames(true)` to complete the
  handshake, then — if a `ServerTrust` anchor is configured (the server's cert for pinning, or its
  CA) — verifies the server's presented cert against *only* those anchors with openssl
  (`X509StoreContext`). Done this way because native-tls can't drop the system trust store. With no
  anchor, the server cert is **not** verified at this layer — so callers warn / gate the connection:
  `run_mumble_stack` logs a `warn!` once per connection when `server_trust` is `None`, the CLI also
  prints an unconditional `eprintln!` (its logger is error-only by default), and the plugin instead
  implements **TOFU** — it probes the server cert on connect (`probe_server_cert`), asks the user to
  trust the fingerprint, pins it (`known_hosts.toml` → `ServerTrust::from_pem`), and warns on change
  (see §6). `probe_server_cert`/`cert_fingerprint` are the reusable TOFU primitives in `client.rs`.
  Audio is Opus, 48 kHz mono, position info attached to each voice packet.
  Connection: uses the idiomatic `(host, port)` pattern for `TcpStream::connect` and `UdpSocket::connect`, which robustly handles hostname resolution and IPv6 formatting. UDP sockets are dynamically bound to the correct address family (`0.0.0.0:0` or `[::]:0`) to support IPv6 connectivity.
  `spatialize()` applies a stereo-width blend after all gain/attenuation math:
  `mid + (L/R − mid) × width`, where `width` is read from `spatial_width` at each packet.
  Values in [0, 1] blend toward mono; values in (1, 2] exaggerate the stereo spread beyond
  the natural geometry. Output gains are clamped to ≥ 0 to prevent phase inversion.
- `session.rs` — `Session` is the *mutable* per-connection state (crypt, channel map, decoders,
  encoder, sequence counters). Key methods:
  - `on_mic_pcm` — **the transmit-gating logic.** Delegates to `CockpitState::should_transmit_*`
    predicates (defined in `state.rs`) to decide whether *this* client should transmit. Sends a
    final "end" frame on PTT release (`was_transmitting`).
  - `on_udp_recv` — decode + route inbound audio. PA/Radio clients are TX-only and ignore RX
    (the Voice client already covers RX for those channels). IC applies `ic_vol`/`ic_tog` gating
    and plays flat; everything else is spatialized.
  - `check_zone_channel` / `on_user_state` — the Voice client's zone→channel switching. It
    requests creation of a temporary channel, retries the move every tick until the **server
    echoes** the move back (no silent failure). `PermissionDenied` on creation means it already
    exists.
- `spatial.rs` — **pure math, no I/O.** `compute_stereo_gains` builds the listener's head basis
  (forward/top/right from psi/the/phi), projects the source direction onto the right vector
  (`calc_gain`, Mumble's curve), applies distance falloff (1.5 m→8 m), and `door_attenuation` for
  the cabin and lavatory doors. `aircraft_skin_gains` handles voice sources crossing the fuselage
  boundary: two summed paths (hull transmission 0.15 + two-step door aperture: scalar `point_gain`
  from source to `MAIN_DOOR_POS`, then full stereo from door to listener). `pa_gain` computes the
  scalar PA attenuation for a listener position: full in cabin, cabin-door-attenuated in cockpit,
  and seeded from the gain at the main-door opening when outside (so the level is continuous through
  the doorway). `is_inside_aircraft` tests a Mumble-coord position against the CL650 fuselage AABB.
  Also Opus encode/decode and position byte (de)serialization.

> **Coordinate convention (gotcha):** X-Plane is **aft-positive Z**, Mumble is **forward-positive
> Z** — they differ only in the sign of Z. Convert at every X-Plane↔Mumble boundary with the single
> `spatial::xplane_to_mumble(pos)` helper (it is its own inverse) rather than negating Z by hand;
> it is used in `position_bytes`, `spatialize`, the CLI `--pos` parser, and `RADIO_SPEAKER_POSITION`.

### 5. Audio I/O (`core/src/mumble/audio.rs`)
CPAL-based capture/playback. Device selection goes through `select_device(name, input) -> Option<cpal::Device>`, which is `#[cfg]`-split: on Linux it steers PipeWire via `PIPEWIRE_NODE` and finds the `"pipewire"` CPAL device; on macOS/Windows it iterates CPAL devices by exact name, falling back to the system default. `None` means no device was found; callers log an error and return rather than panicking. All device-open calls are serialised under `device_open_lock` (on Linux this also guards the `set_var` race; elsewhere it just serialises concurrent opens).

`OPUS_FRAME_SAMPLES` (960) is a `pub const` in `mumble::mod` — use it everywhere a frame size is needed; do not hard-code 960.

Linux additionally provides `enumerate_pw_devices` (single PipeWire round-trip for sinks +
sources) and `create_linux_sink` / `start_loopback_capture` for the `MumblingRadio` virtual sink
used to capture xPilot's radio output — all `#[cfg(target_os = "linux")]`. On macOS/Windows,
`start_loopback_capture` uses a standard CPAL `build_input_stream` instead (for virtual audio
cables such as BlackHole or VB-Cable). `mic_gain` / `ambient_vol` / `ic_vol` / `spatial_width`
are all `Arc<AtomicU32>` (f32 bits) so the GUI can adjust them live without reconnecting.

**Denoise** (`--denoise` flag / GUI checkbox): RNNoise via `nnnoiseless`, 480-sample (10 ms)
frames. Gain is applied *after* `process_frame` so the denoiser always sees a normalised signal.
`process_frame` returns a VAD probability used as a **soft gate**: frames at or above `VAD_THRESHOLD`
(0.5, module-level const in `audio.rs`) pass at full gain; frames below are attenuated by
`gain * vad / VAD_THRESHOLD`, approaching zero smoothly rather than stepping to silence — avoiding
click artifacts at speech boundaries. The denoiser is stateful and created once per capture stream;
the flag cannot be toggled while connected.

### 6. Plugin GUI (`plugin/src/gui/`)
ImGui window rendered via `imgui-glow-renderer`. `GuiState` (in `gui/mod.rs`) holds all UI/config
state and is the bridge between user input and `connection::start/stop`. Config persists to
`Resources/plugins/MumbledFlight/config.toml` (`gui/config.rs`, serde) — **except secrets** (the
server password and client-cert passphrase), which go to the **OS secret store** via `gui/secrets.rs`
(`keyring`: freedesktop Secret Service on Linux — GNOME Keyring / KWallet / KeePassXC — Keychain on
macOS, Credential Manager on Windows). Legacy plain-text secrets in an existing toml are read once,
migrated to the store, and cleared. Device enumeration runs on
a **background thread** (every 2 s, panic-caught) and is applied non-blocking in the flight loop —
deliberately kept off the XPLM main thread so it never blocks `XPluginEnable`.

**Radio source combo** is platform-conditional via `RADIO_DEVICE_OFFSET`: on Linux index 0 =
disabled, 1 = auto-sink (`__auto__`), 2+ = input devices; on macOS/Windows index 0 = disabled,
1+ = input devices (auto-sink entry is hidden). `radio_params()`, `radio_source_str()`, and
`refresh_output_devices()` all use the same offset constant.

**Draw loop structure** (`gui/draw/`): the drawing code is a directory module split into four files. `draw/mod.rs` owns the orchestration of the **main config window** — `GuiState::draw` snapshots mutable fields into locals via `.take()` or `.clone()`, renders via imgui, then writes them back (necessary to avoid borrow conflicts with `imgui::Ui`) — plus the `pub(super)` renderer helpers `init_imgui` and `make_gl` shared by all three draw methods. The rendering logic is split into methods on a local `struct Ctx<'ui> { ui, fw }` (defined in `draw/widgets.rs`) that carries the shared draw-time state. `draw/widgets.rs` holds `Ctx` and its reusable primitives (`row`, `password_row`, `file_row`, `slider`, `reset_icon_button`, `combo`, `draw_modal_dim`, `fit_label`); `draw/panels.rs` holds the config panels (`connection_fields`, `audio_controls`, `denoise_toggle`, `output_device_pickers`, `mic_picker`, `radio_picker`, `log_level_picker`, `connect_button`, `status_display`) plus the `BrowseClicks` return struct; `draw/file_picker.rs` holds the file-browser UI (`file_picker_content`, `FilePick`, `start_dir`) **and** `GuiState::draw_file_picker` — the XPLM window frame setup for the file-picker popup; `draw/tofu.rs` holds the TOFU probe/decide logic (`start_probe`, `poll_step`, `render_decision`, `TrustView`/`TrustChoice`/`TofuWindowAction`) **and** `GuiState::draw_tofu` — the XPLM window frame setup for the trust popup. Primitives and panels are all methods on `Ctx`, declared `pub(super)` so the sibling modules can call across files. The shared `LABEL_COL_X` constant lives in `draw/widgets.rs`.

**Single imgui context** (critical invariant): imgui-rs only allows **one** `imgui::Context` per thread. `GuiState` holds a single `main_imgui: ImguiWindowState` that is shared by all three XPLM windows (main, file-picker, TOFU). Each window's draw callback (`draw`, `draw_file_picker`, `draw_tofu`) runs a complete imgui frame (`ctx.frame()` → `ctx.render()`) using `main_imgui.ctx`/`main_imgui.renderer`. This is safe because X-Plane's draw callbacks fire sequentially and all XPLM windows share one GL context. **Never add a second `ImguiWindowState` with its own `imgui::Context`** — it will panic the moment it tries to `create()` while the first context is alive.

**TOFU trust-on-connect** (SSH `known_hosts` analogue): clicking **Connect** with an empty `Server CA` does *not* connect immediately. `draw/mod.rs` spawns a background `mumble::probe_server_cert(host, port)` (a blocking TLS handshake) tagged with a monotonic `PROBE_GEN`; the result lands in `GuiState::probe_slot` and is polled each frame. The `GuiState::trust_state` machine (`TrustState::Idle | Probing | Decide`) drives `trust_modal`: an **Unknown** server shows the SHA-256 fingerprint with **Trust**/**Cancel**; an unchanged cert (matches the pinned fingerprint via `mumble::cert_fingerprint`) connects silently; a **Changed** cert shows old+new fingerprints and a MITM warning; a probe failure shows the error. **Trust** persists the cert PEM to `GuiState::known_hosts` (`gui/known_hosts.rs`, a `host:port → PEM` store in `known_hosts.toml` beside `config.toml`) and sets `should_connect`. On the next connect `connection::start` resolves `server_trust` from the pinned cert (`ServerTrust::from_pem`) — an explicit `Server CA` still wins over the TOFU pin. A pinned cert that no longer matches fails verification in the client (connection refused) until re-trusted. `draw_modal_dim` covers the background while any modal is open.

**Connection fields layout**: required fields first (Server → Flight ID → Username), then an `Optional auth & security` collapsing header (Password, Cert Pass, Client Cert, Server CA). The header defaults open when any of those four fields is non-empty (`[&str; 4]` typed array + `.any(|s| !s.is_empty())`). File-path fields (Client Cert, Server CA) use `file_row`, which renders a text input + `...` browse button.

**Sliders** — Voice Vol, IC Vol, Mic Gain, and Spatial all go through `Ctx::slider` (min, max,
`SliderFlags`, default). Double-clicking any slider resets it to its default. The Spatial slider
(`spatial_width`, 0–2, linear, `NO_INPUT`) controls stereo width: 0 = mono, 1 = natural geometry
(default), 2 = super-stereo. It is live-adjustable via `spatial_width_live: Option<Arc<AtomicU32>>`
— same pattern as `mic_gain_live`, `ambient_vol_live`, `ic_vol_live`.

**Window size**: default 630×600 (`left: 60, top: 660, right: 690, bottom: 60` in `window.rs`).

**X-Plane 12 plugin path**: install as `Resources/plugins/MumbledFlight/lin_x64/MumbledFlight.xpl` (Linux), `mac_x64/`, `win_x64/`. The old XP10/11 `64/lin.xpl` layout is not used.

## Conventions & gotchas

- **DataRef changes touch three places** in `state.rs` (variant, `name()`, `all()`) — see §1.
- **Transmit-gating predicates live in `state.rs`** (`should_transmit_ic/pa/radio`). **Routing and
  decode live in `session.rs`** (`on_mic_pcm`, `on_udp_recv`). If a client keys when it shouldn't,
  check `state.rs`; if audio routes to the wrong output, check `session.rs`.
- **`f32_atomic(v)` in `connection.rs`** — use this helper whenever you need `Arc<AtomicU32>` from
  an `f32`; do not repeat `Arc::new(AtomicU32::new(v.to_bits()))` inline.
- The plugin must export `XPlugin*` symbols (`#[no_mangle] extern "C"`); callbacks invoked by
  X-Plane use `extern "C-unwind"`. XPLM handle types are raw pointers, hand-marked `Send`, and
  must only be touched on the main thread.
- `build.rs` in the plugin injects `BUILD_TIMESTAMP` (chrono) used in the startup log line. It uses `rerun-if-changed=__force_rerun__` (a non-existent path) so Cargo always re-runs it and the timestamp is never stale.
- The `--denoise` flag / GUI checkbox enables **RNNoise** noise suppression (via `nnnoiseless`)
  on the capture side in `audio.rs::start_capture`. Gain is applied *after* denoising; VAD uses
  a soft gate (see §5) to avoid click artifacts. The denoiser is stateful and created once per
  capture stream; it threads from both frontends → `run_mumble_stack` → `start_capture` only
  (it is *not* a per-client concern).

## License

Licensed under **GPL-3.0-or-later**. Copyright (C) 2026 Zhongtai Virtual.

- Every `.rs` source file must begin with the standard GPL copyright + copying-permission block. A git pre-commit hook (`.githooks/pre-commit`) enforces this automatically — run `git config core.hooksPath .githooks` once after cloning to activate it.
- The `license = "GPL-3.0-or-later"` field must be present in every crate's `[package]` section.
- The full license text lives in `LICENSE`.

## CI / release (`.github/workflows/build.yml`)
Matrix build of the plugin on Linux/macOS/Windows on push to `main`, PRs, and manual dispatch.
Each platform produces `MumbledFlight.xpl` under the X-Plane plugin dir layout
(`lin_x64` / `mac_x64` / `win_x64`); a `bundle` job zips all three into `MumbledFlight` — extract
into `Resources/plugins/` to install. macOS uses stub XPLM/XPWidgets frameworks with
`-undefined dynamic_lookup`; Windows resolves OpenSSL/Opus via vcpkg; Linux installs the system
deps listed above.
