//! Mumble connection lifecycle — start and stop helpers.

use log::{info, warn};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use mumbled_flight_core::{mumble, state::CockpitState};

use crate::PluginState;

pub struct MumbleConnection {
    pub cockpit_state: Arc<Mutex<CockpitState>>,
    pub _runtime: tokio::runtime::Runtime,
}

pub fn start(ps: &mut PluginState) {
    info!("start_connection — server='{}' user='{}' flight='{}'",
        ps.gui.server, ps.gui.user_name, ps.gui.flight_id);

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
    let runtime       = tokio::runtime::Runtime::new().expect("tokio runtime");

    let state_clone   = Arc::clone(&cockpit_state);
    let user_name     = ps.gui.user_name.clone();
    let flight_id     = ps.gui.flight_id.clone();
    let gain                    = ps.gui.gain;
    let output_device           = ps.gui.output_device();
    let (radio_source, auto_sink) = ps.gui.radio_params();

    runtime.spawn(async move {
        mumble::run_mumble_stack(
            state_clone, user_name, flight_id, gain,
            false, radio_source, auto_sink, false, None,
            server_addr, output_device,
        ).await;
    });

    ps.connection = Some(MumbleConnection { cockpit_state, _runtime: runtime });
    ps.gui.is_connected = true;
    ps.gui.status = format!("Connected to {}", ps.gui.server);
    ps.gui.save_config();
    info!("connected — user={} flight={}", ps.gui.user_name, ps.gui.flight_id);
}

pub fn stop(ps: &mut PluginState) {
    ps.connection = None;
    ps.gui.is_connected = false;
    ps.gui.status = "Disconnected.".to_string();
    info!("disconnected");
}
