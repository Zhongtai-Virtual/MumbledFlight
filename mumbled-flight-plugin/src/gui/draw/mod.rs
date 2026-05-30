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
//! - [`file_picker`] — the modal file-browser popup.
//!
//! This module owns the orchestration: it computes the XPLM↔ImGui coordinate
//! mapping, snapshots `GuiState` into locals (to dodge the borrow conflict
//! between `imgui::Ui` and `self`), drives the panels via `Ctx`, and writes the
//! edited locals back.

mod file_picker;
mod panels;
mod widgets;

use log::{debug, warn};
use mumbled_flight_core::mumble;
use std::ffi::CString;
use std::os::raw::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use xplane_sys::{
    XPLMGetScreenBoundsGlobal, XPLMGetScreenSize, XPLMGetWindowGeometry, XPLMSetGraphicsState,
    XPLMWindowID,
};

use super::known_hosts::KnownHosts;
use super::{FilePickTarget, FilePicker, GuiState, TrustDecide, TrustKind, TrustState};
use file_picker::{start_dir, FilePick};
use panels::{BrowseClicks, TrustChoice, TrustView, TRUST_POPUP_ID};
use widgets::{Ctx, LABEL_COL_X};

/// Monotonic probe generation, so a stale result from a cancelled/superseded probe is ignored.
static PROBE_GEN: AtomicU64 = AtomicU64::new(0);

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

        // Plugin folder derived from the already-resolved config path (XPLMGetSystemPath-based).
        let plugin_dir = self
            .config_path
            .parent()
            .filter(|p| p.is_dir())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" }));

        // Snapshot mutable fields — avoids borrow conflict between imgui::Ui and self.
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
        // TOFU trust flow: take the state out (written back at the end), and snapshot the pinned
        // cert for the current server so the connect handler can run without borrowing `self`.
        let mut trust_state = std::mem::replace(&mut self.trust_state, TrustState::Idle);
        let probe_slot = Arc::clone(&self.probe_slot);
        let known_key = KnownHosts::key(&self.server, self.port);
        // Only clone the pinned PEM when idle — during Probing/Decide it is already held
        // inside trust_state, so cloning here every frame would be wasted.
        let known_stored = matches!(trust_state, TrustState::Idle)
            .then(|| self.known_hosts.get(&known_key).cloned())
            .flatten();
        let mut trust_to_store: Option<(String, String)> = None;
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
            let fw = (width as f32 - LABEL_COL_X - pad_r).max(80.0);
            let p = Ctx {
                ui: &*ui,
                fw,
                win_x: win_imgui_x,
                win_y: win_imgui_y,
                win_w: width as f32,
                win_h: height as f32,
            };

            ui.window("##main")
                .position([win_imgui_x, win_imgui_y], imgui::Condition::Always)
                .size([width as f32, height as f32], imgui::Condition::Always)
                .title_bar(false)
                .resizable(false)
                .movable(false)
                .scroll_bar(false)
                .build(|| {
                    // ── Regular UI (rendered first, sits behind the dim) ────────────
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
                    let is_probing = matches!(trust_state, TrustState::Probing { .. });
                    let (conn, disc) = p.connect_button(is_connected || is_probing, &flight_id, &user_name);
                    should_disconnect = disc;
                    if conn {
                        if !server_ca.trim().is_empty() {
                            // An explicit Server CA/cert verifies the server — connect directly.
                            should_connect = true;
                        } else {
                            // TOFU: probe the server's certificate in the background, then decide.
                            let gen = PROBE_GEN.fetch_add(1, Ordering::Relaxed);
                            trust_state = TrustState::Probing {
                                gen,
                                key: known_key.clone(),
                                stored: known_stored.clone(),
                            };
                            let slot = Arc::clone(&probe_slot);
                            let host = server.trim().to_string();
                            let probe_port = port;
                            std::thread::spawn(move || {
                                let r = mumble::probe_server_cert(&host, probe_port)
                                    .map_err(|e| e.to_string());
                                *slot.lock().unwrap() = Some((gen, r));
                            });
                        }
                    }
                    p.status_display(voip_statuses.as_ref(), &status);

                    // ── File picker (dim + modal rendered last, on top of all widgets) ─
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
                                ui.open_popup("##fp");
                            }
                        }
                    }
                    if file_picker.is_some() || matches!(trust_state, TrustState::Decide(_)) {
                        p.draw_modal_dim();
                    }
                    let pick = file_picker.as_mut().map(|fp| p.file_picker_modal(fp));
                    if let Some(pick) = pick {
                        match pick {
                            FilePick::Open => {}
                            FilePick::Closed => {
                                file_picker = None;
                            }
                            FilePick::Selected(target, path) => {
                                match target {
                                    FilePickTarget::UserCert => cert_path = path,
                                    FilePickTarget::ServerCa => server_ca = path,
                                }
                                file_picker = None;
                            }
                        }
                    }

                    // ── TOFU trust flow ────────────────────────────────────────────
                    // While probing, consume the matching background result and decide whether to
                    // connect silently (cert unchanged), prompt to trust (new), or warn (changed).
                    let current = std::mem::replace(&mut trust_state, TrustState::Idle);
                    trust_state = match current {
                        TrustState::Probing { gen, key, stored } => {
                            let result = {
                                let mut guard = probe_slot.lock().unwrap();
                                match guard.as_ref() {
                                    Some((g, _)) if *g == gen => guard.take().map(|(_, r)| r),
                                    _ => None,
                                }
                            };
                            match result {
                                None => TrustState::Probing { gen, key, stored },
                                Some(Err(error)) => {
                                    ui.open_popup(TRUST_POPUP_ID);
                                    TrustState::Decide(TrustDecide {
                                        key,
                                        pem: None,
                                        kind: TrustKind::Failed { error },
                                    })
                                }
                                Some(Ok(probed)) => {
                                    let new_fp = probed.sha256;
                                    let pem = String::from_utf8_lossy(&probed.pem).into_owned();
                                    let old_fp = stored.as_deref().and_then(|p| {
                                        match mumble::cert_fingerprint(p.as_bytes()) {
                                            Ok(fp) => Some(fp),
                                            Err(e) => {
                                                warn!("stored cert for {key} could not be fingerprinted (corrupt?): {e}");
                                                None
                                            }
                                        }
                                    });
                                    match old_fp {
                                        Some(old) if old == new_fp => {
                                            // Cert unchanged — already trusted, connect silently.
                                            should_connect = true;
                                            TrustState::Idle
                                        }
                                        Some(old) => {
                                            ui.open_popup(TRUST_POPUP_ID);
                                            TrustState::Decide(TrustDecide {
                                                key,
                                                pem: Some(pem),
                                                kind: TrustKind::Changed { old, new: new_fp },
                                            })
                                        }
                                        None => {
                                            ui.open_popup(TRUST_POPUP_ID);
                                            TrustState::Decide(TrustDecide {
                                                key,
                                                pem: Some(pem),
                                                kind: TrustKind::Unknown { fingerprint: new_fp },
                                            })
                                        }
                                    }
                                }
                            }
                        }
                        other => other,
                    };

                    // Render the modal for the current state and capture the choice without
                    // holding a borrow of `trust_state` across the follow-up mutation.
                    let choice = match &trust_state {
                        // While probing (brief background handshake) no modal is shown; the dialog
                        // appears once the probe resolves into a decision.
                        TrustState::Idle | TrustState::Probing { .. } => None,
                        TrustState::Decide(d) => {
                            let view = match &d.kind {
                                TrustKind::Unknown { fingerprint } => TrustView::Unknown {
                                    server: &server,
                                    fingerprint,
                                },
                                TrustKind::Changed { old, new } => TrustView::Changed {
                                    server: &server,
                                    old,
                                    new,
                                },
                                TrustKind::Failed { error } => TrustView::Failed {
                                    server: &server,
                                    error,
                                },
                            };
                            Some(p.trust_modal(view))
                        }
                    };
                    match choice {
                        None | Some(TrustChoice::Pending) => {}
                        Some(TrustChoice::Cancel) => trust_state = TrustState::Idle,
                        Some(TrustChoice::Trust) => {
                            if let TrustState::Decide(d) = &trust_state {
                                if let Some(pem) = &d.pem {
                                    trust_to_store = Some((d.key.clone(), pem.clone()));
                                    should_connect = true;
                                }
                            }
                            trust_state = TrustState::Idle;
                        }
                    }
                });
        } // ui borrow ends here
        let draw_data = ctx.render();

        // Sync X-Plane's GL state cache before the renderer touches raw GL.
        unsafe { XPLMSetGraphicsState(0, 1, 0, 0, 1, 0, 0) };
        renderer.render(draw_data).ok();

        // Write back modified config locals.
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
        if let Some((key, pem)) = trust_to_store {
            self.known_hosts.insert_and_save(key, pem);
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
        // Disable full-screen modal dim; draw_modal_dim() draws our own clipped to the plugin window.
        ctx.style_mut().colors[imgui::StyleColor::ModalWindowDimBg as usize] = [0.0; 4];
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
