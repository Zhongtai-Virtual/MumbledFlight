//! ImGui draw loop and renderer initialisation for GuiState.

use std::borrow::Cow;
use std::ffi::CString;
use std::os::raw::c_void;
use std::time::Instant;
use log::{debug, warn, LevelFilter};
use std::sync::atomic::Ordering;

use xplane_sys::{
    XPLMGetScreenBoundsGlobal, XPLMGetScreenSize, XPLMGetWindowGeometry,
    XPLMSetGraphicsState, XPLMWindowID,
};

use super::GuiState;

impl GuiState {
    pub fn draw(&mut self, win: XPLMWindowID) {
        if self.imgui_ctx.is_none() {
            self.init_renderer();
        }

        let (mut left, mut top, mut right, mut bottom) = (0i32, 0i32, 0i32, 0i32);
        unsafe { XPLMGetWindowGeometry(win, &mut left, &mut top, &mut right, &mut bottom) };
        let width  = (right - left).max(1);
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

        // Snapshot mutable fields — avoids borrow conflict between imgui::Ui and self.
        let mut server           = self.server.clone();
        let mut flight_id        = self.flight_id.clone();
        let mut user_name        = self.user_name.clone();
        let mut gain             = self.gain;
        let mut denoise          = self.denoise;
        let mut selected_ambient = self.selected_ambient;
        let mut selected_ic      = self.selected_ic;
        let mut log_level        = self.log_level;
        let mut should_connect   = false;
        let mut should_disconnect = false;
        let is_connected         = self.is_connected;
        let status               = self.status.clone();
        let output_devices        = self.output_devices.clone();
        let output_device_labels  = self.output_device_labels.clone();
        let radio_input_devices   = self.radio_input_devices.clone();
        let mut selected_radio    = self.selected_radio;
        let mouse_pos             = self.mouse_pos;
        let mouse_down            = self.mouse_down;

        let (Some(ctx), Some(renderer)) =
            (self.imgui_ctx.as_mut(), self.imgui_renderer.as_mut())
        else {
            return;
        };

        {
            let io = ctx.io_mut();
            io.display_size              = [virt_w as f32, virt_h as f32];
            io.display_framebuffer_scale = [scale_x, scale_y];
            io.delta_time                = dt;
            io.mouse_pos                 = mouse_pos;
            io.mouse_down                = mouse_down;
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

                    row("Server",    "##srv", &mut server);
                    row("Flight ID", "##fid", &mut flight_id);
                    row("Username",  "##usr", &mut user_name);

                    ui.text("Mic Gain");
                    ui.same_line();
                    ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                    ui.set_next_item_width(fw);
                    ui.slider_config("##gain", 0.1_f32, 20.0_f32)
                        .flags(imgui::SliderFlags::LOGARITHMIC)
                        .build(&mut gain);

                    ui.text("Denoise");
                    ui.same_line();
                    ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                    let _dis = ui.begin_disabled(is_connected);
                    ui.checkbox("##denoise", &mut denoise);
                    drop(_dis);

                    if !output_devices.is_empty() {
                        let _dis = ui.begin_disabled(is_connected);
                        let output_combo = |label: &str, id: &str, selected: &mut i32| {
                            let preview = output_device_labels
                                .get(*selected as usize)
                                .map(|s| s.as_str())
                                .unwrap_or("(default)");
                            ui.text(label);
                            ui.same_line();
                            ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                            ui.set_next_item_width(fw);
                            if let Some(_tok) = ui.begin_combo(id, preview) {
                                let avail_w = ui.content_region_avail()[0];
                                for (i, lbl) in output_device_labels.iter().enumerate() {
                                    let display = fit_label(ui, lbl, avail_w);
                                    if ui.selectable_config(&*display)
                                        .selected(*selected == i as i32)
                                        .build()
                                    {
                                        *selected = i as i32;
                                    }
                                }
                            }
                        };
                        output_combo("Ambient Out", "##dev_ambient", &mut selected_ambient);
                        output_combo("IC Out",      "##dev_ic",      &mut selected_ic);
                        drop(_dis);
                    }

                    {
                        let radio_labels: Vec<String> = {
                            let mut v = vec![
                                "(disabled)".to_string(),
                                "MumblingRadio (auto-sink)".to_string(),
                            ];
                            v.extend(radio_input_devices.iter().cloned());
                            v
                        };
                        let radio_preview = radio_labels
                            .get(selected_radio as usize)
                            .map(|s| s.as_str())
                            .unwrap_or("(disabled)");
                        ui.text("Radio Source");
                        ui.same_line();
                        ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                        ui.set_next_item_width(fw);
                        let _dis = ui.begin_disabled(is_connected);
                        if let Some(_tok) = ui.begin_combo("##radio", radio_preview) {
                            let avail_w = ui.content_region_avail()[0];
                            for (i, label) in radio_labels.iter().enumerate() {
                                let display = fit_label(ui, label, avail_w);
                                if ui.selectable_config(&*display)
                                    .selected(selected_radio == i as i32)
                                    .build()
                                {
                                    selected_radio = i as i32;
                                }
                            }
                        }
                        drop(_dis);
                    }

                    const LOG_LEVELS: &[LevelFilter] = &[
                        LevelFilter::Error, LevelFilter::Warn,
                        LevelFilter::Info,  LevelFilter::Debug,
                    ];
                    let level_preview = format!("{log_level}");
                    ui.text("Log Level");
                    ui.same_line();
                    ui.set_cursor_pos([115.0, ui.cursor_pos()[1]]);
                    ui.set_next_item_width(fw);
                    if let Some(_tok) = ui.begin_combo("##loglevel", &level_preview) {
                        for &lvl in LOG_LEVELS {
                            if ui.selectable_config(format!("{lvl}"))
                                .selected(log_level == lvl)
                                .build()
                            {
                                log_level = lvl;
                            }
                        }
                    }

                    ui.spacing();
                    ui.separator();
                    ui.spacing();

                    if is_connected {
                        if ui.button("Disconnect") { should_disconnect = true; }
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
        self.server          = server;
        self.flight_id       = flight_id;
        self.user_name       = user_name;
        self.gain            = gain;
        if let Some(ref atomic) = self.mic_gain_live {
            atomic.store(gain.to_bits(), Ordering::Relaxed);
        }
        self.selected_ambient = selected_ambient;
        self.selected_ic      = selected_ic;
        self.selected_radio   = selected_radio;
        self.denoise         = denoise;
        if log_level != self.log_level {
            self.log_level = log_level;
            log::set_max_level(log_level);
            self.save_config();
        }
        if should_connect    { self.should_connect    = true; }
        if should_disconnect { self.should_disconnect = true; }
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
                let cstr = CString::new(s).unwrap_or_default();
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

/// Truncate `text` with `...` so it fits within `max_px` using ImGui's font metrics.
fn fit_label<'a>(ui: &imgui::Ui, text: &'a str, max_px: f32) -> Cow<'a, str> {
    if ui.calc_text_size(text)[0] <= max_px {
        return Cow::Borrowed(text);
    }
    let ell_w = ui.calc_text_size("...")[0];
    let avail = (max_px - ell_w).max(0.0);
    let mut end = text.len();
    while end > 0 {
        while end > 0 && !text.is_char_boundary(end) { end -= 1; }
        if ui.calc_text_size(&text[..end])[0] <= avail { break; }
        end -= 1;
    }
    Cow::Owned(format!("{}...", &text[..end]))
}
