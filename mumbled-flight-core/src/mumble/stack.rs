// Copyright (C) 2026 Zhongtai Virtual
//
// This file is part of MumbledFlight.
//
// MumbledFlight is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// MumbledFlight is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with MumbledFlight.  If not, see <https://www.gnu.org/licenses/>.

//! Stack startup: wires mic capture, radio loopback, playback mixers, and the four VoIP clients.

use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{broadcast, mpsc};

use super::audio::{create_linux_sink, start_capture, start_loopback_capture, start_playback};
use super::voip::client::{ClientRole, MumbleVoipClient, VoipClientStatus};
use super::{InputType, MumbleStackConfig, TestClient};
use crate::state::{CockpitState, SharedCockpitZone};

/// Spawns a single Mumble client's run loop, logging a disconnect at error level.
fn spawn_client(
    client: MumbleVoipClient,
    server_host: String,
    server_port: u16,
    state: Arc<Mutex<CockpitState>>,
    audio_rx: broadcast::Receiver<Vec<f32>>,
    playback_tx: mpsc::Sender<Vec<f32>>,
) {
    tokio::spawn(async move {
        if let Err(e) = client.run(&server_host, server_port, state, audio_rx, playback_tx).await {
            log::error!("[VoIP:{}] disconnected: {e}", client.username);
        }
    });
}

/// Returns a `Sender` into the shared radio loopback broadcast channel.
/// The underlying capture stream and forwarding thread are created only once per process
/// regardless of reconnects, so reconnecting never spawns a second loopback stream.
fn radio_loopback_sender(source_name: String) -> broadcast::Sender<Vec<f32>> {
    static TX: OnceLock<broadcast::Sender<Vec<f32>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, _) = broadcast::channel::<Vec<f32>>(128);
        let tx_fwd = tx.clone();
        std::thread::spawn(move || {
            let (sync_tx, mut sync_rx) = mpsc::channel(128);
            start_loopback_capture(source_name, sync_tx);
            while let Some(frame) = sync_rx.blocking_recv() {
                let _ = tx_fwd.send(frame);
            }
        });
        tx
    })
    .clone()
}

