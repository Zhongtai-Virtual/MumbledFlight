//! ImGui configuration window — state, construction, and input handling.

pub mod config;
pub mod devices;
mod draw;
mod window;

use mumbled_flight_core::mumble::audio::enumerate_pw_devices;
use mumbled_flight_core::mumble::VoipStatuses;
use std::os::raw::c_int;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};
use log::{debug, LevelFilter};

struct DeviceSnapshot {
    output_names:  Vec<String>,
    output_labels: Vec<String>,
    input_names:   Vec<String>,
}

use xplane_sys::{XPLMMouseStatus, XPLMTakeKeyboardFocus, XPLMWindowID};

/// Sentinel stored in config when the auto-sink option is selected.
pub const RADIO_AUTO_SINK: &str = "__auto__";

pub struct GuiState {
    pub window_id: XPLMWindowID,
    config_path: PathBuf,

    // Lazily initialised on first draw — GL context guaranteed active then.
    imgui_ctx: Option<imgui::Context>,
    imgui_renderer: Option<imgui_glow_renderer::AutoRenderer>,

    last_time: Instant,
    pending_devices: Arc<Mutex<Option<DeviceSnapshot>>>,
    screen_h: i32,
    logged_coords: bool,
    mouse_pos: [f32; 2],
    mouse_down: [bool; 5],

    pub server: String,
    pub flight_id: String,
    pub user_name: String,
    pub gain: f32,
    pub denoise: bool,
    pub output_devices: Vec<String>,
    pub output_device_labels: Vec<String>,
    pub selected_ambient: i32,
    pub selected_ic: i32,
    pub log_level: LevelFilter,

    // Radio relay source.
    // Index 0 = disabled, 1 = auto-sink, 2+ = input_devices[i-2].
    pub radio_input_devices: Vec<String>,
    pub selected_radio: i32,
    // Config-preferred device names — used on the first snapshot arrival to resolve indices
    // when the list was empty at construction time (avoids blocking the XPLM thread).
    initial_ambient_device: String,
    initial_ic_device: String,
    initial_radio_source: String,

    pub should_connect: bool,
    pub should_disconnect: bool,
    pub is_connected: bool,
    pub status: String,
    /// Per-client connection statuses — None when disconnected.
    pub voip_statuses: Option<VoipStatuses>,
    /// Shared with the capture thread while connected — None when disconnected.
    pub mic_gain_live: Option<Arc<AtomicU32>>,
}

// XPLMWindowID is *mut c_void — all XPLM + GL access is on the X-Plane main thread.
unsafe impl Send for GuiState {}

impl GuiState {
    pub fn new(auto_user: Option<String>, config_path: PathBuf) -> Self {
        let cfg = config::PluginConfig::load(&config_path);
        // Device lists start empty — the background thread populates them immediately
        // off the XPLM main thread to avoid blocking XPluginEnable.
        let output_devices       = vec![];
        let output_device_labels = vec![];
        let radio_input_devices  = vec![];
        let selected_ambient = 0;
        let selected_ic      = 0;
        let selected_radio   = 0;

        let log_level = match cfg.log_level.to_lowercase().as_str() {
            "error" => LevelFilter::Error,
            "warn"  => LevelFilter::Warn,
            "debug" => LevelFilter::Debug,
            _       => LevelFilter::Info,
        };
        log::set_max_level(log_level);

        let user_name = if cfg.user_name.is_empty() {
            auto_user.unwrap_or_default()
        } else {
            cfg.user_name
        };

        let window_id = unsafe { window::create_xplm_window() };

        let pending_devices: Arc<Mutex<Option<DeviceSnapshot>>> = Arc::new(Mutex::new(None));
        {
            // Background thread enumerates devices immediately then refreshes every 2 s.
            // Holds a Weak ref — exits automatically when GuiState (and the Arc) is dropped.
            let weak = Arc::downgrade(&pending_devices);
            std::thread::spawn(move || loop {
                let Some(arc) = weak.upgrade() else { return };
                #[cfg(target_os = "linux")]
                let (output_names, output_labels, input_names) = {
                    use mumbled_flight_core::mumble::audio::VIRTUAL_SINK_NAME;
                    // Single PW round-trip for both sinks and sources on Linux.
                    let (sinks, sources) = enumerate_pw_devices();
                    let sinks: Vec<_> = sinks.into_iter()
                        .filter(|s| s.name != VIRTUAL_SINK_NAME)
                        .collect();
                    let names  = sinks.iter().map(|s| s.name.clone()).collect();
                    let labels = sinks.into_iter().map(|s| s.description).collect();
                    (names, labels, sources)
                };
                #[cfg(not(target_os = "linux"))]
                let (output_names, output_labels, input_names) = {
                    let (names, labels) = devices::enumerate_output_devices();
                    let inputs = devices::enumerate_input_devices();
                    (names, labels, inputs)
                };
                *arc.lock().unwrap() = Some(DeviceSnapshot { output_names, output_labels, input_names });
                std::thread::sleep(Duration::from_secs(2));
            });
        }

        Self {
            window_id,
            config_path,
            imgui_ctx: None,
            imgui_renderer: None,
            last_time: Instant::now(),
            pending_devices,
            screen_h: 0,
            logged_coords: false,
            mouse_pos: [0.0; 2],
            mouse_down: [false; 5],
            server: cfg.server,
            flight_id: cfg.flight_id,
            user_name,
            gain: cfg.gain,
            denoise: cfg.denoise,
            output_devices,
            output_device_labels,
            selected_ambient,
            selected_ic,
            log_level,
            radio_input_devices,
            selected_radio,
            initial_ambient_device: cfg.ambient_device,
            initial_ic_device: cfg.ic_device,
            initial_radio_source: cfg.radio_source,
            should_connect: false,
            should_disconnect: false,
            is_connected: false,
            status: String::new(),
            voip_statuses: None,
            mic_gain_live: None,
        }
    }

