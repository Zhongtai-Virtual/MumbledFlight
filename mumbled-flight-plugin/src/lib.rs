//! X-Plane 12 plugin — reads DataRefs at sim rate and drives the Mumble VoIP stack.

use std::ffi::{CStr, CString};
use std::net::SocketAddr;
use std::os::raw::{c_char, c_float, c_int, c_void};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use mumbled_flight_core::{
    config::Config,
    mumble,
    state::{CockpitState, DataRefId},
};
use xplane_sys::{
    XPLMDataRef, XPLMDebugString, XPLMFindDataRef, XPLMGetDataf, XPLMGetSystemPath,
    XPLMRegisterFlightLoopCallback, XPLMUnregisterFlightLoopCallback,
};

struct PluginState {
    cockpit_state: Arc<Mutex<CockpitState>>,
    datarefs: Vec<(XPLMDataRef, DataRefId)>,
    _runtime: tokio::runtime::Runtime,
}

// XPLMDataRef is *mut c_void — only ever touched from the XPLM flight-loop thread.
unsafe impl Send for PluginState {}

static PLUGIN: OnceLock<Mutex<Option<PluginState>>> = OnceLock::new();

fn plugin_cell() -> &'static Mutex<Option<PluginState>> {
    PLUGIN.get_or_init(|| Mutex::new(None))
}

// ── XPLM entry points ────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn XPluginStart(
    out_name: *mut c_char,
    out_sig: *mut c_char,
    out_desc: *mut c_char,
) -> c_int {
    write_cstr(out_name, "MumbledFlight");
    write_cstr(out_sig, "dev.mumbling.flight");
    write_cstr(out_desc, "Spatial audio bridge for Mumble VoIP");
    1
}

#[no_mangle]
pub unsafe extern "C" fn XPluginStop() {
    *plugin_cell().lock().unwrap() = None;
}

#[no_mangle]
pub unsafe extern "C" fn XPluginEnable() -> c_int {
    let flight_id = match std::env::var("MUMBLED_FLIGHT") {
        Ok(v) => v,
        Err(_) => {
            xp_log("MumbledFlight: set MUMBLED_FLIGHT env var to your session ID\n");
            return 0;
        }
    };

    let user_name = if let Ok(u) = std::env::var("MUMBLED_USER") {
        u
    } else {
        let mut buf = vec![0i8; 512];
        XPLMGetSystemPath(buf.as_mut_ptr());
        let xp_path = CStr::from_ptr(buf.as_ptr()).to_string_lossy().to_string();
        match Config::read_cl60_config(PathBuf::from(&xp_path).as_path()) {
            Some(cfg) => cfg.user_name,
            None => {
                xp_log("MumbledFlight: cannot read CL650 config — set MUMBLED_USER\n");
                return 0;
            }
        }
    };

    let server_addr: SocketAddr = std::env::var("MUMBLED_SERVER")
        .unwrap_or_else(|_| "127.0.0.1:64738".to_string())
        .parse()
        .unwrap_or_else(|_| "127.0.0.1:64738".parse().unwrap());

    let gain: f32 = std::env::var("MUMBLED_GAIN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1.0);

    let datarefs: Vec<(XPLMDataRef, DataRefId)> = DataRefId::all()
        .iter()
        .filter_map(|&id| {
            let name = CString::new(id.name()).ok()?;
            let dr = XPLMFindDataRef(name.as_ptr());
            if dr.is_null() {
                None
            } else {
                Some((dr, id))
            }
        })
        .collect();

    xp_log(&format!(
        "MumbledFlight: user={} flight={} datarefs={}\n",
        user_name,
        flight_id,
        datarefs.len()
    ));

    let cockpit_state = Arc::new(Mutex::new(CockpitState::default()));

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let state_clone = Arc::clone(&cockpit_state);
    runtime.spawn(async move {
        mumble::run_mumble_stack(
            state_clone,
            user_name,
            flight_id,
            gain,
            false,
            None,
            false,
            false,
            None,
            server_addr,
        )
        .await;
    });

    *plugin_cell().lock().unwrap() = Some(PluginState {
        cockpit_state,
        datarefs,
        _runtime: runtime,
    });

    // 20 Hz — positive value = seconds between calls
    XPLMRegisterFlightLoopCallback(Some(flight_loop_cb), 1.0 / 20.0, std::ptr::null_mut());
    1
}

#[no_mangle]
pub unsafe extern "C" fn XPluginDisable() {
    XPLMUnregisterFlightLoopCallback(Some(flight_loop_cb), std::ptr::null_mut());
    *plugin_cell().lock().unwrap() = None;
}

#[no_mangle]
pub unsafe extern "C" fn XPluginReceiveMessage(
    _from: c_int,
    _msg: c_int,
    _param: *mut c_void,
) {
}

// ── Flight loop ───────────────────────────────────────────────────────────────

unsafe extern "C-unwind" fn flight_loop_cb(
    _elapsed: c_float,
    _elapsed_loop: c_float,
    _counter: c_int,
    _refcon: *mut c_void,
) -> c_float {
    if let Ok(mut guard) = plugin_cell().lock() {
        if let Some(ps) = guard.as_mut() {
            if let Ok(mut cs) = ps.cockpit_state.lock() {
                for &(dr, id) in &ps.datarefs {
                    cs.update_from_float(id, XPLMGetDataf(dr));
                }
            }
        }
    }
    1.0 / 20.0
}

// ── Helpers ───────────────────────────────────────────────────────────────────

unsafe fn write_cstr(dest: *mut c_char, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(255);
    for (i, &b) in bytes[..len].iter().enumerate() {
        *dest.add(i) = b as c_char;
    }
    *dest.add(len) = 0;
}

fn xp_log(s: &str) {
    if let Ok(cs) = CString::new(s) {
        unsafe { XPLMDebugString(cs.as_ptr()) };
    }
}
