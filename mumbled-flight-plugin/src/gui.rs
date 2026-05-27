//! ImGui configuration window rendered into an XPLMCreateWindowEx floating panel.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::time::Instant;
use log::{debug, warn, LevelFilter};
use serde::{Deserialize, Serialize};

use cpal::traits::{DeviceTrait, HostTrait};
use xplane_sys::{
    XPLMCreateWindowEx, XPLMGetScreenBoundsGlobal, XPLMGetScreenSize, XPLMGetWindowGeometry,
    XPLMMouseStatus, XPLMSetGraphicsState, XPLMSetWindowPositioningMode, XPLMSetWindowTitle,
    XPLMTakeKeyboardFocus, XPLMWindowDecoration, XPLMWindowID, XPLMWindowLayer,
    XPLMWindowPositioningMode,
};

// ── Persisted config ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct PluginConfig {
    server: String,
    flight_id: String,
    user_name: String,
    gain: f32,
    output_device: String,
    log_level: String,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            server: "127.0.0.1:64738".to_string(),
            flight_id: String::new(),
            user_name: String::new(),
            gain: 1.0,
            output_device: String::new(),
            log_level: "info".to_string(),
        }
    }
}

impl PluginConfig {
    fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, path: &PathBuf) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match toml::to_string_pretty(self) {
            Ok(s) => { let _ = std::fs::write(path, s); }
            Err(e) => warn!("config save failed: {e}"),
        }
    }
}

// ── Draw helpers ─────────────────────────────────────────────────────────────

/// Truncate `text` with `...` so it fits within `max_px` using ImGui's font metrics.
fn fit_label<'a>(ui: &imgui::Ui, text: &'a str, max_px: f32) -> std::borrow::Cow<'a, str> {
    if ui.calc_text_size(text)[0] <= max_px {
        return std::borrow::Cow::Borrowed(text);
    }
    let ell_w = ui.calc_text_size("...")[0];
    let avail = (max_px - ell_w).max(0.0);
    let mut end = text.len();
    while end > 0 {
        while end > 0 && !text.is_char_boundary(end) { end -= 1; }
        if ui.calc_text_size(&text[..end])[0] <= avail { break; }
        end -= 1;
    }
    std::borrow::Cow::Owned(format!("{}...", &text[..end]))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn pactl_sink_descriptions() -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(out) = std::process::Command::new("pactl").args(["list", "sinks"]).output() else {
        return map;
    };
    let Ok(s) = std::str::from_utf8(&out.stdout) else { return map };
    let mut current_name: Option<String> = None;
    for line in s.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("Name: ") {
            current_name = Some(name.to_string());
        } else if let Some(desc) = line.strip_prefix("Description: ") {
            if let Some(name) = current_name.take() {
                map.insert(name, desc.to_string());
            }
        }
    }
    map
}

// ── Public state ──────────────────────────────────────────────────────────────

pub struct GuiState {
    pub window_id: XPLMWindowID,
    config_path: PathBuf,

    // Lazily initialised on first draw call (GL context guaranteed active then).
    imgui_ctx: Option<imgui::Context>,
    imgui_renderer: Option<imgui_glow_renderer::AutoRenderer>,

    last_time: Instant,
    screen_h: i32,       // cached for Y-flip in mouse coord conversion
    logged_coords: bool, // emit coordinate-space diagnostics once on first draw
    mouse_pos: [f32; 2],
    mouse_down: [bool; 5],

    // Config fields
    pub server: String,
    pub flight_id: String,
    pub user_name: String,
    pub gain: f32,
    pub output_devices: Vec<String>,
    pub output_device_labels: Vec<String>,
    pub selected_device: i32,
    pub log_level: LevelFilter,

    // Actions / status
    pub should_connect: bool,
    pub should_disconnect: bool,
    pub is_connected: bool,
    pub status: String,
}

// XPLMWindowID is *mut c_void; all XPLM + GL access is on the X-Plane main thread.
unsafe impl Send for GuiState {}

