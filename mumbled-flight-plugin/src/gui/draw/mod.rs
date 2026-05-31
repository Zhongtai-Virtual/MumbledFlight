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

//! ImGui draw loop and renderer initialisation for `GuiState`.
//!
//! The per-frame drawing is split across submodules:
//! - [`widgets`] — the `Ctx` panel-renderer and its reusable primitive widgets.
//! - [`panels`] — the individual config panels rendered onto a `Ctx`.
//! - [`file_picker`] — the file-browser window (separate XPLM window); owns `draw_file_picker`.
//! - [`tofu`] — the TOFU probe/decide flow; owns `draw_tofu` and the probe/decide logic.
//!
//! Each floating window has its own [`ImguiWindowState`] (ctx + renderer + input
//! state) and its own draw method. `GuiState::draw` (here) renders the main config window;
//! `draw_file_picker` and `draw_tofu` live in their respective submodules.
//!
//! `init_imgui` and `make_gl` are `pub(super)` helpers shared by all three draw methods.

mod file_picker;
mod panels;
mod tofu;
mod widgets;

use log::{debug, warn};
use std::ffi::CString;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use xplane_sys::{
    XPLMGetScreenBoundsGlobal, XPLMGetScreenSize, XPLMGetWindowGeometry, XPLMSetGraphicsState,
    XPLMSetWindowGeometry, XPLMSetWindowIsVisible, XPLMWindowID,
};

use super::known_hosts::KnownHosts;
use super::trust::TrustState;
use super::{FilePickTarget, FilePicker, GuiState, ImguiWindowState};
use file_picker::start_dir;
use panels::BrowseClicks;
use widgets::{Ctx, LABEL_COL_X};

// ── Shared renderer helpers ───────────────────────────────────────────────────

