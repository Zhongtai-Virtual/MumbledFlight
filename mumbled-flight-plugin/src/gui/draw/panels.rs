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

//! The individual config panels rendered onto a [`Ctx`] — connection fields,
//! audio sliders, device pickers, log level, the connect button, and the
//! per-client status display.

use log::{debug, warn, LevelFilter};
use mumbled_flight_core::mumble::{ClientStatus, VoipStatuses};

use super::widgets::{Ctx, LABEL_COL_X};

pub(super) struct BrowseClicks {
    pub(super) cert: bool,
    pub(super) ca: bool,
}

/// Shared popup id for the TOFU trust modal. The same string must be used for `open_popup` (in
/// the draw loop) and `modal_popup_config` (in `trust_modal`); it has no `##`-prefixed title since
/// the heading varies by case and is drawn inside the modal.
pub(super) const TRUST_POPUP_ID: &str = "##trust";

/// What the trust modal should display this frame. (Shown only once the background probe has
/// resolved into a decision — while probing, no modal is drawn.)
pub(super) enum TrustView<'a> {
    /// Server not trusted before — show its fingerprint.
    Unknown {
        server: &'a str,
        fingerprint: &'a str,
    },
    /// Pinned cert differs from the one presented — possible MITM.
    Changed {
        server: &'a str,
        old: &'a str,
        new: &'a str,
    },
    /// The probe failed.
    Failed { server: &'a str, error: &'a str },
}

/// The user's response to the trust modal.
pub(super) enum TrustChoice {
    /// Still showing — take no action.
    Pending,
    /// Dismissed; do not connect.
    Cancel,
    /// Trust the certificate and connect.
    Trust,
}

