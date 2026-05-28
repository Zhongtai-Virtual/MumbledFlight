//! X-Plane 12 plugin — GUI configuration panel + manual connect/disconnect.

mod connection;
mod gui;
mod logger;

use log::{debug, info};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_float, c_int, c_void};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use mumbled_flight_core::{
    config::Config,
    state::{CockpitState, DataRefId},
};
use xplane_sys::{
    XPLMAppendMenuItem, XPLMCheckMenuItem, XPLMCreateMenu, XPLMDataRef, XPLMDataTypeID,
    XPLMDestroyMenu, XPLMFindDataRef, XPLMFindPluginsMenu, XPLMGetDataRefTypes, XPLMGetDataf,
    XPLMGetDatai, XPLMGetSystemPath, XPLMGetWindowIsVisible, XPLMMenuCheck, XPLMMenuID,
    XPLMRegisterFlightLoopCallback, XPLMSetWindowIsVisible, XPLMTakeKeyboardFocus,
    XPLMUnregisterFlightLoopCallback,
};

// ── Plugin state ──────────────────────────────────────────────────────────────

pub struct PluginState {
    datarefs: Vec<(XPLMDataRef, DataRefId, bool)>, // bool = read as int
    pending_datarefs: Vec<DataRefId>,
    pub gui: gui::GuiState,
    connection: Option<connection::MumbleConnection>,
    menu_id: XPLMMenuID,
    retry_ticks: u32,
}

// XPLMDataRef is *mut c_void — only accessed from the XPLM main thread.
unsafe impl Send for PluginState {}

impl PluginState {
    unsafe fn retry_pending_datarefs(&mut self) {
        if self.pending_datarefs.is_empty() {
            return;
        }
        self.retry_ticks += 1;
        if self.retry_ticks < 40 {
            return;
        }
        self.retry_ticks = 0;
        let pending = std::mem::take(&mut self.pending_datarefs);
        let mut newly_found = 0u32;
        for id in pending {
            match find_dataref(id) {
                Some(e) => {
                    self.datarefs.push(e);
                    newly_found += 1;
                }
                None => self.pending_datarefs.push(id),
            }
        }
        if newly_found > 0 {
            info!(
                "+{newly_found} DataRefs resolved ({} still pending)",
                self.pending_datarefs.len()
            );
        }
    }
}

// ── Global plugin cell ────────────────────────────────────────────────────────

static PLUGIN: OnceLock<Mutex<Option<PluginState>>> = OnceLock::new();

pub fn plugin_cell() -> &'static Mutex<Option<PluginState>> {
    PLUGIN.get_or_init(|| Mutex::new(None))
}

// ── XPLM entry points ─────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn XPluginStart(
    out_name: *mut c_char,
    out_sig: *mut c_char,
    out_desc: *mut c_char,
) -> c_int {
    write_cstr(out_name, "MumbledFlight");
    write_cstr(out_sig, "app.mzt.mumbled-flight");
    write_cstr(out_desc, "Spatial audio with Mumble");
    logger::init();
    info!(
        "XPluginStart v{} (built {})",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_TIMESTAMP")
    );
    1
}

#[no_mangle]
pub unsafe extern "C" fn XPluginStop() {
    info!("XPluginStop");
    *plugin_cell().lock().unwrap() = None;
}

