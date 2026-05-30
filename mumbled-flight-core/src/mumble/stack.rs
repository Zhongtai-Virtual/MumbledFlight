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

//! Stack startup: wires the transport-agnostic audio plumbing (mic capture, radio loopback,
//! playback mixers), then hands a [`TransportContext`] to the selected [`VoipTransport`]
//! backend. The choice of backend (Mumble / WebRTC-SFU / QUIC) is the only thing that varies
//! below this boundary — see [`crate::mumble::transport`].

use std::sync::OnceLock;

use tokio::sync::{broadcast, mpsc};

use super::audio::{create_linux_sink, start_capture, start_loopback_capture, start_playback};
use super::transport::{self, TransportContext, TransportKind};
use super::{InputType, MumbleStackConfig, TestClient};

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

    // 1. MIC Chain — capture thread bridges sync → broadcast.
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
        _ => None,
    };

    // 3. Playback Mixers — ambient and IC each route to their own output device.
    //    Radio RX shares the ambient mixer (no separate radio output).
    let (ambient_pb_tx, ambient_pb_rx) = mpsc::channel(1024);
    let (ic_pb_tx, ic_pb_rx) = mpsc::channel(1024);
    start_playback(ambient_pb_rx, ambient_output, ambient_vol);
    start_playback(ic_pb_rx, ic_output, ic_vol);

    // 4. Hand the assembled plumbing to the selected transport backend.
    let ctx = TransportContext {
        state,
        user_name,
        session_id,
        password: server_password,
        client_cert,
        server_trust,
        server_host,
        server_port,
        mic_tx,
        radio_tx,
        ambient_pb_tx,
        ic_pb_tx,
        statuses,
        spatial_width,
        test_client,
        test_pos,
    };

    let backend = transport::build(TransportKind::Mumble);
    log::info!("[VoIP] starting '{}' transport", backend.name());
    if let Err(e) = backend.run(ctx).await {
        log::error!("[VoIP] transport failed to start: {e}");
    }
}