    pub fn save_config(&self) {
        let device_name = |idx: i32| self.output_devices.get(idx as usize).cloned().unwrap_or_default();
        let cfg = config::PluginConfig {
            server: self.server.clone(),
            flight_id: self.flight_id.clone(),
            user_name: self.user_name.clone(),
            gain: self.gain,
            denoise: self.denoise,
            ambient_device: device_name(self.selected_ambient),
            ic_device:      device_name(self.selected_ic),
            log_level: self.log_level.to_string().to_lowercase(),
            radio_source: self.radio_source_str().to_string(),
        };
        cfg.save(&self.config_path);
    }

    fn device_at(&self, idx: i32) -> Option<String> {
        self.output_devices.get(idx as usize).filter(|s| !s.is_empty()).cloned()
    }
    pub fn ambient_output(&self) -> Option<String> { self.device_at(self.selected_ambient) }
    pub fn ic_output(&self)      -> Option<String> { self.device_at(self.selected_ic) }

    /// Apply a pending device snapshot produced by the background refresh thread.
    /// Non-blocking — the flight loop calls this at 20 Hz with no pactl overhead.
    pub fn refresh_output_devices(&mut self) {
        let snap = self.pending_devices.lock().unwrap().take();
        let Some(snap) = snap else { return };

        let changed = snap.output_names != self.output_devices
            || snap.input_names != self.radio_input_devices;
        if changed {
            debug!("device refresh: sinks={:?} inputs={:?}", snap.output_names, snap.input_names);
        }

        let resolve = |cur: Option<String>| cur
            .and_then(|name| snap.output_names.iter().position(|d| *d == name))
            .map(|i| i as i32)
            .unwrap_or(0);
        // On the first refresh the live lists are empty, so use the config-preferred names.
        let ambient_name = if self.output_devices.is_empty() {
            Some(self.initial_ambient_device.clone())
        } else {
            self.ambient_output()
        };
        let ic_name = if self.output_devices.is_empty() {
            Some(self.initial_ic_device.clone())
        } else {
            self.ic_output()
        };
        self.selected_ambient = resolve(ambient_name);
        self.selected_ic      = resolve(ic_name);
        self.output_devices       = snap.output_names;
        self.output_device_labels = snap.output_labels;

        let current_radio = if self.radio_input_devices.is_empty() {
            self.initial_radio_source.clone()
        } else {
            self.radio_source_str().to_string()
        };
        self.selected_radio = match current_radio.as_str() {
            ""              => 0,
            RADIO_AUTO_SINK => 1,
            name            => snap.input_names.iter()
                .position(|d| d == name)
                .map(|i| i as i32 + 2)
                .unwrap_or(0),
        };
        self.radio_input_devices = snap.input_names;
    }

    /// Returns `(radio_source, auto_sink)` for passing to `run_mumble_stack`.
    pub fn radio_params(&self) -> (Option<String>, bool) {
        match self.selected_radio {
            0 => (None, false),
            1 => (None, true),
            i => (self.radio_input_devices.get(i as usize - 2).cloned(), false),
        }
    }

    fn radio_source_str(&self) -> &str {
        match self.selected_radio {
            0 => "",
            1 => RADIO_AUTO_SINK,
            i => self.radio_input_devices
                .get(i as usize - 2)
                .map(|s| s.as_str())
                .unwrap_or(""),
        }
    }

    // ── Input handlers ────────────────────────────────────────────────────────

    pub fn on_mouse(&mut self, win: XPLMWindowID, x: c_int, y: c_int, status: XPLMMouseStatus) {
        self.mouse_pos = [x as f32, (self.screen_h - y) as f32];
        if status == XPLMMouseStatus::Down {
            self.mouse_down[0] = true;
            unsafe { XPLMTakeKeyboardFocus(win); }
            debug!("mouse down xplm=({x},{y}) imgui=({:.0},{:.0}) screen_h={}",
                self.mouse_pos[0], self.mouse_pos[1], self.screen_h);
        } else if status == XPLMMouseStatus::Up {
            self.mouse_down[0] = false;
        }
    }

    pub fn on_mouse_move(&mut self, x: i32, y: i32) {
        self.mouse_pos = [x as f32, (self.screen_h - y) as f32];
    }

    pub fn on_wheel(&mut self, x: i32, y: i32, axis: i32, clicks: i32) {
        self.on_mouse_move(x, y);
        let Some(ctx) = &mut self.imgui_ctx else { return };
        let io = ctx.io_mut();
        if axis == 0 { io.mouse_wheel   += clicks as f32; }
        else         { io.mouse_wheel_h += clicks as f32; }
    }

    pub fn on_char(&mut self, key: u8) {
        let Some(ctx) = &mut self.imgui_ctx else { return };
        let io = ctx.io_mut();
        match key {
            8    => {
                io.add_key_event(imgui::Key::Backspace, true);
                io.add_key_event(imgui::Key::Backspace, false);
            }
            32.. => io.add_input_character(key as char),
            _    => {}
        }
    }
}
