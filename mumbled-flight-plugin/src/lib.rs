//! X-Plane 12 plugin — GUI configuration panel + manual connect/disconnect.

mod gui;

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
    XPLMAppendMenuItem, XPLMCheckMenuItem, XPLMCreateMenu, XPLMDataRef, XPLMDataTypeID,
    XPLMDebugString, XPLMDestroyMenu, XPLMFindDataRef, XPLMFindPluginsMenu, XPLMGetDataf,
    XPLMGetDatai, XPLMGetDataRefTypes, XPLMGetSystemPath, XPLMGetWindowIsVisible, XPLMMenuCheck,
    XPLMMenuID, XPLMRegisterFlightLoopCallback, XPLMSetWindowIsVisible, XPLMTakeKeyboardFocus,
    XPLMUnregisterFlightLoopCallback,
};

// ── Plugin state ──────────────────────────────────────────────────────────────

struct MumbleConnection {
    cockpit_state: Arc<Mutex<CockpitState>>,
    _runtime: tokio::runtime::Runtime,
}

pub struct PluginState {
    datarefs: Vec<(XPLMDataRef, DataRefId, bool)>, // bool = read as int
    pending_datarefs: Vec<DataRefId>,
    pub gui: gui::GuiState,
    connection: Option<MumbleConnection>,
    menu_id: XPLMMenuID,
    retry_ticks: u32,
}

// XPLMDataRef is *mut c_void — only accessed from the XPLM main thread.
unsafe impl Send for PluginState {}

static PLUGIN: OnceLock<Mutex<Option<PluginState>>> = OnceLock::new();

pub fn plugin_cell() -> &'static Mutex<Option<PluginState>> {
    PLUGIN.get_or_init(|| Mutex::new(None))
}

// ── XPLM entry points ─────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn XPluginStart(
    out_name: *mut c_char,
    out_sig:  *mut c_char,
    out_desc: *mut c_char,
) -> c_int {
    write_cstr(out_name, "MumbledFlight");
    write_cstr(out_sig,  "dev.mumbling.flight");
    write_cstr(out_desc, "Spatial audio bridge for Mumble VoIP");
    xp_log("MumbledFlight: XPluginStart\n");
    1
}

#[no_mangle]
pub unsafe extern "C" fn XPluginStop() {
    xp_log("MumbledFlight: XPluginStop\n");
    *plugin_cell().lock().unwrap() = None;
}

unsafe fn find_dataref(id: DataRefId) -> Option<(XPLMDataRef, DataRefId, bool)> {
    let name = CString::new(id.name()).ok()?;
    let dr = XPLMFindDataRef(name.as_ptr());
    if dr.is_null() { return None; }
    let types = XPLMGetDataRefTypes(dr);
    let use_int = (types & XPLMDataTypeID::Float).0 == 0;
    Some((dr, id, use_int))
}

#[no_mangle]
pub unsafe extern "C" fn XPluginEnable() -> c_int {
    xp_log("MumbledFlight: XPluginEnable\n");

    // Auto-detect username from CL650 config; user can override in the GUI.
    let auto_user = {
        let mut buf = vec![0i8; 512];
        XPLMGetSystemPath(buf.as_mut_ptr());
        let xp_path = CStr::from_ptr(buf.as_ptr()).to_string_lossy().to_string();
        Config::read_cl60_config(PathBuf::from(&xp_path).as_path())
            .map(|c| c.user_name)
    };

    let mut datarefs: Vec<(XPLMDataRef, DataRefId, bool)> = Vec::new();
    let mut pending_datarefs: Vec<DataRefId> = Vec::new();
    for &id in DataRefId::all() {
        match find_dataref(id) {
            Some(entry) => datarefs.push(entry),
            None => pending_datarefs.push(id),
        }
    }
    xp_log(&format!("MumbledFlight: {}/{} DataRefs found at enable ({} pending)\n",
        datarefs.len(), DataRefId::all().len(), pending_datarefs.len()));

    // Build "Plugins → MumbledFlight → Show Window" menu.
    let plugins_menu = XPLMFindPluginsMenu();
    let sub_idx = XPLMAppendMenuItem(
        plugins_menu,
        b"MumbledFlight\0".as_ptr() as *const c_char,
        std::ptr::null_mut(),
        0,
    );
    let menu_id = XPLMCreateMenu(
        b"MumbledFlight\0".as_ptr() as *const c_char,
        plugins_menu,
        sub_idx,
        Some(menu_handler),
        std::ptr::null_mut(),
    );
    XPLMAppendMenuItem(
        menu_id,
        b"Show Window\0".as_ptr() as *const c_char,
        std::ptr::null_mut(),
        0,
    );
    // Start unchecked — window is hidden on startup.
    XPLMCheckMenuItem(menu_id, 0, XPLMMenuCheck::Unchecked);
    xp_log("MumbledFlight: menu created\n");

    let gui = gui::GuiState::new(auto_user);

    *plugin_cell().lock().unwrap() = Some(PluginState {
        datarefs,
        pending_datarefs,
        gui,
        connection: None,
        menu_id,
        retry_ticks: 0,
    });

    XPLMRegisterFlightLoopCallback(Some(flight_loop_cb), 1.0 / 20.0, std::ptr::null_mut());
    1
}