impl GuiState {
    pub fn new(auto_user: Option<String>, config_path: PathBuf) -> Self {
        let mut output_devices: Vec<String> = cpal::default_host()
            .output_devices()
            .map(|it| it.filter_map(|d| d.name().ok()).collect())
            .unwrap_or_default();

        // Append PipeWire/PulseAudio sinks not visible to ALSA (e.g. Bluetooth, virtual sinks).
        // Also collect descriptions for friendlier display names.
        #[cfg(target_os = "linux")]
        let descriptions = {
            let d = pactl_sink_descriptions();
            if let Ok(out) = std::process::Command::new("pactl").args(["list", "short", "sinks"]).output() {
                if let Ok(s) = std::str::from_utf8(&out.stdout) {
                    for sink in s.lines().filter_map(|l| l.split_whitespace().nth(1)) {
                        if !output_devices.iter().any(|d| d == sink) {
                            output_devices.push(sink.to_string());
                        }
                    }
                }
            }
            d
        };
        #[cfg(not(target_os = "linux"))]
        let descriptions = std::collections::HashMap::<String, String>::new();
        let output_device_labels: Vec<String> = output_devices.iter()
            .map(|name| descriptions.get(name).cloned().unwrap_or_else(|| name.clone()))
            .collect();

        let cfg = PluginConfig::load(&config_path);

        let selected_device = output_devices.iter()
            .position(|d| d == &cfg.output_device)
            .map(|i| i as i32)
            .unwrap_or(0);

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

        let window_id = unsafe { create_xplm_window() };

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
        let cfg = PluginConfig {
            server: self.server.clone(),
            flight_id: self.flight_id.clone(),
            user_name: self.user_name.clone(),
            gain: self.gain,
            output_device,
            log_level: self.log_level.to_string().to_lowercase(),
        };
        cfg.save(&self.config_path);
    }

    pub fn output_device(&self) -> Option<String> {
        self.output_devices
            .get(self.selected_device as usize)
            .cloned()
    }

    fn make_gl() -> glow::Context {
        unsafe {
            glow::Context::from_loader_function(|s| {
                // X-Plane has already loaded libGL; RTLD_DEFAULT (null) finds it.
                let cstr = CString::new(s).unwrap_or_default();
                libc::dlsym(std::ptr::null_mut(), cstr.as_ptr()) as *const c_void
            })
        }
    }

    fn init_renderer(&mut self) {
        let mut ctx = imgui::Context::create();
        ctx.set_ini_filename(None);
        ctx.style_mut().use_dark_colors();
        ctx.fonts().add_font(&[imgui::FontSource::DefaultFontData {
            config: Some(imgui::FontConfig {
                size_pixels: 15.0,
                ..Default::default()
            }),
        }]);

        match imgui_glow_renderer::AutoRenderer::initialize(Self::make_gl(), &mut ctx) {
            Ok(renderer) => {
                self.imgui_ctx = Some(ctx);
                self.imgui_renderer = Some(renderer);
            }
            Err(e) => warn!("renderer init failed: {e}"),
        }
    }

    // ── Draw callback ─────────────────────────────────────────────────────────

