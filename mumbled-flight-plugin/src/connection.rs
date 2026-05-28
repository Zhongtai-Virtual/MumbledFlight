//! Mumble connection lifecycle — start and stop helpers.

use log::{info, warn};
use std::net::SocketAddr;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use mumbled_flight_core::{mumble, mumble::TestClient, state::CockpitState};

use crate::PluginState;

pub struct MumbleConnection {
    pub cockpit_state: Arc<Mutex<CockpitState>>,
    pub _mic_gain: Arc<AtomicU32>,
    pub _runtime: tokio::runtime::Runtime,
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
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    let state_clone = Arc::clone(&cockpit_state);
    let user_name = ps.gui.user_name.clone();
    let flight_id = ps.gui.flight_id.clone();
    let mic_gain = Arc::new(AtomicU32::new(ps.gui.gain.to_bits()));
    let mic_gain_for_thread = Arc::clone(&mic_gain);
    let denoise = ps.gui.denoise;
    let ambient_output = ps.gui.ambient_output();
    let ic_output = ps.gui.ic_output();
    let (radio_source, auto_sink) = ps.gui.radio_params();

    runtime.spawn(async move {
        mumble::run_mumble_stack(
            state_clone,
            user_name,
            flight_id,
            mic_gain_for_thread,
            denoise,
            radio_source,
            auto_sink,
            TestClient::default(),
            false,
            None,
            server_addr,
            ambient_output,
            ic_output,
        )
        .await;
    });

    ps.gui.mic_gain_live = Some(Arc::clone(&mic_gain));
    ps.connection = Some(MumbleConnection {
        cockpit_state,
        _mic_gain: mic_gain,
        _runtime: runtime,
    });
    ps.gui.is_connected = true;
    ps.gui.status = format!("Connected to {}", ps.gui.server);
    ps.gui.save_config();
    info!(
        "connected — user={} flight={}",
        ps.gui.user_name, ps.gui.flight_id
    );
}

pub fn stop(ps: &mut PluginState) {
    ps.connection = None;
    ps.gui.mic_gain_live = None;
    ps.gui.is_connected = false;
    ps.gui.status = "Disconnected.".to_string();
    info!("disconnected");
}
