//! ImGui configuration window — state, construction, and input handling.

pub mod config;
pub mod devices;
mod draw;
mod window;

use std::os::raw::c_int;
use std::path::PathBuf;
use std::time::Instant;
use log::{debug, LevelFilter};

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
    screen_h: i32,
    logged_coords: bool,
    mouse_pos: [f32; 2],
    mouse_down: [bool; 5],

    pub server: String,
    pub flight_id: String,
    pub user_name: String,
    pub gain: f32,
    pub output_devices: Vec<String>,
    pub output_device_labels: Vec<String>,
    pub selected_device: i32,
    pub log_level: LevelFilter,

    // Radio relay source.
    // Index 0 = disabled, 1 = auto-sink, 2+ = input_devices[i-2].
    pub radio_input_devices: Vec<String>,
    pub selected_radio: i32,

    pub should_connect: bool,
    pub should_disconnect: bool,
    pub is_connected: bool,
    pub status: String,
}

// XPLMWindowID is *mut c_void — all XPLM + GL access is on the X-Plane main thread.
unsafe impl Send for GuiState {}

impl GuiState {
    pub fn new(auto_user: Option<String>, config_path: PathBuf) -> Self {
        let (output_devices, output_device_labels) = devices::enumerate_output_devices();
        let radio_input_devices = devices::enumerate_input_devices();
        let cfg = config::PluginConfig::load(&config_path);

        let selected_device = output_devices.iter()
            .position(|d| d == &cfg.output_device)
            .map(|i| i as i32)
            .unwrap_or(0);

        let selected_radio = match cfg.radio_source.as_str() {
            ""              => 0,
            RADIO_AUTO_SINK => 1,
            name            => radio_input_devices.iter()
                .position(|d| d == name)
                .map(|i| i as i32 + 2)
                .unwrap_or(0),
        };

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

        Self {
            window_id,
            config_path,
            imgui_ctx: None,
            imgui_renderer: None,
            last_time: Instant::now(),
            screen_h: 0,
            logged_coords: false,
            mouse_pos: [0.0; 2],
            mouse_down: [false; 5],
            server: cfg.server,
            flight_id: cfg.flight_id,
            user_name,
            gain: cfg.gain,
            output_devices,
            output_device_labels,
            selected_device,
            log_level,
            radio_input_devices,
            selected_radio,
            should_connect: false,
            should_disconnect: false,
            is_connected: false,
            status: String::new(),
        }
    }

    pub fn save_config(&self) {
        let output_device = self.output_devices
            .get(self.selected_device as usize)
            .cloned()
            .unwrap_or_default();
        let radio_source = self.radio_source_str().to_string();
        let cfg = config::PluginConfig {
            server: self.server.clone(),
            flight_id: self.flight_id.clone(),
            user_name: self.user_name.clone(),
            gain: self.gain,
            output_device,
            log_level: self.log_level.to_string().to_lowercase(),
            radio_source,
        };
        cfg.save(&self.config_path);
    }

    pub fn output_device(&self) -> Option<String> {
        self.output_devices.get(self.selected_device as usize).cloned()
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