pub async fn run_mumble_stack(cfg: MumbleStackConfig) {
    let MumbleStackConfig {
        state,
        user_name,
        session_id,
        server_password,
        client_cert,
        server_trust,
        mic_gain,
        denoise,
        radio_source,
        auto_sink,
        test_client,
        input_type,
        mic_device,
        test_pos,
        server_host,
        server_port,
        ambient_output,
        ic_output,
        ambient_vol,
        ic_vol,
        statuses,
        shutdown,
        spatial_width,
    } = cfg;

    // Surface the unverified-server risk once per connection. With no trust anchor the TLS
    // handshake accepts any certificate (see `MumbleVoipClient::connect`), so the server's
    // identity is unauthenticated and an on-path attacker could intercept the connection —
    // including the server password sent in the Authenticate message.
    if server_trust.is_none() {
        log::warn!(
            "Connecting to {server_host}:{server_port} WITHOUT server-certificate verification \
             (no Server CA/cert configured). The server's identity is not authenticated and the \
             connection — including the server password — could be intercepted by an on-path \
             attacker. Configure a Server CA/cert anchor to verify the server."
        );
    }

    // 1. MIC Chain — capture thread bridges sync → broadcast.
    let is_synthetic_input = !matches!(input_type, InputType::Real);
    let (mic_tx, _) = broadcast::channel::<Vec<f32>>(128);
    let mic_tx_clone = mic_tx.clone();
    std::thread::spawn(move || {
        let (sync_tx, mut sync_rx) = mpsc::channel(128);
        match input_type {
            InputType::Sine => super::audio::start_sine_capture(sync_tx, mic_gain),
            InputType::File(path) => super::audio::start_file_capture(path, sync_tx, mic_gain),
            InputType::Real => start_capture(sync_tx, denoise, mic_gain, 0.0, mic_device, shutdown),
        }
        while let Some(frame) = sync_rx.blocking_recv() {
            let _ = mic_tx_clone.send(frame);
        }
    });

    // Wait for the primary mic capture to fully establish itself in the OS mixer.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 2. RADIO Chain.
    let final_radio_source = if auto_sink { create_linux_sink() } else { radio_source };
    let radio_tx = match final_radio_source {
        Some(src) if matches!(test_client, TestClient::All | TestClient::Radio) => {
            Some(radio_loopback_sender(src))
        }
        // --test radio with --sine/--file: bridge the mic chain into the radio channel so the
        // Radio client receives synthetic audio without needing a real loopback device.
        None if matches!(test_client, TestClient::Radio) && is_synthetic_input => {
            let (tx, _) = broadcast::channel::<Vec<f32>>(128);
            let tx_fwd = tx.clone();
            let mut mic_rx = mic_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    match mic_rx.recv().await {
                        Ok(frame) => { let _ = tx_fwd.send(frame); }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            Some(tx)
        }
        _ => None,
    };

    // 3. Playback Mixers — ambient and IC each route to their own output device.
    //    Radio RX shares the ambient mixer (no separate radio output).
    let (ambient_pb_tx, ambient_pb_rx) = mpsc::channel(1024);
    let (ic_pb_tx, ic_pb_rx) = mpsc::channel(1024);
    let ic_monitor_tx = ic_pb_tx.clone();
    start_playback(ambient_pb_rx, ambient_output, ambient_vol);
    start_playback(ic_pb_rx, ic_output, ic_vol);

    // Helper: allocate a status slot and register it in the shared map.
    let mk_status = |label: &str| -> Arc<Mutex<VoipClientStatus>> {
        let slot = Arc::new(Mutex::new(VoipClientStatus::Connecting));
        statuses.lock().unwrap().insert(label.to_string(), Arc::clone(&slot));
        slot
    };

    let fbo_ch      = format!("{session_id}_ambient_fbo");
    let aircraft_ch = format!("{session_id}_ambient_aircraft");
    let ambient_ctx = format!("{session_id}_ambient");

    // 4. Voice Client — natural speech, spatialized. Starts in the channel for the current zone.
    if matches!(test_client, TestClient::All | TestClient::Voice) {
        let initial_ch = match state.lock().unwrap().zone {
            SharedCockpitZone::InFbo             => fbo_ch.clone(),
            SharedCockpitZone::AroundOrInAircraft => aircraft_ch.clone(),
        };
        spawn_client(
            MumbleVoipClient {
                username:       format!("{user_name}_voice"),
                context:        ambient_ctx.clone(),
                role:           ClientRole::Voice,
                voip_status:    mk_status("Voice"),
                target_channel: initial_ch,
                zone_channels:  Some((fbo_ch.clone(), aircraft_ch.clone())),
                test_pos,
                password:       server_password.clone(),
                client_cert:    client_cert.clone(),
                server_trust:   server_trust.clone(),
                spatial_width:  Arc::clone(&spatial_width),
            },
            server_host.clone(),
            server_port,
            Arc::clone(&state),
            mic_tx.subscribe(),
            ambient_pb_tx.clone(),
        );
    }

    if test_client == TestClient::Voice {
        return;
    }

    // 5. Intercom Client.
    if !matches!(test_client, TestClient::Pa | TestClient::Radio) {
        spawn_client(
            MumbleVoipClient {
                username:       format!("{user_name}_ic"),
                context:        format!("{session_id}_ic"),
                role:           ClientRole::Ic,
                voip_status:    mk_status("IC"),
                target_channel: format!("{session_id}_ic"),
                zone_channels:  None,
                test_pos,
                password:       server_password.clone(),
                client_cert:    client_cert.clone(),
                server_trust:   server_trust.clone(),
                spatial_width:  Arc::clone(&spatial_width),
            },
            server_host.clone(),
            server_port,
            Arc::clone(&state),
            mic_tx.subscribe(),
            ic_pb_tx,
        );
    }

    // 6. PA (Public Address) Client — always in the aircraft channel.
    if !matches!(test_client, TestClient::Ic | TestClient::Radio) {
        spawn_client(
            MumbleVoipClient {
                username:       format!("{user_name}_PA"),
                context:        ambient_ctx.clone(),
                role:           ClientRole::Pa,
                voip_status:    mk_status("PA"),
                target_channel: aircraft_ch.clone(),
                zone_channels:  None,
                test_pos:       None,
                password:       server_password.clone(),
                client_cert:    client_cert.clone(),
                server_trust:   server_trust.clone(),
                spatial_width:  Arc::clone(&spatial_width),
            },
            server_host.clone(),
            server_port,
            Arc::clone(&state),
            mic_tx.subscribe(),
            ambient_pb_tx.clone(),
        );
    }

    // 7. Radio Relay Client + local COM monitor.
    if let Some(rtx) = radio_tx {
        // X-Plane cockpit-speaker position; converted to Mumble's Z convention.
        const RADIO_SPEAKER_POSITION: [f32; 3] = super::voip::xplane_to_mumble([0.0, 0.9, -6.8]);
        spawn_client(
            MumbleVoipClient {
                username:       format!("{user_name}_radio"),
                context:        ambient_ctx.clone(),
                role:           ClientRole::Radio { has_source: true },
                voip_status:    mk_status("Radio"),
                target_channel: aircraft_ch.clone(),
                zone_channels:  None,
                test_pos:       test_pos.or(Some(RADIO_SPEAKER_POSITION)),
                password:       server_password.clone(),
                client_cert:    client_cert.clone(),
                server_trust:   server_trust.clone(),
                spatial_width:  Arc::clone(&spatial_width),
            },
            server_host.clone(),
            server_port,
            Arc::clone(&state),
            rtx.subscribe(),
            ambient_pb_tx.clone(),
        );

        // Mirror radio source to the IC output so pilots monitor COM through their IC headphones.
        let mut monitor_rx = rtx.subscribe();
        tokio::spawn(async move {
            loop {
                match monitor_rx.recv().await {
                    Ok(pcm) => {
                        let stereo: Vec<f32> = pcm.iter().flat_map(|&s| [s, s]).collect();
                        let _ = ic_monitor_tx.send(stereo).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}
