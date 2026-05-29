//! Mumble connection lifecycle — start and stop helpers.

use log::{info, warn};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use mumbled_flight_core::{mumble, mumble::{InputType, MumbleStackConfig, TestClient, VoipStatuses}, state::CockpitState};

use crate::PluginState;

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
        "start_connection — server='{}' user='{}' flight='{}'",
        ps.gui.server, ps.gui.user_name, ps.gui.flight_id
    );

    let server_addr: SocketAddr = match ps.gui.server.parse() {
        Ok(a) => a,
        Err(e) => {
            let msg = format!("Invalid server address '{}': {e}", ps.gui.server);
            warn!("{msg}");
            ps.gui.status = msg;
            return;
        }
    };

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
    let server_password = ps.gui.server_password.clone();
    let flight_id = ps.gui.flight_id.clone();
    let mic_gain = Arc::new(AtomicU32::new(ps.gui.gain.to_bits()));
    let mic_gain_for_thread = Arc::clone(&mic_gain);
    let ambient_vol = Arc::new(AtomicU32::new(ps.gui.ambient_vol.to_bits()));
    let ambient_vol_for_thread = Arc::clone(&ambient_vol);
    let ic_vol = Arc::new(AtomicU32::new(ps.gui.ic_vol.to_bits()));
    let ic_vol_for_thread = Arc::clone(&ic_vol);
    let denoise = ps.gui.denoise;
    let ambient_output = ps.gui.ambient_output();
    let ic_output      = ps.gui.ic_output();
    let mic_input      = ps.gui.mic_input();
    let (radio_source, auto_sink) = ps.gui.radio_params();
    let spatial_width = Arc::new(AtomicU32::new(ps.gui.spatial_width.to_bits()));

    let statuses: VoipStatuses = Arc::new(Mutex::new(HashMap::new()));
    let statuses_clone = Arc::clone(&statuses);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_stack = Arc::clone(&shutdown);

    let spatial_width_for_thread = Arc::clone(&spatial_width);
    runtime.spawn(async move {
        mumble::run_mumble_stack(MumbleStackConfig {
            state: state_clone,
            server_password,
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
            server_addr,
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
    ps.gui.status = String::new();
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
