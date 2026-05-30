# MumbledFlight

A high-fidelity spatial-audio bridge between **X-Plane 12** and a **Mumble** VoIP server, designed for the Hotstart Challenger 650 (CL650). It simulates a shared cockpit where each pilot's voice is spatialized in 3D based on their real head position, seat, and cockpit zone — making intercom, cabin speech, PA announcements, and COM radio each behave like the real aircraft's audio system.

## What it does

MumbledFlight reads cockpit state from X-Plane DataRefs (head pose, seat, ACP switches, door positions, xPilot radio flags) and drives four logical Mumble clients per pilot:

| Client | Purpose |
|--------|---------|
| **Voice** | Natural in-person speech, fully spatialized. Switches Mumble channel by zone (FBO vs. in/around aircraft). |
| **IC** | Intercom — flat equal-power stereo, simulating headphone monitoring. |
| **PA** | Public address — transmit-only speaker with realistic in-cabin attenuation. |
| **Radio** | Relays xPilot/COM radio audio captured from a loopback device into the shared channel. |

Spatial audio features include:

- Distance falloff (1.5 m → 8 m), head-relative stereo panning, and door attenuation for the cabin and lavatory doors.
- Fuselage skin-crossing model: voice from outside the aircraft arrives via hull transmission and the main-door aperture.
- Adjustable stereo width (0 = mono, 1 = natural geometry, 2 = super-stereo).
- Optional RNNoise-based noise suppression with a soft VAD gate to prevent click artifacts at speech boundaries.
- Server certificate pinning / CA verification and optional client-certificate (PKCS#12) authentication.
- Passwords and passphrases stored in the OS secret store (GNOME Keyring / KWallet / macOS Keychain / Windows Credential Manager) — never in config files.

Two pilots sharing the same **Flight ID** automatically meet in the same set of per-flight Mumble channels.

## How to use

### X-Plane plugin (normal use)

1. Install the plugin (see [Build & install](#build--install)).
2. Load a flight in X-Plane 12 with the Hotstart CL650.
3. Open the **MumbledFlight** window from the Plugins menu.
4. Fill in your Mumble server address, flight ID, and username.
5. Optionally set a server password, client certificate, or CA anchor for certificate pinning.
6. Select audio devices for ambient output, IC output, and microphone.
7. Click **Connect**. Both pilots connect with the same flight ID to share the cockpit.

Configuration (except secrets) persists to `Resources/plugins/MumbledFlight/config.toml`.

### Standalone CLI bridge (testing, no X-Plane plugin required)

The CLI polls X-Plane's Web REST API instead of reading DataRefs in-process:

```bash
# Connect to a local Mumble server using live X-Plane state
cargo run -p mumbled-flight-cli -- <FLIGHT_ID> --user Alice

# Override the Mumble server (default: 127.0.0.1:64738)
cargo run -p mumbled-flight-cli -- <FLIGHT_ID> --user Alice --server my.server:64738

# Verbose logging
RUST_LOG=debug cargo run -p mumbled-flight-cli -- <FLIGHT_ID> --user Alice
```

### CLI test modes (no X-Plane required)

```bash
# Send a 500 Hz sine wave through the Voice client at a fixed position
cargo run -p mumbled-flight-cli -- testflight --user Alice --test voice --sine --pos 2,0,-6.8

# Loop an audio file through the IC client
cargo run -p mumbled-flight-cli -- testflight --user Bob --test ic --file clip.wav

# List available audio devices
cargo run -p mumbled-flight-cli -- --list-devices
```

`--sine` and `--file` use **ffmpeg** as the audio source (must be on `PATH`). `--test` skips the X-Plane bridge entirely.

## Build & install

### System dependencies (Linux)

**Debian / Ubuntu**
```bash
sudo apt install libasound2-dev libpulse-dev libssl-dev libopus-dev \
                 libpipewire-0.3-dev libdbus-1-dev clang pkg-config
```

**Arch Linux**
```bash
sudo pacman -S alsa-lib libpulse openssl opus pipewire dbus clang pkgconf
```

PipeWire is a hard dependency on Linux for radio loopback and device enumeration.

### Build

```bash
# Build everything (debug)
cargo build

# Build the shippable plugin (release)
cargo build --release -p mumbled-flight-plugin

# Run tests
cargo test -p mumbled-flight-core

# Check for lints (expected clean)
cargo clippy --workspace
```

### Install the plugin

The plugin binary is at `target/release/libmumbled_flight_plugin.so` (Linux), `.dylib` (macOS), or `.dll` (Windows). Rename it to `MumbledFlight.xpl` and place it under your X-Plane plugins directory:

```
X-Plane 12/Resources/plugins/MumbledFlight/
  ├── lin_x64/MumbledFlight.xpl   # Linux
  ├── mac_x64/MumbledFlight.xpl   # macOS
  └── win_x64/MumbledFlight.xpl   # Windows
```

### CI / pre-built releases

GitHub Actions builds the plugin for all three platforms on every push to `main`. Download the `MumbledFlight` zip from the latest release and extract it directly into `Resources/plugins/`.

## License

MumbledFlight is free software: you can redistribute it and/or modify it under the terms of the [GNU General Public License](LICENSE) as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

Copyright (C) 2026 Zhongtai Virtual.

After cloning, activate the pre-commit hook that enforces per-file license headers:

```bash
git config core.hooksPath .githooks
```

## Acknowledgements

**PilotEdge / Keith Smith** — The concept of simulating full aircraft audio for shared-cockpit flying — positional voice, crew intercom, PA to passengers, cockpit speaker monitoring of COM radio, and proximity-based conversations in the FBO and cabin — originates from **Keith Smith** at [PilotEdge](https://www.pilotedge.net), who built this system in collaboration with the Hotstart CL650 shared cockpit feature. MumbledFlight reimplements these ideas on top of Mumble so they are accessible outside the PilotEdge network.

**Spatial audio** — The stereo spatialization math (head-basis projection, Mumble's gain curve, distance falloff, and the `spatialize()` stereo-width blend) is derived from and designed to be compatible with the [Mumble](https://github.com/mumble-voip/mumble) open-source VoIP client's positional audio model. Mumble is licensed under the BSD 2-Clause License.

**Claude** — Architecture, implementation, and iteration on this project were developed with the assistance of [Claude](https://claude.ai) (Anthropic). Spatial math, protocol integration, and the plugin GUI were all shaped through that collaboration.