pub(super) fn init_imgui(state: &mut ImguiWindowState) {
    let mut ctx = imgui::Context::create();
    ctx.set_ini_filename(None);
    ctx.style_mut().use_dark_colors();
    ctx.fonts().add_font(&[imgui::FontSource::DefaultFontData {
        config: Some(imgui::FontConfig {
            size_pixels: 15.0,
            ..Default::default()
        }),
    }]);
    match imgui_glow_renderer::AutoRenderer::initialize(make_gl(), &mut ctx) {
        Ok(renderer) => {
            state.ctx = Some(ctx);
            state.renderer = Some(renderer);
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
                libc::dlsym(std::ptr::null_mut(), cstr.as_ptr()) as *const c_void
            }
            #[cfg(target_os = "windows")]
            {
                extern "system" {
                    fn wglGetProcAddress(name: *const i8) -> *const c_void;
                    fn LoadLibraryA(name: *const i8) -> *mut c_void;
                    fn GetProcAddress(module: *mut c_void, name: *const i8) -> *const c_void;
                }
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

/// Reads the XPLM window geometry and virtual/physical screen metrics, returning
/// `(width, height, virt_w, virt_h, scale_x, scale_y, win_imgui_x, win_imgui_y)`.
pub(super) fn window_metrics(win: XPLMWindowID) -> (i32, i32, i32, i32, f32, f32, f32, f32) {
    let (mut left, mut top, mut right, mut bottom) = (0i32, 0, 0, 0);
    unsafe { XPLMGetWindowGeometry(win, &mut left, &mut top, &mut right, &mut bottom) };
    let width = (right - left).max(1);
    let height = (top - bottom).max(1);

    let (mut virt_l, mut virt_t, mut virt_r, mut virt_b) = (0i32, 0, 0, 0);
    unsafe { XPLMGetScreenBoundsGlobal(&mut virt_l, &mut virt_t, &mut virt_r, &mut virt_b) };
    let virt_w = (virt_r - virt_l).max(1);
    let virt_h = (virt_t - virt_b).max(1);

    let (mut phys_w, mut phys_h) = (0i32, 0);
    unsafe { XPLMGetScreenSize(&mut phys_w, &mut phys_h) };
    let scale_x = phys_w as f32 / virt_w as f32;
    let scale_y = phys_h as f32 / virt_h as f32;

    let win_imgui_x = (left - virt_l) as f32;
    let win_imgui_y = (virt_h - (top - virt_b)) as f32;

    (
        width,
        height,
        virt_w,
        virt_h,
        scale_x,
        scale_y,
        win_imgui_x,
        win_imgui_y,
    )
}

/// Positions `popup` centred over `main` then makes it visible.
unsafe fn show_centred(popup: XPLMWindowID, main: XPLMWindowID, w: i32, h: i32) {
    let (mut ml, mut mt, mut mr, mut mb) = (0i32, 0, 0, 0);
    XPLMGetWindowGeometry(main, &mut ml, &mut mt, &mut mr, &mut mb);
    let cx = (ml + mr) / 2;
    let cy = (mb + mt) / 2;
    XPLMSetWindowGeometry(popup, cx - w / 2, cy + h / 2, cx + w / 2, cy - h / 2);
    XPLMSetWindowIsVisible(popup, 1);
}

// ── Draw implementations ──────────────────────────────────────────────────────

impl GuiState {
    /// Dispatch entry point called by the shared draw callback for all three windows.
    pub fn draw_any(&mut self, win: XPLMWindowID) {
        let fp = self.file_picker_win;
        let tofu = self.tofu_win;
        if win == fp {
            self.draw_file_picker(win);
        } else if win == tofu {
            self.draw_tofu(win);
        } else {
            self.draw(win);
        }
    }

    pub fn draw(&mut self, win: XPLMWindowID) {
        if self.main_imgui.ctx.is_none() {
            init_imgui(&mut self.main_imgui);
        }

        let (width, height, virt_w, virt_h, scale_x, scale_y, win_imgui_x, win_imgui_y) =
            window_metrics(win);
        self.screen_h = virt_h;

        if !self.main_imgui.logged_coords {
            self.main_imgui.logged_coords = true;
            debug!(
                "virt={virt_w}x{virt_h} phys={:.0}x{:.0} scale={scale_x:.2}x{scale_y:.2} \
                 imgui_pos=({win_imgui_x},{win_imgui_y})",
                virt_w as f32 * scale_x,
                virt_h as f32 * scale_y,
            );
        }

        let dt = {
            let now = Instant::now();
            let d = (now - self.main_imgui.last_time).as_secs_f32().max(1e-6);
            self.main_imgui.last_time = now;
            d
        };

        let plugin_dir = self
            .config_path
            .parent()
            .filter(|p| p.is_dir())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }));

        // Snapshot mutable fields to avoid borrow conflict with imgui::Ui.
        let mut file_picker = self.file_picker.take();
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
        let mut trust_state = std::mem::replace(&mut self.trust_state, TrustState::Idle);
        let probe_slot = Arc::clone(&self.probe_slot);
        // Snapshot known_hosts so the closure can look up the current server's stored cert using
        // the post-edit server value (text widgets in the closure may update `server`/`port`
        // before start_probe is called).
        let known_hosts_snap = self.known_hosts.snapshot();
        let is_connected = self.is_connected;
        let status = self.status.clone();
        let voip_statuses = self.voip_statuses.clone();
        let output_device_labels = self.output_device_labels.clone();
        let mic_input_device_labels = self.mic_input_device_labels.clone();
        let mut selected_mic = self.selected_mic;
        let radio_input_device_labels = self.radio_input_device_labels.clone();
        let mut selected_radio = self.selected_radio;
        let mouse_pos = self.main_imgui.mouse_pos;
        let mouse_down = self.main_imgui.mouse_down;
        let file_picker_win = self.file_picker_win;
        let tofu_win = self.tofu_win;
        let window_id = self.window_id;

        let (Some(ctx), Some(renderer)) = (
            self.main_imgui.ctx.as_mut(),
            self.main_imgui.renderer.as_mut(),
        ) else {
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
            let pad_r = ui.clone_style().window_padding[0];
            let fw = (width as f32 - LABEL_COL_X - pad_r).max(80.0);
            let p = Ctx { ui: &*ui, fw };

            ui.window("##main")
                .position([win_imgui_x, win_imgui_y], imgui::Condition::Always)
                .size([width as f32, height as f32], imgui::Condition::Always)
                .title_bar(false)
                .resizable(false)
                .movable(false)
                .scroll_bar(false)
                .build(|| {
                    let BrowseClicks {
                        cert: cert_browse,
                        ca: ca_browse,
                    } = p.connection_fields(
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
                    let tofu_active = matches!(
                        trust_state,
                        TrustState::Probing { .. } | TrustState::Decide(_)
                    );
                    let picker_active = file_picker.is_some();
                    let (conn, disc) = p.connect_button(
                        is_connected || tofu_active || picker_active,
                        &flight_id,
                        &user_name,
                    );
                    should_disconnect = disc;
                    if disc {
                        trust_state = TrustState::Idle;
                    }
                    if conn {
                        if !server_ca.trim().is_empty() {
                            should_connect = true;
                        } else {
                            let probe_key = KnownHosts::key(&server, port);
                            let probe_stored = known_hosts_snap.get(&probe_key).cloned();
                            tofu::start_probe(
                                &mut trust_state,
                                &probe_slot,
                                probe_key,
                                probe_stored,
                                &server,
                                port,
                            );
                        }
                    }
                    p.status_display(voip_statuses.as_ref(), &status);

                    // Open the file picker window when a Browse button is clicked.
                    {
                        let requests: [(bool, FilePickTarget, &str, &'static [&'static str]); 2] = [
                            (
                                cert_browse,
                                FilePickTarget::UserCert,
                                &cert_path,
                                &["p12", "pfx"],
                            ),
                            (
                                ca_browse,
                                FilePickTarget::ServerCa,
                                &server_ca,
                                &["pem", "der", "crt"],
                            ),
                        ];
                        if file_picker.is_none() {
                            if let Some((_, target, current, exts)) =
                                requests.iter().find(|(b, ..)| *b)
                            {
                                file_picker = Some(FilePicker::new(
                                    start_dir(current, &plugin_dir),
                                    *target,
                                    exts,
                                ));
                                unsafe { show_centred(file_picker_win, window_id, 430, 330) };
                            }
                        }
                    }

                    // Poll the TOFU probe; show the TOFU window if a decision is needed.
                    let outcome = tofu::poll_step(&mut trust_state, &probe_slot);
                    if outcome.silent_connect {
                        should_connect = true;
                    }
                    if outcome.show_tofu {
                        unsafe { show_centred(tofu_win, window_id, 430, 280) };
                    }
                });
        }
        let draw_data = ctx.render();
        unsafe { XPLMSetGraphicsState(0, 1, 0, 0, 1, 0, 0) };
        renderer.render(draw_data).ok();

        // Write back.
        self.file_picker = file_picker;
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
        self.trust_state = trust_state;
        if should_connect {
            self.should_connect = true;
        }
        if should_disconnect {
            self.should_disconnect = true;
        }
    }
}