    pub fn draw(&mut self, win: XPLMWindowID) {
        if self.imgui_ctx.is_none() {
            self.init_renderer();
        }

        let (mut left, mut top, mut right, mut bottom) = (0i32, 0i32, 0i32, 0i32);
        unsafe { XPLMGetWindowGeometry(win, &mut left, &mut top, &mut right, &mut bottom) };
        let width = (right - left).max(1);
        let height = (top - bottom).max(1);

        // Virtual desktop bounds — same coordinate space as XPLMGetWindowGeometry.
        let (mut virt_l, mut virt_t, mut virt_r, mut virt_b) = (0i32, 0i32, 0i32, 0i32);
        unsafe { XPLMGetScreenBoundsGlobal(&mut virt_l, &mut virt_t, &mut virt_r, &mut virt_b) };
        let virt_w = (virt_r - virt_l).max(1);
        let virt_h = (virt_t - virt_b).max(1); // Y-up: top > bottom

        // Physical framebuffer size — may differ from virtual on HiDPI / UI-scale setups.
        let (mut phys_w, mut phys_h) = (0i32, 0i32);
        unsafe { XPLMGetScreenSize(&mut phys_w, &mut phys_h) };
        let scale_x = phys_w as f32 / virt_w as f32;
        let scale_y = phys_h as f32 / virt_h as f32;

        // Cache virtual height for Y-flip in mouse coordinate conversion.
        self.screen_h = virt_h;

        // XPLM Y-up → ImGui Y-down, all in virtual pixel units.
        let win_imgui_x = (left - virt_l) as f32;
        let win_imgui_y = (virt_h - (top - virt_b)) as f32;

        if !self.logged_coords {
            self.logged_coords = true;
            debug!("virt={virt_w}x{virt_h} phys={phys_w}x{phys_h} \
                 scale={scale_x:.2}x{scale_y:.2} win=({left},{bottom})-({right},{top}) \
                 imgui_pos=({win_imgui_x},{win_imgui_y})");
        }

        let dt = {
            let now = Instant::now();
            let d = (now - self.last_time).as_secs_f32().max(1e-6);
            self.last_time = now;
            d
        };

        // Snapshot mutable config into locals — avoids a borrow conflict between
        // the imgui Context borrow (through Ui) and the config field borrows.
        let mut server = self.server.clone();
        let mut flight_id = self.flight_id.clone();
        let mut user_name = self.user_name.clone();
        let mut gain = self.gain;
        let mut selected_device = self.selected_device;
        let mut log_level = self.log_level;
        let mut should_connect = false;
        let mut should_disconnect = false;
        let is_connected = self.is_connected;
        let status = self.status.clone();
        let output_devices = self.output_devices.clone();
        let output_device_labels = self.output_device_labels.clone();
        let mouse_pos = self.mouse_pos;
        let mouse_down = self.mouse_down;

        let (Some(ctx), Some(renderer)) = (self.imgui_ctx.as_mut(), self.imgui_renderer.as_mut())
        else {
            return;
        };

        {
            let io = ctx.io_mut();
            // Virtual-pixel canvas; framebuffer_scale tells the renderer the physical size.
            io.display_size = [virt_w as f32, virt_h as f32];
            io.display_framebuffer_scale = [scale_x, scale_y];
            io.delta_time = dt;
            io.mouse_pos = mouse_pos;
            io.mouse_down = mouse_down;
        }

        {
            let ui = ctx.frame();
            let fw = (width as f32 - 115.0).max(80.0);

            ui.window("##main")
                .position([win_imgui_x, win_imgui_y], imgui::Condition::Always)
                .size([width as f32, height as f32], imgui::Condition::Always)
                .title_bar(false)
                .resizable(false)
                .movable(false)
                .scroll_bar(false)
                .build(|| {
                    let row = |label: &str, id: &str, buf: &mut String| {
                        ui.text(label);
                        ui.same_line();
                        ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                        ui.set_next_item_width(fw);
                        ui.input_text(id, buf).build();
                    };

                    row("Server", "##srv", &mut server);
                    row("Flight ID", "##fid", &mut flight_id);
                    row("Username", "##usr", &mut user_name);

                    ui.text("Gain");
                    ui.same_line();
                    ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                    ui.set_next_item_width(fw);
                    ui.slider_config("##gain", 0.1_f32, 4.0_f32)
                        .build(&mut gain);

                    if !output_devices.is_empty() {
                        let preview = output_device_labels
                            .get(selected_device as usize)
                            .map(|s| s.as_str())
                            .unwrap_or("(default)");
                        ui.text("Audio Playback");
                        ui.same_line();
                        ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                        ui.set_next_item_width(fw);
                        let _dis = ui.begin_disabled(is_connected);
                        if let Some(_tok) = ui.begin_combo("##dev", preview) {
                            let avail_w = ui.content_region_avail()[0];
                            for (i, label) in output_device_labels.iter().enumerate() {
                                let display = fit_label(ui, label, avail_w);
                                if ui
                                    .selectable_config(&*display)
                                    .selected(selected_device == i as i32)
                                    .build()
                                {
                                    selected_device = i as i32;
                                }
                            }
                        }
                        drop(_dis);
                    }

                    const LOG_LEVELS: &[LevelFilter] = &[
                        LevelFilter::Error,
                        LevelFilter::Warn,
                        LevelFilter::Info,
                        LevelFilter::Debug,
                    ];
                    let level_preview = format!("{log_level}");
                    ui.text("Log Level");
                    ui.same_line();
                    ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                    ui.set_next_item_width(fw);
                    if let Some(_tok) = ui.begin_combo("##loglevel", &level_preview) {
                        for &lvl in LOG_LEVELS {
                            if ui.selectable_config(format!("{lvl}")).selected(log_level == lvl).build() {
                                log_level = lvl;
                            }
                        }
                    }

                    ui.spacing();
                    ui.separator();
                    ui.spacing();

                    if is_connected {
                        if ui.button("Disconnect") {
                            should_disconnect = true;
                        }
                        ui.same_line();
                        ui.text_colored([0.3, 1.0, 0.3, 1.0], "Connected");
                    } else {
                        if ui.button("Connect") {
                            debug!("Connect pressed — flight_id='{}' user='{}'",
                                flight_id.trim(), user_name.trim());
                            if !flight_id.trim().is_empty() && !user_name.trim().is_empty() {
                                should_connect = true;
                            } else {
                                warn!("Connect blocked — flight_id or username is empty");
                            }
                        }
                        ui.same_line();
                        ui.text_colored([0.8, 0.3, 0.3, 1.0], "Disconnected");
                    }

                    if !status.is_empty() {
                        ui.spacing();
                        ui.text_disabled(&status);
                    }
                });
        } // ui borrow ends here
        let draw_data = ctx.render();

        // Sync X-Plane's GL state cache before the renderer touches raw GL.
        unsafe { XPLMSetGraphicsState(0, 1, 0, 0, 1, 0, 0) };
        renderer.render(draw_data).ok();

        // Write back modified config locals.
        self.server = server;
        self.flight_id = flight_id;
        self.user_name = user_name;
        self.gain = gain;
        self.selected_device = selected_device;
        if log_level != self.log_level {
            self.log_level = log_level;
            log::set_max_level(log_level);
            self.save_config();
        }
        if should_connect {
            self.should_connect = true;
        }
        if should_disconnect {
            self.should_disconnect = true;
        }
    }