impl<'ui> Ctx<'ui> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn connection_fields(
        &self,
        server: &mut String,
        port: &mut u16,
        server_password: &mut String,
        cert_path: &mut String,
        cert_pass: &mut String,
        server_ca: &mut String,
        flight_id: &mut String,
        user_name: &mut String,
    ) -> BrowseClicks {
        self.row("Server", "##srv", server);
        {
            self.ui.text("Port");
            self.ui.same_line();
            self.ui.set_cursor_pos([LABEL_COL_X, self.ui.cursor_pos()[1]]);
            self.ui.set_next_item_width(self.fw);
            let mut p = *port as i32;
            if self
                .ui
                .input_int("##port", &mut p)
                .step(0)
                .step_fast(0)
                .build()
            {
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
            let cert = self.file_row("Client Cert", "##cert", cert_path);
            self.password_row("Cert Pass", "##certpw", cert_pass);
            let ca = self.file_row("Server CA", "##sca", server_ca);
            self.ui.spacing();
            return BrowseClicks { cert, ca };
        }
        self.ui.spacing();
        BrowseClicks {
            cert: false,
            ca: false,
        }
    }

    pub(super) fn audio_controls(
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

    pub(super) fn denoise_toggle(&self, denoise: &mut bool, is_connected: bool) {
        self.ui.text("Denoise");
        self.ui.same_line();
        self.ui.set_cursor_pos([LABEL_COL_X, self.ui.cursor_pos()[1]]);
        let _dis = self.ui.begin_disabled(is_connected);
        self.ui.checkbox("##denoise", denoise);
    }

    pub(super) fn output_device_pickers(
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

    pub(super) fn mic_picker(
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

    pub(super) fn radio_picker(
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

    pub(super) fn log_level_picker(&self, log_level: &mut LevelFilter) {
        const LOG_LEVELS: &[LevelFilter] = &[
            LevelFilter::Error,
            LevelFilter::Warn,
            LevelFilter::Info,
            LevelFilter::Debug,
        ];
        let level_preview = format!("{log_level}");
        self.ui.text("Log Level");
        self.ui.same_line();
        self.ui.set_cursor_pos([LABEL_COL_X, self.ui.cursor_pos()[1]]);
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
    pub(super) fn connect_button(
        &self,
        is_connected: bool,
        flight_id: &str,
        user_name: &str,
    ) -> (bool, bool) {
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

    /// TOFU trust modal, shown when Connect is clicked with no Server CA. Centred over the XPLM
    /// window like the file picker; the heading and buttons vary by `view`. Returns the user's
    /// choice (or `Pending` while still open). `Trust` is only ever returned for the `Unknown` /
    /// `Changed` cases — `Probing` and `Failed` offer only a dismiss button.
    pub(super) fn trust_modal(&self, view: TrustView) -> TrustChoice {
        let win_cx = self.win_x + self.win_w * 0.5;
        let win_cy = self.win_y + self.win_h * 0.5;
        unsafe {
            imgui_sys::igSetNextWindowPos(
                imgui_sys::ImVec2 { x: win_cx, y: win_cy },
                imgui::Condition::Always as i32,
                imgui_sys::ImVec2 { x: 0.5, y: 0.5 },
            );
            // Fixed width (auto height) so `text_wrapped` and the long fingerprints have a wrap
            // boundary; never wider than the XPLM window, the limit for mouse-event delivery.
            let w = 430.0_f32.min(self.win_w);
            imgui_sys::igSetNextWindowSize(
                imgui_sys::ImVec2 { x: w, y: 0.0 },
                imgui::Condition::Always as i32,
            );
        }
        let Some(_token) = self
            .ui
            .modal_popup_config(TRUST_POPUP_ID)
            .title_bar(false)
            .resizable(false)
            .movable(false)
            .begin_popup()
        else {
            return TrustChoice::Pending;
        };

        const AMBER: [f32; 4] = [1.0, 0.8, 0.2, 1.0];
        const RED: [f32; 4] = [1.0, 0.4, 0.4, 1.0];

        match view {
            TrustView::Unknown {
                server,
                fingerprint,
            } => {
                self.ui.text_colored(AMBER, "Unrecognized server");
                self.ui.spacing();
                self.ui.text_wrapped(
                    "This server's certificate has not been trusted before. Confirm the SHA-256 \
                     fingerprint matches the one the server operator gave you, then choose Trust \
                     to remember it for future connections.",
                );
                self.ui.spacing();
                self.ui.text_wrapped(format!("Server: {server}"));
                self.ui.text_disabled("SHA-256 fingerprint:");
                self.ui.text_wrapped(fingerprint);
                self.ui.spacing();
                self.trust_buttons()
            }
            TrustView::Changed { server, old, new } => {
                self.ui.text_colored(RED, "Server certificate CHANGED");
                self.ui.spacing();
                self.ui.text_wrapped(
                    "WARNING: this server is presenting a different certificate from the one you \
                     previously trusted. This can be a legitimate change — or someone intercepting \
                     the connection (MITM). Only Trust if you expected the certificate to change.",
                );
                self.ui.spacing();
                self.ui.text_wrapped(format!("Server: {server}"));
                self.ui.text_disabled("Previously trusted:");
                self.ui.text_wrapped(old);
                self.ui.text_disabled("Now presented:");
                self.ui.text_wrapped(new);
                self.ui.spacing();
                self.trust_buttons()
            }
            TrustView::Failed { server, error } => {
                self.ui.text_colored(RED, "Could not reach server");
                self.ui.spacing();
                self.ui
                    .text_wrapped(format!("Failed to retrieve the certificate from {server}:"));
                self.ui.text_wrapped(error);
                self.ui.spacing();
                if self.ui.button("Close##trust") {
                    self.ui.close_current_popup();
                    return TrustChoice::Cancel;
                }
                TrustChoice::Pending
            }
        }
    }

    /// Shared `Cancel` / `Trust` button row for the decision cases of [`Self::trust_modal`].
    fn trust_buttons(&self) -> TrustChoice {
        let cancel = self.ui.button("Cancel##trust");
        self.ui.same_line();
        let trust = self.ui.button("Trust##trust");
        if cancel {
            self.ui.close_current_popup();
            return TrustChoice::Cancel;
        }
        if trust {
            self.ui.close_current_popup();
            return TrustChoice::Trust;
        }
        TrustChoice::Pending
    }

    pub(super) fn status_display(&self, voip_statuses: Option<&VoipStatuses>, status: &str) {
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
            let disabled_col =
                self.ui.clone_style().colors[imgui::StyleColor::TextDisabled as usize];
            let _col = self
                .ui
                .push_style_color(imgui::StyleColor::Text, disabled_col);
            self.ui.text_wrapped(status);
        }
    }
}
