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

//! ImGui draw loop and renderer initialisation for GuiState.

use log::{debug, warn, LevelFilter};
use std::borrow::Cow;
use std::ffi::CString;
use std::os::raw::c_void;
use std::sync::atomic::Ordering;
use std::time::Instant;

use mumbled_flight_core::mumble::{ClientStatus, VoipStatuses};
use xplane_sys::{
    XPLMGetScreenBoundsGlobal, XPLMGetScreenSize, XPLMGetWindowGeometry, XPLMSetGraphicsState,
    XPLMWindowID,
};

use super::GuiState;

impl GuiState {
    pub fn draw(&mut self, win: XPLMWindowID) {
        if self.imgui_ctx.is_none() {
            self.init_renderer();
        }

        let (mut left, mut top, mut right, mut bottom) = (0i32, 0i32, 0i32, 0i32);
        unsafe { XPLMGetWindowGeometry(win, &mut left, &mut top, &mut right, &mut bottom) };
        let width = (right - left).max(1);
        let height = (top - bottom).max(1);

        let (mut virt_l, mut virt_t, mut virt_r, mut virt_b) = (0i32, 0i32, 0i32, 0i32);
        unsafe { XPLMGetScreenBoundsGlobal(&mut virt_l, &mut virt_t, &mut virt_r, &mut virt_b) };
        let virt_w = (virt_r - virt_l).max(1);
        let virt_h = (virt_t - virt_b).max(1); // Y-up: top > bottom

        let (mut phys_w, mut phys_h) = (0i32, 0i32);
        unsafe { XPLMGetScreenSize(&mut phys_w, &mut phys_h) };
        let scale_x = phys_w as f32 / virt_w as f32;
        let scale_y = phys_h as f32 / virt_h as f32;

        self.screen_h = virt_h;

        let win_imgui_x = (left - virt_l) as f32;
        let win_imgui_y = (virt_h - (top - virt_b)) as f32;

        if !self.logged_coords {
            self.logged_coords = true;
            debug!(
                "virt={virt_w}x{virt_h} phys={phys_w}x{phys_h} \
                 scale={scale_x:.2}x{scale_y:.2} win=({left},{bottom})-({right},{top}) \
                 imgui_pos=({win_imgui_x},{win_imgui_y})"
            );
        }

        let dt = {
            let now = Instant::now();
            let d = (now - self.last_time).as_secs_f32().max(1e-6);
            self.last_time = now;
            d
        };

        // Snapshot mutable fields — avoids borrow conflict between imgui::Ui and self.
        let mut server = self.server.clone();
        let mut port = self.port;
        let mut server_password = self.server_password.clone();
        let mut cert_path = self.cert_path.clone();
        let mut cert_pass = self.cert_pass.clone();
        let mut server_ca = self.server_ca.clone();
        let mut flight_id = self.flight_id.clone();
        let mut user_name = self.user_name.clone();
        let mut gain = self.gain;
        let mut ambient_vol = self.ambient_vol;
        let mut ic_vol = self.ic_vol;
        let mut spatial_width = self.spatial_width;
        let mut denoise = self.denoise;
        let mut selected_ambient = self.selected_ambient;
        let mut selected_ic = self.selected_ic;
        let mut log_level = self.log_level;
        let mut should_connect = false;
        let mut should_disconnect = false;
        let is_connected = self.is_connected;
        let status = self.status.clone();
        let voip_statuses = self.voip_statuses.clone();
        let output_device_labels = self.output_device_labels.clone();
        let mic_input_device_labels = self.mic_input_device_labels.clone();
        let mut selected_mic = self.selected_mic;
        let radio_input_device_labels = self.radio_input_device_labels.clone();
        let mut selected_radio = self.selected_radio;
        let mouse_pos = self.mouse_pos;
        let mouse_down = self.mouse_down;

        let (Some(ctx), Some(renderer)) = (self.imgui_ctx.as_mut(), self.imgui_renderer.as_mut())
        else {
            return;
        };

        {
            let io = ctx.io_mut();
            io.display_size = [virt_w as f32, virt_h as f32];
            io.display_framebuffer_scale = [scale_x, scale_y];
            io.delta_time = dt;
            io.mouse_pos = mouse_pos;
            io.mouse_down = mouse_down;
        }

        {
            let ui = ctx.frame();
            // Right edge of the widget column = content-region edge. Subtracting the window
            // padding keeps the right margin symmetric with the left and stops the trailing
            // reset icons from being jammed against the window's outer edge.
            let pad_r = ui.clone_style().window_padding[0];
            let fw = (width as f32 - 115.0 - pad_r).max(80.0);
            let p = Ctx { ui: &*ui, fw };

            ui.window("##main")
                .position([win_imgui_x, win_imgui_y], imgui::Condition::Always)
                .size([width as f32, height as f32], imgui::Condition::Always)
                .title_bar(false)
                .resizable(false)
                .movable(false)
                .scroll_bar(false)
                .build(|| {
                    p.connection_fields(
                        &mut server,
                        &mut port,
                        &mut server_password,
                        &mut cert_path,
                        &mut cert_pass,
                        &mut server_ca,
                        &mut flight_id,
                        &mut user_name,
                    );
                    p.audio_controls(&mut ambient_vol, &mut ic_vol, &mut gain, &mut spatial_width);
                    p.denoise_toggle(&mut denoise, is_connected);
                    if !output_device_labels.is_empty() {
                        p.output_device_pickers(
                            is_connected,
                            &output_device_labels,
                            &mut selected_ambient,
                            &mut selected_ic,
                        );
                    }
                    p.mic_picker(is_connected, &mic_input_device_labels, &mut selected_mic);
                    p.radio_picker(
                        is_connected,
                        &radio_input_device_labels,
                        &mut selected_radio,
                    );
                    p.log_level_picker(&mut log_level);
                    let (conn, disc) = p.connect_button(is_connected, &flight_id, &user_name);
                    should_connect = conn;
                    should_disconnect = disc;
                    p.status_display(voip_statuses.as_ref(), &status);
                });
        } // ui borrow ends here
        let draw_data = ctx.render();

        // Sync X-Plane's GL state cache before the renderer touches raw GL.
        unsafe { XPLMSetGraphicsState(0, 1, 0, 0, 1, 0, 0) };
        renderer.render(draw_data).ok();

        // Write back modified config locals.
        self.server = server;
        self.port = port;
        self.server_password = server_password;
        self.cert_path = cert_path;
        self.cert_pass = cert_pass;
        self.server_ca = server_ca;
        self.flight_id = flight_id;
        self.user_name = user_name;
        self.gain = gain;
        if let Some(ref atomic) = self.mic_gain_live {
            atomic.store(gain.to_bits(), Ordering::Relaxed);
        }
        self.ambient_vol = ambient_vol;
        if let Some(ref atomic) = self.ambient_vol_live {
            atomic.store(ambient_vol.to_bits(), Ordering::Relaxed);
        }
        self.ic_vol = ic_vol;
        if let Some(ref atomic) = self.ic_vol_live {
            atomic.store(ic_vol.to_bits(), Ordering::Relaxed);
        }
        self.spatial_width = spatial_width;
        if let Some(ref atomic) = self.spatial_width_live {
            atomic.store(spatial_width.to_bits(), Ordering::Relaxed);
        }
        self.selected_ambient = selected_ambient;
        self.selected_ic = selected_ic;
        self.selected_mic = selected_mic;
        self.selected_radio = selected_radio;
        self.denoise = denoise;
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

    fn make_gl() -> glow::Context {
        unsafe {
            glow::Context::from_loader_function(|s| {
                let cstr = CString::new(s).expect("GL symbol names contain no interior nul bytes");
                #[cfg(not(target_os = "windows"))]
                {
                    // RTLD_DEFAULT (null) searches already-loaded libs — libGL is loaded by X-Plane.
                    libc::dlsym(std::ptr::null_mut(), cstr.as_ptr()) as *const c_void
                }
                #[cfg(target_os = "windows")]
                {
                    extern "system" {
                        fn wglGetProcAddress(name: *const i8) -> *const c_void;
                        fn LoadLibraryA(name: *const i8) -> *mut c_void;
                        fn GetProcAddress(module: *mut c_void, name: *const i8) -> *const c_void;
                    }
                    // wglGetProcAddress covers OpenGL extensions; GetProcAddress covers core 1.1.
                    let p = wglGetProcAddress(cstr.as_ptr());
                    if !p.is_null() {
                        p
                    } else {
                        let lib = LoadLibraryA(b"opengl32.dll\0".as_ptr() as *const i8);
                        GetProcAddress(lib, cstr.as_ptr())
                    }
                }
            })
        }
    }
}

// ── Panel renderer ────────────────────────────────────────────────────────────
//
// Groups shared draw-time state (`ui`, `fw`) so panel methods don't repeat
// those two parameters on every call.

struct Ctx<'ui> {
    ui: &'ui imgui::Ui,
    fw: f32,
}