    // ── Input ─────────────────────────────────────────────────────────────────

    pub fn on_mouse(&mut self, win: XPLMWindowID, x: c_int, y: c_int, status: XPLMMouseStatus) {
        // XPLM gives global coords with Y-up; ImGui expects Y-down to match display_size.
        self.mouse_pos = [x as f32, (self.screen_h - y) as f32];
        if status == XPLMMouseStatus::Down {
            self.mouse_down[0] = true;
            unsafe {
                XPLMTakeKeyboardFocus(win);
            }
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
        if axis == 0 {
            io.mouse_wheel += clicks as f32;
        } else {
            io.mouse_wheel_h += clicks as f32;
        }
    }

    pub fn on_char(&mut self, key: u8) {
        let Some(ctx) = &mut self.imgui_ctx else {
            return;
        };
        let io = ctx.io_mut();
        match key {
            8 => {
                io.add_key_event(imgui::Key::Backspace, true);
                io.add_key_event(imgui::Key::Backspace, false);
            }
            32.. => io.add_input_character(key as char),
            _ => {}
        }
    }
}

// ── XPLM window creation ──────────────────────────────────────────────────────

unsafe fn create_xplm_window() -> XPLMWindowID {
    unsafe extern "C-unwind" fn draw_cb(win: XPLMWindowID, _: *mut c_void) {
        let Ok(mut g) = crate::plugin_cell().lock() else {
            return;
        };
        if let Some(ps) = g.as_mut() {
            ps.gui.draw(win);
        }
    }
    unsafe extern "C-unwind" fn mouse_cb(
        win: XPLMWindowID,
        x: c_int,
        y: c_int,
        s: XPLMMouseStatus,
        _: *mut c_void,
    ) -> c_int {
        let Ok(mut g) = crate::plugin_cell().lock() else {
            return 1;
        };
        if let Some(ps) = g.as_mut() {
            ps.gui.on_mouse(win, x, y, s);
        }
        1
    }
    unsafe extern "C-unwind" fn cursor_cb(
        _win: XPLMWindowID,
        x: c_int,
        y: c_int,
        _: *mut c_void,
    ) -> xplane_sys::XPLMCursorStatus {
        let Ok(mut g) = crate::plugin_cell().lock() else {
            return xplane_sys::XPLMCursorStatus::Default;
        };
        if let Some(ps) = g.as_mut() {
            ps.gui.on_mouse_move(x, y);
        }
        xplane_sys::XPLMCursorStatus::Default
    }
    unsafe extern "C-unwind" fn wheel_cb(
        _win: XPLMWindowID,
        x: c_int,
        y: c_int,
        wheel: c_int,
        clicks: c_int,
        _: *mut c_void,
    ) -> c_int {
        let Ok(mut g) = crate::plugin_cell().lock() else { return 1 };
        if let Some(ps) = g.as_mut() {
            ps.gui.on_wheel(x, y, wheel, clicks);
        }
        1
    }
    unsafe extern "C-unwind" fn key_cb(
        _: XPLMWindowID,
        key: c_char,
        flags: xplane_sys::XPLMKeyFlags,
        _vk: c_char,
        _: *mut c_void,
        losing: c_int,
    ) {
        // ignore up-events and focus-loss notifications
        if losing != 0 || (flags & xplane_sys::XPLMKeyFlags::Down).0 == 0 {
            return;
        }
        if key > 0 {
            let Ok(mut g) = crate::plugin_cell().lock() else {
                return;
            };
            if let Some(ps) = g.as_mut() {
                ps.gui.on_char(key as u8);
            }
        }
    }

    let mut params = xplane_sys::XPLMCreateWindow_t {
        structSize: std::mem::size_of::<xplane_sys::XPLMCreateWindow_t>() as c_int,
        left: 60,
        top: 460,
        right: 480,
        bottom: 60,
        visible: 0,
        drawWindowFunc: Some(draw_cb),
        handleMouseClickFunc: Some(mouse_cb),
        handleKeyFunc: Some(key_cb),
        handleCursorFunc: Some(cursor_cb),
        handleMouseWheelFunc: Some(wheel_cb),
        refcon: std::ptr::null_mut(),
        decorateAsFloatingWindow: XPLMWindowDecoration::RoundRectangle,
        layer: XPLMWindowLayer::FloatingWindows,
        handleRightClickFunc: None,
    };

    let win = XPLMCreateWindowEx(&mut params);
    XPLMSetWindowTitle(win, b"MumbledFlight\0".as_ptr() as *const c_char);
    XPLMSetWindowPositioningMode(win, XPLMWindowPositioningMode::PositionFree, -1);
    win
}
