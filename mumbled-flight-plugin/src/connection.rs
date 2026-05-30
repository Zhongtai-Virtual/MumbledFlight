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


//! Mumble connection lifecycle — start and stop helpers.

use log::{info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use mumbled_flight_core::{mumble, mumble::{InputType, MumbleStackConfig, TestClient, VoipStatuses}, state::CockpitState};

use crate::PluginState;

fn f32_atomic(v: f32) -> Arc<AtomicU32> {
    Arc::new(AtomicU32::new(v.to_bits()))
}

pub struct MumbleConnection {
    pub cockpit_state: Arc<Mutex<CockpitState>>,
    pub _mic_gain: Arc<AtomicU32>,
    shutdown: Arc<AtomicBool>,
    pub _runtime: tokio::runtime::Runtime,
}

impl Drop for MumbleConnection {
    fn drop(&mut self) {
        // Signal the microphone capture thread to release its stream. Runs before the
        // tokio runtime field is dropped, so capture winds down as the connection tears down.
        self.shutdown.store(true, Ordering::Release);
    }
}

pub fn start(ps: &mut PluginState) {
    info!(
        "start_connection — server='{}' port={} user='{}' flight='{}'",
        ps.gui.server, ps.gui.port, ps.gui.user_name, ps.gui.flight_id
    );

    let cockpit_state = Arc::new(Mutex::new(CockpitState::default()));
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            let msg = format!("Failed to start async runtime: {e}");
            warn!("{msg}");
            ps.gui.status = msg;
            return;
        }
    };

    let state_clone = Arc::clone(&cockpit_state);
    let user_name = ps.gui.user_name.clone();
    let server_host = ps.gui.server.clone();
    let server_port = ps.gui.port;
    let server_password = ps.gui.server_password.clone();
    let flight_id = ps.gui.flight_id.clone();
    // Each Arc is split into two handles: one given to the async stack (moved into the
    // spawned future) and one retained on ps.gui.*_live so the draw loop can adjust
    // the value live without reconnecting.
    let mic_gain     = f32_atomic(ps.gui.gain);
    let ambient_vol  = f32_atomic(ps.gui.ambient_vol);
    let ic_vol       = f32_atomic(ps.gui.ic_vol);
    let spatial_width = f32_atomic(ps.gui.spatial_width);
    let mic_gain_for_thread      = Arc::clone(&mic_gain);
    let ambient_vol_for_thread   = Arc::clone(&ambient_vol);
    let ic_vol_for_thread        = Arc::clone(&ic_vol);
    let spatial_width_for_thread = Arc::clone(&spatial_width);
    let denoise = ps.gui.denoise;
    let ambient_output = ps.gui.ambient_output();
    let ic_output      = ps.gui.ic_output();
    let mic_input      = ps.gui.mic_input();
    let (radio_source, auto_sink) = ps.gui.radio_params();

    // Optional client-certificate auth. A bad path/passphrase aborts the connect with a
    // status message instead of failing silently inside the four spawned clients.
    let client_cert = if ps.gui.cert_path.trim().is_empty() {
        None
    } else {
        match mumble::ClientCert::load(std::path::Path::new(ps.gui.cert_path.trim()), &ps.gui.cert_pass) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                let msg = format!("Client certificate error: {e}");
                warn!("{msg}");
                ps.gui.status = msg;
                return;
            }
        }
    };

    // Server-certificate verification. An explicit Server CA/cert wins; otherwise fall back to
    // the TOFU-pinned cert remembered for this server (the draw loop only sets `should_connect`
    // once the user has trusted it, so a missing pin here means the user opted to stay unverified).
    let server_trust = if !ps.gui.server_ca.trim().is_empty() {
        match mumble::ServerTrust::load(std::path::Path::new(ps.gui.server_ca.trim())) {
            Ok(t) => Some(Arc::new(t)),
            Err(e) => {
                let msg = format!("Server CA error: {e}");
                warn!("{msg}");
                ps.gui.status = msg;
                return;
            }
        }
    } else {
        let key = crate::gui::known_hosts::KnownHosts::key(&ps.gui.server, ps.gui.port);
        match ps.gui.known_hosts.get(&key) {
            Some(pem) => match mumble::ServerTrust::from_pem(pem.clone().into_bytes()) {
                Ok(t) => Some(Arc::new(t)),
                Err(e) => {
                    let msg = format!(
                        "Stored server certificate for {key} is corrupt: {e}. \
                         Reconnect to re-trust the server."
                    );
                    warn!("{msg}");
                    ps.gui.status = msg;
                    return;
                }
            },
            None => None,
        }
    };

    let statuses: VoipStatuses = Arc::new(Mutex::new(HashMap::new()));
    let statuses_clone = Arc::clone(&statuses);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_stack = Arc::clone(&shutdown);

    // Captured before `server_trust` is moved into the stack: when no anchor is configured the
    // server's TLS certificate is not verified, so warn the user in the status area.
    let server_unverified = server_trust.is_none();

    runtime.spawn(async move {
        mumble::run_mumble_stack(MumbleStackConfig {
            state: state_clone,
            server_password,
            client_cert,
            server_trust,
            user_name,
            session_id: flight_id,
            mic_gain: mic_gain_for_thread,
            denoise,
            radio_source,
            auto_sink,
            test_client: TestClient::default(),
            input_type: InputType::Real,
            mic_device: mic_input,
            test_pos: None,
            server_host,
            server_port,
            ambient_output,
            ic_output,
            ambient_vol: ambient_vol_for_thread,
            ic_vol: ic_vol_for_thread,
            statuses: statuses_clone,
            shutdown: shutdown_for_stack,
            spatial_width: spatial_width_for_thread,
        })
        .await;
    });

    ps.gui.mic_gain_live    = Some(Arc::clone(&mic_gain));
    ps.gui.ambient_vol_live = Some(Arc::clone(&ambient_vol));
    ps.gui.ic_vol_live      = Some(Arc::clone(&ic_vol));
    ps.gui.spatial_width_live = Some(spatial_width);
    ps.gui.voip_statuses = Some(statuses);
    ps.connection = Some(MumbleConnection {
        cockpit_state,
        _mic_gain: mic_gain,
        shutdown,
        _runtime: runtime,
    });
    ps.gui.is_connected = true;
    ps.gui.status = if server_unverified {
        "Warning: server identity unverified — set a Server CA to authenticate the server.".to_string()
    } else {
        String::new()
    };
    ps.gui.save_config();
    info!(
        "connected — user={} flight={}",
        ps.gui.user_name, ps.gui.flight_id
    );
}

pub fn stop(ps: &mut PluginState) {
    ps.connection = None;
    ps.gui.mic_gain_live       = None;
    ps.gui.ambient_vol_live    = None;
    ps.gui.ic_vol_live         = None;
    ps.gui.spatial_width_live  = None;
    ps.gui.voip_statuses = None;
    ps.gui.is_connected = false;
    ps.gui.status = String::new();
    info!("disconnected");
}