#[no_mangle]
pub unsafe extern "C" fn XPluginEnable() -> c_int {
    info!("XPluginEnable");

    let mut buf = vec![0i8; 512];
    XPLMGetSystemPath(buf.as_mut_ptr());
    let xp_path = CStr::from_ptr(buf.as_ptr()).to_string_lossy().to_string();

    let auto_user =
        Config::read_cl60_config(PathBuf::from(&xp_path).as_path()).map(|c| c.user_name);

    let config_path = PathBuf::from(&xp_path).join("Resources/plugins/MumbledFlight/config.toml");

    let mut datarefs: Vec<(XPLMDataRef, DataRefId, bool)> = Vec::new();
    let mut pending_datarefs: Vec<DataRefId> = Vec::new();
    for &id in DataRefId::all() {
        match find_dataref(id) {
            Some(entry) => datarefs.push(entry),
            None => pending_datarefs.push(id),
        }
    }
    info!(
        "{}/{} DataRefs found at enable ({} pending)",
        datarefs.len(),
        DataRefId::all().len(),
        pending_datarefs.len()
    );

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
    XPLMCheckMenuItem(menu_id, 0, XPLMMenuCheck::Unchecked);
    debug!("menu created");

    let gui = gui::GuiState::new(auto_user, config_path);

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
    info!("XPluginDisable");
    XPLMUnregisterFlightLoopCallback(Some(flight_loop_cb), std::ptr::null_mut());
    let mut guard = plugin_cell().lock().unwrap();
    if let Some(ps) = guard.as_ref() {
        ps.gui.save_config();
        XPLMDestroyMenu(ps.menu_id);
    }
    *guard = None;
}

#[no_mangle]
pub unsafe extern "C" fn XPluginReceiveMessage(_from: c_int, _msg: c_int, _param: *mut c_void) {}

// ── Menu callback ─────────────────────────────────────────────────────────────

unsafe extern "C-unwind" fn menu_handler(_menu_ref: *mut c_void, _item_ref: *mut c_void) {
    let Ok(mut g) = plugin_cell().lock() else {
        return;
    };
    let Some(ps) = g.as_mut() else { return };
    let win = ps.gui.window_id;
    let now_visible = XPLMGetWindowIsVisible(win) != 0;
    XPLMSetWindowIsVisible(win, if now_visible { 0 } else { 1 });
    if !now_visible {
        XPLMTakeKeyboardFocus(win);
    }
    XPLMCheckMenuItem(
        ps.menu_id,
        0,
        if now_visible {
            XPLMMenuCheck::Unchecked
        } else {
            XPLMMenuCheck::Checked
        },
    );
    info!("window {}", if now_visible { "hidden" } else { "shown" });
}

// ── Flight loop — 20 Hz on the XPLM main thread ───────────────────────────────

unsafe extern "C-unwind" fn flight_loop_cb(
    _elapsed: c_float,
    _elapsed_loop: c_float,
    _counter: c_int,
    _refcon: *mut c_void,
) -> c_float {
    let Ok(mut guard) = plugin_cell().lock() else {
        return 1.0 / 20.0;
    };
    let Some(ps) = guard.as_mut() else {
        return 1.0 / 20.0;
    };

    ps.retry_pending_datarefs();
    ps.gui.refresh_output_devices();

    if let Some(conn) = &ps.connection {
        if let Ok(mut cs) = conn.cockpit_state.lock() {
            poll_datarefs(&ps.datarefs, &mut cs);
        }
    }

    if ps.gui.should_connect {
        ps.gui.should_connect = false;
        connection::start(ps);
    }
    if ps.gui.should_disconnect {
        ps.gui.should_disconnect = false;
        connection::stop(ps);
    }

    1.0 / 20.0
}

// ── DataRef helpers ───────────────────────────────────────────────────────────

unsafe fn poll_datarefs(datarefs: &[(XPLMDataRef, DataRefId, bool)], cs: &mut CockpitState) {
    for &(dr, id, use_int) in datarefs {
        let val = if use_int {
            XPLMGetDatai(dr) as f32
        } else {
            XPLMGetDataf(dr)
        };
        cs.update_from_float(id, val);
    }
}

unsafe fn find_dataref(id: DataRefId) -> Option<(XPLMDataRef, DataRefId, bool)> {
    let name = CString::new(id.name()).ok()?;
    let dr = XPLMFindDataRef(name.as_ptr());
    if dr.is_null() {
        return None;
    }
    let types = XPLMGetDataRefTypes(dr);
    let use_int = (types & XPLMDataTypeID::Float).0 == 0;
    Some((dr, id, use_int))
}

unsafe fn write_cstr(dest: *mut c_char, s: &str) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(255);
    for (i, &b) in bytes[..len].iter().enumerate() {
        *dest.add(i) = b as c_char;
    }
    *dest.add(len) = 0;
}