#[no_mangle]
pub unsafe extern "C" fn XPluginDisable() {
    xp_log("MumbledFlight: XPluginDisable\n");
    XPLMUnregisterFlightLoopCallback(Some(flight_loop_cb), std::ptr::null_mut());
    let mut guard = plugin_cell().lock().unwrap();
    if let Some(ps) = guard.as_ref() {
        XPLMDestroyMenu(ps.menu_id);
    }
    *guard = None;
}

// ── Menu callback ─────────────────────────────────────────────────────────────

unsafe extern "C-unwind" fn menu_handler(_menu_ref: *mut c_void, _item_ref: *mut c_void) {
    let Ok(mut g) = plugin_cell().lock() else { return };
    let Some(ps) = g.as_mut() else { return };
    let win = ps.gui.window_id;
    let now_visible = XPLMGetWindowIsVisible(win) != 0;
    XPLMSetWindowIsVisible(win, if now_visible { 0 } else { 1 });
    if !now_visible { XPLMTakeKeyboardFocus(win); }
    XPLMCheckMenuItem(
        ps.menu_id, 0,
        if now_visible { XPLMMenuCheck::Unchecked } else { XPLMMenuCheck::Checked },
    );
    xp_log(&format!("MumbledFlight: window {}\n",
        if now_visible { "hidden" } else { "shown" }));
}

#[no_mangle]
pub unsafe extern "C" fn XPluginReceiveMessage(
    _from: c_int, _msg: c_int, _param: *mut c_void,
) {}

// ── Flight loop — runs at 20 Hz on the XPLM main thread ──────────────────────

unsafe extern "C-unwind" fn flight_loop_cb(
    _elapsed: c_float,
    _elapsed_loop: c_float,
    _counter: c_int,
    _refcon: *mut c_void,
) -> c_float {
    let Ok(mut guard) = plugin_cell().lock() else { return 1.0 / 20.0 };
    let Some(ps) = guard.as_mut() else { return 1.0 / 20.0 };

    // 0. Retry any DataRefs not found at enable time (aircraft plugin may load later).
    //    Check every ~2 seconds (40 ticks at 20 Hz) until all are resolved.
    if !ps.pending_datarefs.is_empty() {
        ps.retry_ticks += 1;
        if ps.retry_ticks >= 40 {
            ps.retry_ticks = 0;
            let mut still_pending = Vec::new();
            let mut newly_found = 0u32;
            for &id in &ps.pending_datarefs {
                match find_dataref(id) {
                    Some(entry) => { ps.datarefs.push(entry); newly_found += 1; }
                    None => still_pending.push(id),
                }
            }
            if newly_found > 0 {
                xp_log(&format!("MumbledFlight: +{} DataRefs resolved ({} still pending)\n",
                    newly_found, still_pending.len()));
            }
            ps.pending_datarefs = still_pending;
        }
    }

    // 1. Update CockpitState from DataRefs (only when connected).
    if let Some(conn) = &ps.connection {
        if let Ok(mut cs) = conn.cockpit_state.lock() {
            for &(dr, id, use_int) in &ps.datarefs {
                let val = if use_int { XPLMGetDatai(dr) as f32 } else { XPLMGetDataf(dr) };
                cs.update_from_float(id, val);
            }
        }
    }

    // 2. React to GUI actions.
    if ps.gui.should_connect {
        xp_log("MumbledFlight: flight_loop — should_connect=true, calling start_connection\n");
        ps.gui.should_connect = false;
        start_connection(ps);
    }
    if ps.gui.should_disconnect {
        ps.gui.should_disconnect = false;
        stop_connection(ps);
    }

    1.0 / 20.0
}

// ── Connection helpers ────────────────────────────────────────────────────────

fn start_connection(ps: &mut PluginState) {
    xp_log(&format!(
        "MumbledFlight: start_connection — server='{}' user='{}' flight='{}'\n",
        ps.gui.server, ps.gui.user_name, ps.gui.flight_id
    ));
    let server_addr: SocketAddr = match ps.gui.server.parse() {
        Ok(a) => a,
        Err(e) => {
            let msg = format!("Invalid server address '{}': {e}", ps.gui.server);
            xp_log(&format!("MumbledFlight: {msg}\n"));
            ps.gui.status = msg;
            return;
        }
    };

    let cockpit_state = Arc::new(Mutex::new(CockpitState::default()));
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");

    let state_clone   = Arc::clone(&cockpit_state);
    let user_name     = ps.gui.user_name.clone();
    let flight_id     = ps.gui.flight_id.clone();
    let gain          = ps.gui.gain;
    let output_device = ps.gui.output_device();

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
            output_device,
        )
        .await;
    });

    ps.connection = Some(MumbleConnection { cockpit_state, _runtime: runtime });
    ps.gui.is_connected = true;
    ps.gui.status = format!("Connected to {}", ps.gui.server);
    xp_log(&format!("MumbledFlight: connected — user={} flight={}\n",
        ps.gui.user_name, ps.gui.flight_id));
}

fn stop_connection(ps: &mut PluginState) {
    ps.connection = None;
    ps.gui.is_connected = false;
    ps.gui.status = "Disconnected.".to_string();
    xp_log("MumbledFlight: disconnected\n");
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

pub fn xp_log(s: &str) {
    if let Ok(cs) = CString::new(s) {
        unsafe { XPLMDebugString(cs.as_ptr()) };
    }
}