impl<'ui> Ctx<'ui> {
    // ── Primitive helpers ─────────────────────────────────────────────────────

    fn row(&self, label: &str, id: &str, buf: &mut String) {
        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([115.0, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw);
        self.ui.input_text(id, buf).build();
    }

    /// Same layout as `row`, but the input is masked (for the server password).
    fn password_row(&self, label: &str, id: &str, buf: &mut String) {
        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([115.0, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw);
        self.ui.input_text(id, buf).password(true).build();
    }

    // A labelled slider plus a reset icon; the parameters map 1:1 to imgui's slider config.
    #[allow(clippy::too_many_arguments)]
    fn slider(
        &self,
        label: &str,
        id: &str,
        v: &mut f32,
        min: f32,
        max: f32,
        flags: imgui::SliderFlags,
        default: f32,
    ) {
        let icon_sz = self.ui.current_font_size();
        let spacing = self.ui.clone_style().item_spacing[0];

        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([115.0, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw - icon_sz - spacing);
        self.ui
            .slider_config(id, min, max)
            .flags(flags)
            .display_format("")
            .build(v);
        self.ui.same_line();
        // id is "##foo"; "_r" suffix gives the button a distinct imgui ID ("foo_r" vs "foo").
        if self.reset_icon_button(&format!("{id}_r")) {
            *v = default;
        }
    }

    /// Draws a circular-arrow icon button and returns `true` when clicked.
    /// Occupies `current_font_size` × `frame_height` so the icon sits centred
    /// against any adjacent widget with standard frame padding.
    fn reset_icon_button(&self, id: &str) -> bool {
        let icon_sz = self.ui.current_font_size();
        let frame_h = self.ui.frame_height();

        let clicked = self.ui.invisible_button(id, [icon_sz, frame_h]);
        let hovered = self.ui.is_item_hovered();
        let rect_min = self.ui.item_rect_min();
        let rect_max = self.ui.item_rect_max();

        // Centre derived from the actual placed rect — immune to spacing offsets.
        let cx = (rect_min[0] + rect_max[0]) * 0.5;
        let cy = (rect_min[1] + rect_max[1]) * 0.5;
        let r = icon_sz * 0.28;

        let col: [f32; 4] = if hovered {
            [1.0, 1.0, 1.0, 1.0]
        } else {
            [0.55, 0.55, 0.55, 1.0]
        };

        // Arc: ~306° clockwise (increasing θ = clockwise in screen/y-down coords).
        // Starts at ~36° (lower-right), ends at ~342° (upper-right); gap on the right side.
        use std::f32::consts::PI;
        let start_a = PI * 0.2;
        let sweep = PI * 1.7;
        let end_a = start_a + sweep;
        const N: usize = 14;
        let arc: Vec<[f32; 2]> = (0..=N)
            .map(|i| {
                let a = start_a + sweep * i as f32 / N as f32;
                [cx + r * a.cos(), cy + r * a.sin()]
            })
            .collect();
        let draw = self.ui.get_window_draw_list();
        draw.add_polyline(arc, col).thickness(1.5).build();

        // Filled arrowhead at arc end pointing in the clockwise tangent direction.
        // Clockwise tangent at θ in screen (y-down) coords: (−sin θ, cos θ).
        let tip = [cx + r * end_a.cos(), cy + r * end_a.sin()];
        let (tx, ty) = (-end_a.sin(), end_a.cos()); // clockwise tangent
        let (nx, ny) = (end_a.cos(), end_a.sin()); // outward radial normal
        let al = r * 0.55;
        let aw = r * 0.40;
        let p2 = [tip[0] - al * tx + aw * nx, tip[1] - al * ty + aw * ny];
        let p3 = [tip[0] - al * tx - aw * nx, tip[1] - al * ty - aw * ny];
        draw.add_triangle(tip, p2, p3, col).filled(true).build();

        if hovered {
            self.ui.tooltip_text("Reset");
        }
        clicked
    }

    fn combo(&self, label: &str, id: &str, labels: &[String], selected: &mut i32) {
        let preview = labels
            .get(*selected as usize)
            .map(|s| s.as_str())
            .unwrap_or("(default)");
        self.ui.text(label);
        self.ui.same_line();
        self.ui.set_cursor_pos([115.0, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw);
        if let Some(_tok) = self.ui.begin_combo(id, preview) {
            let avail_w = self.ui.content_region_avail()[0];
            for (i, lbl) in labels.iter().enumerate() {
                let display = fit_label(self.ui, lbl, avail_w);
                if self
                    .ui
                    .selectable_config(&*display)
                    .selected(*selected == i as i32)
                    .build()
                {
                    *selected = i as i32;
                }
            }
        }
    }

    // ── Panels ────────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn connection_fields(
        &self,
        server: &mut String,
        port: &mut u16,
        server_password: &mut String,
        cert_path: &mut String,
        cert_pass: &mut String,
        server_ca: &mut String,
        flight_id: &mut String,
        user_name: &mut String,
    ) {
        self.row("Server", "##srv", server);
        {
            self.ui.text("Port");
            self.ui.same_line();
            self.ui.set_cursor_pos([115.0, self.ui.cursor_pos()[1]]);
            self.ui.set_next_item_width(self.fw);
            let mut p = *port as i32;
            if self.ui.input_int("##port", &mut p).build() {
                *port = p.clamp(1, 65535) as u16;
            }
        }
        self.row("Flight ID", "##fid", flight_id);
        self.row("Username", "##usr", user_name);

        self.ui.spacing();
        // Default open when any optional field already carries a value so saved
        // credentials don't silently disappear behind a collapsed header.
        let fields: [&str; 4] = [server_password, cert_path, cert_pass, server_ca];
        let any_set = fields.iter().any(|s| !s.is_empty());
        let flags = if any_set {
            imgui::TreeNodeFlags::DEFAULT_OPEN
        } else {
            imgui::TreeNodeFlags::empty()
        };
        if self
            .ui
            .collapsing_header("Optional auth & security##opt", flags)
        {
            self.password_row("Password", "##pwd", server_password);
            self.password_row("Cert Pass", "##certpw", cert_pass);
            self.row("Client Cert", "##cert", cert_path);
            self.row("Server CA", "##sca", server_ca);
        }
        self.ui.spacing();
    }

    fn audio_controls(
        &self,
        ambient_vol: &mut f32,
        ic_vol: &mut f32,
        gain: &mut f32,
        spatial_width: &mut f32,
    ) {
        let vol_flags = imgui::SliderFlags::LOGARITHMIC | imgui::SliderFlags::NO_INPUT;
        self.slider(
            "Ambient Vol",
            "##ambient_vol",
            ambient_vol,
            0.1,
            20.0,
            vol_flags,
            1.0,
        );
        self.slider("IC Vol", "##ic_vol", ic_vol, 0.1, 20.0, vol_flags, 1.0);
        self.slider("Mic Gain", "##gain", gain, 0.1, 20.0, vol_flags, 1.0);
        self.slider(
            "Spatial",
            "##spatial",
            spatial_width,
            0.0,
            2.0,
            imgui::SliderFlags::NO_INPUT,
            1.0,
        );
    }

    fn denoise_toggle(&self, denoise: &mut bool, is_connected: bool) {
        self.ui.text("Denoise");
        self.ui.same_line();
        self.ui.set_cursor_pos([115.0, self.ui.cursor_pos()[1]]);
        let _dis = self.ui.begin_disabled(is_connected);
        self.ui.checkbox("##denoise", denoise);
    }

    fn output_device_pickers(
        &self,
        is_connected: bool,
        output_device_labels: &[String],
        selected_ambient: &mut i32,
        selected_ic: &mut i32,
    ) {
        let _dis = self.ui.begin_disabled(is_connected);
        self.combo(
            "Ambient Out",
            "##dev_ambient",
            output_device_labels,
            selected_ambient,
        );
        self.combo("IC Out", "##dev_ic", output_device_labels, selected_ic);
    }

    fn mic_picker(
        &self,
        is_connected: bool,
        mic_input_device_labels: &[String],
        selected_mic: &mut i32,
    ) {
        if mic_input_device_labels.is_empty() {
            return;
        }
        let mic_labels: Vec<String> = std::iter::once("(system default)".to_string())
            .chain(mic_input_device_labels.iter().cloned())
            .collect();
        let _dis = self.ui.begin_disabled(is_connected);
        self.combo("Mic In", "##mic_in", &mic_labels, selected_mic);
    }

    fn radio_picker(
        &self,
        is_connected: bool,
        radio_input_device_labels: &[String],
        selected_radio: &mut i32,
    ) {
        let radio_labels: Vec<String> = {
            let mut v = vec!["(disabled)".to_string()];
            #[cfg(target_os = "linux")]
            v.push("MumblingRadio (auto-sink)".to_string());
            v.extend(radio_input_device_labels.iter().cloned());
            v
        };
        let _dis = self.ui.begin_disabled(is_connected);
        self.combo("Radio Source", "##radio", &radio_labels, selected_radio);
    }

    fn log_level_picker(&self, log_level: &mut LevelFilter) {
        const LOG_LEVELS: &[LevelFilter] = &[
            LevelFilter::Error,
            LevelFilter::Warn,
            LevelFilter::Info,
            LevelFilter::Debug,
        ];
        let level_preview = format!("{log_level}");
        self.ui.text("Log Level");
        self.ui.same_line();
        self.ui.set_cursor_pos([115.0, self.ui.cursor_pos()[1]]);
        self.ui.set_next_item_width(self.fw);
        if let Some(_tok) = self.ui.begin_combo("##loglevel", &level_preview) {
            for &lvl in LOG_LEVELS {
                if self
                    .ui
                    .selectable_config(format!("{lvl}"))
                    .selected(*log_level == lvl)
                    .build()
                {
                    *log_level = lvl;
                }
            }
        }
    }

    /// Returns `(should_connect, should_disconnect)`.
    fn connect_button(&self, is_connected: bool, flight_id: &str, user_name: &str) -> (bool, bool) {
        self.ui.spacing();
        self.ui.separator();
        self.ui.spacing();
        if is_connected {
            (false, self.ui.button("Disconnect"))
        } else {
            if self.ui.button("Connect") {
                debug!(
                    "Connect pressed — flight_id='{}' user='{}'",
                    flight_id.trim(),
                    user_name.trim()
                );
                if !flight_id.trim().is_empty() && !user_name.trim().is_empty() {
                    return (true, false);
                }
                warn!("Connect blocked — flight_id or username is empty");
            }
            (false, false)
        }
    }

    fn status_display(&self, voip_statuses: Option<&VoipStatuses>, status: &str) {
        self.ui.spacing();
        if let Some(statuses) = voip_statuses {
            let map = statuses.lock().unwrap();
            const KNOWN: &[&str] = &["Voice", "IC", "PA", "Radio"];
            let extras: Vec<&str> = map
                .keys()
                .map(|s| s.as_str())
                .filter(|k| !KNOWN.contains(k))
                .collect();
            for &label in KNOWN.iter().chain(extras.iter()) {
                if let Some(slot) = map.get(label) {
                    let s = slot.lock().unwrap();
                    let (color, tag) = match *s {
                        ClientStatus::Connecting => ([1.0f32, 0.8, 0.2, 1.0], "connecting"),
                        ClientStatus::Connected => ([0.3, 1.0, 0.3, 1.0], "connected"),
                        ClientStatus::Disconnected => ([0.8, 0.3, 0.3, 1.0], "disconnected"),
                    };
                    self.ui.text_disabled(format!("{label}: "));
                    self.ui.same_line();
                    self.ui.text_colored(color, tag);
                }
            }
        } else {
            self.ui.text_colored([0.8, 0.3, 0.3, 1.0], "Disconnected");
        }
        if !status.is_empty() {
            self.ui.spacing();
            self.ui.text_disabled(status);
        }
    }
}

/// Truncate `text` with `...` so it fits within `max_px` using ImGui's font metrics.
fn fit_label<'a>(ui: &imgui::Ui, text: &'a str, max_px: f32) -> Cow<'a, str> {
    if ui.calc_text_size(text)[0] <= max_px {
        return Cow::Borrowed(text);
    }
    let ell_w = ui.calc_text_size("...")[0];
    let avail = (max_px - ell_w).max(0.0);
    let mut end = text.len();
    while end > 0 {
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        if ui.calc_text_size(&text[..end])[0] <= avail {
            break;
        }
        end -= 1;
    }
    Cow::Owned(format!("{}...", &text[..end]))
}
