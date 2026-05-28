# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

MumbledFlight is a high-fidelity spatial-audio bridge between **X-Plane 12** (specifically the
Hotstart Challenger 650 / CL650) and a **Mumble** VoIP server. It simulates a shared cockpit:
each pilot's head position/orientation, seat, cockpit zone, ACP (Audio Control Panel) switches,
doors, and xPilot radio state are read from X-Plane DataRefs and used to drive spatialized,
multi-channel Mumble audio so that voice, intercom (IC), public address (PA), and relayed radio
each behave like the real aircraft's audio system.

There is **no README**. This file is the primary architecture reference.

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

# Tests (note: there is currently no test suite in-tree)
cargo test
```

There are **no unit/integration tests in the repository yet**. `spatial.rs` and `state.rs` are
pure and the obvious place to add them.

### Linux build/runtime system deps
`libasound2-dev libpulse-dev libssl-dev libopus-dev libpipewire-0.3-dev clang pkg-config`
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

`--sine` / `--file` shell out to **ffmpeg** as the mic source. Default server is `127.0.0.1:64738`
(override with `--server host:port`). Set `RUST_LOG=debug` (env_logger) for verbose tracing.

## Architecture — the big picture

### 1. Cockpit state is the single source of truth (`core/src/state.rs`)
`CockpitState` is a plain struct holding head pose, seat, role, zone, PTT switches, mic selector,
speaker/IC volumes, door positions, and xPilot radio flags. It is wrapped in `Arc<Mutex<_>>` and
shared across every async client.

- `DataRefId` is the **canonical enum of every X-Plane DataRef** the app cares about. Each variant
  maps 1:1 to a DataRef name string via `DataRefId::name()`. **When adding a new DataRef, add the
  variant, its `name()` arm, AND include it in `DataRefId::all()`** — both frontends iterate
  `all()` to discover/poll DataRefs, so omitting it there silently drops the field.
- `CockpitState::update_from_dataref` is where a raw DataRef value mutates state. Seat-dependent
  logic lives here: ACP1/contwheel0 apply only when in the **left (Captain) seat**, ACP2/contwheel1
  only in the **right (First Officer) seat**. This is the one place that resolves "which physical
  control belongs to me."
- Aircraft-specific identifiers (`CL650/...`, `xpilot/...`) are CL650-coupled. Generalizing to
  other aircraft means abstracting these names.

### 2. Two ways to feed state in (the only difference between frontends)
- **Plugin** (`plugin/src/lib.rs`): registers a 20 Hz XPLM flight-loop callback. `XPluginEnable`
  resolves DataRef handles via `XPLMFindDataRef`; unresolved ones go to `pending_datarefs` and are
  retried (~every 40 ticks) because aircraft DataRefs only exist once the CL650 is loaded. Each
  tick `poll_datarefs` reads handles and calls `cs.update_from_float`. All XPLM/GL access happens
  on the X-Plane main thread; the global `PluginState` lives in a `OnceLock<Mutex<Option<_>>>`.
- **CLI** (`cli/src/xplane/bridge.rs`): polls X-Plane's **Web REST API** at
  `http://localhost:8086/api/v3` — first GETs `/datarefs` to map names→numeric IDs (filtered
  through `DataRefId::from_name`), then polls each `/datarefs/{id}/value` on a ticker. Auto-retries
  on error.

### 3. `run_mumble_stack` fans out into multiple Mumble clients (`core/src/mumble/mod.rs`)
This is the heart of the system. One real microphone capture is broadcast (tokio `broadcast`
channel) to **four logical Mumble clients**, each a separate TLS+UDP connection with its own
username suffix and Mumble channel:

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
  fixed test position). `run()` is the tokio `select!` event loop: TCP/UDP pings, zone-channel
  checks, inbound UDP voice, inbound TLS control messages, outbound mic PCM. It generates a
  self-signed throwaway TLS identity per connection (`generate_temp_identity`). Audio is Opus,
  48 kHz mono, position info attached to each voice packet.
- `session.rs` — `Session` is the *mutable* per-connection state (crypt, channel map, decoders,
  encoder, sequence counters). Key methods:
  - `on_mic_pcm` — **the transmit-gating logic.** Decides whether *this* client should transmit
    based on cockpit state. RT (radio transmit) takes priority over IC; PA needs the mic selector
    on PA + RT keyed; Radio needs `com1_rx||com2_rx` + speaker on + not a shared-cockpit guest.
    Sends a final "end" frame on PTT release (`was_transmitting`).
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
  the cabin and lavatory doors. Also Opus encode/decode and position byte (de)serialization.

> **Coordinate convention (gotcha):** X-Plane is **aft-positive Z**, Mumble is **forward-positive
> Z**. The Z axis is negated at every X-Plane↔Mumble boundary (see `position_bytes`, `spatialize`,
> the CLI `--pos` parser, and the `RADIO_SPEAKER_POSITION` constant). Keep this consistent when
> touching positions.

### 5. Audio I/O (`core/src/mumble/audio.rs`)
CPAL-based capture/playback plus Linux PipeWire device enumeration (`enumerate_pw_devices`) and the
auto-created virtual sink `MumblingRadio` (`VIRTUAL_SINK_NAME`, `create_linux_sink`) used to
capture xPilot's radio output. `std::env::set_var(PIPEWIRE_NODE)` + CPAL device-open is serialized
under a process-wide mutex (`pipewire_env_lock`) because `set_var` is unsound across threads.
`mic_gain` is an `Arc<AtomicU32>` (f32 bits) so the GUI can adjust gain live.

### 6. Plugin GUI (`plugin/src/gui/`)
ImGui window rendered via `imgui-glow-renderer`. `GuiState` (in `gui/mod.rs`) holds all UI/config
state and is the bridge between user input and `connection::start/stop`. Config persists to
`Resources/plugins/MumbledFlight/config.toml` (`gui/config.rs`, serde). Device enumeration runs on
a **background thread** (every 2 s, panic-caught) and is applied non-blocking in the flight loop —
deliberately kept off the XPLM main thread so it never blocks `XPluginEnable`. The radio source
combo encodes: index 0 = disabled, 1 = auto-sink (`__auto__`), 2+ = an input device.

## Conventions & gotchas

- **DataRef changes touch three places** in `state.rs` (variant, `name()`, `all()`) — see §1.
- **Transmit/receive routing rules live entirely in `session.rs`** (`on_mic_pcm`, `on_udp_recv`).
  If audio is going to the wrong place or not gating correctly, start there, not in the audio
  engine.
- The plugin must export `XPlugin*` symbols (`#[no_mangle] extern "C"`); callbacks invoked by
  X-Plane use `extern "C-unwind"`. XPLM handle types are raw pointers, hand-marked `Send`, and
  must only be touched on the main thread.
- `build.rs` in the plugin injects `BUILD_TIMESTAMP` (chrono) used in the startup log line.
- The `--denoise` CLI flag / WebRTC audio-processing path is currently **not wired** (the param is
  `_denoise` in `audio.rs`); don't assume noise suppression is active.

## CI / release (`.github/workflows/build.yml`)
Matrix build of the plugin on Linux/macOS/Windows on push to `main`, PRs, and manual dispatch.
Each platform produces `MumbledFlight.xpl` under the X-Plane plugin dir layout
(`lin_x64` / `mac_x64` / `win_x64`); a `bundle` job zips all three into `MumbledFlight` — extract
into `Resources/plugins/` to install. macOS uses stub XPLM/XPWidgets frameworks with
`-undefined dynamic_lookup`; Windows resolves OpenSSL/Opus via vcpkg; Linux installs the system
deps listed above.
