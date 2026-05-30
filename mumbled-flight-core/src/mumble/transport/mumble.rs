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

//! Mumble backend — the production transport.
//!
//! Maps the pilot onto **four** independent Mumble client connections, one per [`ClientRole`],
//! because Mumble couples "one user = membership in one channel". Each client owns its own
//! TLS control connection, UDP voice socket, Opus codec, and crypt state (see
//! [`MumbleVoipClient::run`]). This is the only place the four-connection fan-out is expressed;
//! the WebRTC and QUIC backends collapse it to a single multi-track connection.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::sync::{broadcast, mpsc};

use super::{TransportContext, VoipTransport};
use crate::mumble::voip::client::{ClientRole, MumbleVoipClient, VoipClientStatus};
use crate::mumble::TestClient;
use crate::state::{CockpitState, SharedCockpitZone};

/// The current production transport: four Mumble clients per pilot.
pub struct MumbleTransport;

impl VoipTransport for MumbleTransport {
    fn name(&self) -> &'static str {
        "mumble"
    }

    fn run(self: Box<Self>, ctx: TransportContext) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            spawn_clients(ctx);
            Ok(())
        })
    }
}

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

/// Brings up the Voice / IC / PA / Radio clients per the requested [`TestClient`] selection.
///
/// This is the four-client fan-out lifted out of the old `run_mumble_stack`; behaviour is
/// identical, it just reads its inputs from the [`TransportContext`].
fn spawn_clients(ctx: TransportContext) {
    let TransportContext {
        state,
        user_name,
        session_id,
        password,
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
    } = ctx;

    // Helper: allocate a status slot and register it in the shared map.
    let mk_status = |label: &str| -> Arc<Mutex<VoipClientStatus>> {
        let slot = Arc::new(Mutex::new(VoipClientStatus::Connecting));
        statuses.lock().unwrap().insert(label.to_string(), Arc::clone(&slot));
        slot
    };

    let fbo_ch = format!("{session_id}_ambient_fbo");
    let aircraft_ch = format!("{session_id}_ambient_aircraft");
    let ambient_ctx = format!("{session_id}_ambient");

    // 1. Voice Client — natural speech, spatialized. Starts in the channel for the current zone.
    if matches!(test_client, TestClient::All | TestClient::Voice) {
        let initial_ch = match state.lock().unwrap().zone {
            SharedCockpitZone::InFbo => fbo_ch.clone(),
            SharedCockpitZone::AroundOrInAircraft => aircraft_ch.clone(),
        };
        spawn_client(
            MumbleVoipClient {
                username: format!("{user_name}_voice"),
                context: ambient_ctx.clone(),
                role: ClientRole::Voice,
                voip_status: mk_status("Voice"),
                target_channel: initial_ch,
                zone_channels: Some((fbo_ch.clone(), aircraft_ch.clone())),
                test_pos,
                password: password.clone(),
                client_cert: client_cert.clone(),
                server_trust: server_trust.clone(),
                spatial_width: Arc::clone(&spatial_width),
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

    // 2. Intercom Client.
    if !matches!(test_client, TestClient::Pa | TestClient::Radio) {
        spawn_client(
            MumbleVoipClient {
                username: format!("{user_name}_ic"),
                context: format!("{session_id}_ic"),
                role: ClientRole::Ic,
                voip_status: mk_status("IC"),
                target_channel: format!("{session_id}_ic"),
                zone_channels: None,
                test_pos,
                password: password.clone(),
                client_cert: client_cert.clone(),
                server_trust: server_trust.clone(),
                spatial_width: Arc::clone(&spatial_width),
            },
            server_host.clone(),
            server_port,
            Arc::clone(&state),
            mic_tx.subscribe(),
            ic_pb_tx.clone(),
        );
    }

    // 3. PA (Public Address) Client — always in the aircraft channel.
    if !matches!(test_client, TestClient::Ic | TestClient::Radio) {
        spawn_client(
            MumbleVoipClient {
                username: format!("{user_name}_PA"),
                context: ambient_ctx.clone(),
                role: ClientRole::Pa,
                voip_status: mk_status("PA"),
                target_channel: aircraft_ch.clone(),
                zone_channels: None,
                test_pos: None,
                password: password.clone(),
                client_cert: client_cert.clone(),
                server_trust: server_trust.clone(),
                spatial_width: Arc::clone(&spatial_width),
            },
            server_host.clone(),
            server_port,
            Arc::clone(&state),
            mic_tx.subscribe(),
            ambient_pb_tx.clone(),
        );
    }

    // 4. Radio Relay Client + local COM monitor.
    if let Some(rtx) = radio_tx {
        // X-Plane cockpit-speaker position; converted to Mumble's Z convention.
        const RADIO_SPEAKER_POSITION: [f32; 3] =
            crate::mumble::voip::xplane_to_mumble([0.0, 0.9, -6.8]);
        spawn_client(
            MumbleVoipClient {
                username: format!("{user_name}_radio"),
                context: ambient_ctx.clone(),
                role: ClientRole::Radio { has_source: true },
                voip_status: mk_status("Radio"),
                target_channel: aircraft_ch.clone(),
                zone_channels: None,
                test_pos: test_pos.or(Some(RADIO_SPEAKER_POSITION)),
                password: password.clone(),
                client_cert: client_cert.clone(),
                server_trust: server_trust.clone(),
                spatial_width: Arc::clone(&spatial_width),
            },
            server_host.clone(),
            server_port,
            Arc::clone(&state),
            rtx.subscribe(),
            ambient_pb_tx.clone(),
        );

        // Mirror radio source to the IC output so pilots monitor COM through their IC headphones.
        let ic_monitor_tx = ic_pb_tx.clone();
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
