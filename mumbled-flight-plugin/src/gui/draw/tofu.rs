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

//! TOFU probe-and-decide flow and overlay rendering for the trust popup.
//!
//! Entry points for the main draw loop:
//! - [`start_probe`] — called when Connect is clicked with no Server CA; spawns the background
//!   cert-fetch thread.
//! - [`poll_step`] — called every frame from the main draw; polls the probe result and returns
//!   whether to show the TOFU window.
//! - [`render_tofu_overlay`] — renders the decision UI as an imgui overlay window inside the
//!   current frame and returns the user's action.

use log::warn;
use mumbled_flight_core::mumble;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::gui::trust::PROBE_GEN;

use super::super::trust::{PendingTrust, ProbeSlot, TrustKind, TrustState};
use super::widgets::{Ctx, LABEL_COL_X};

// ── Trust content rendering ───────────────────────────────────────────────────

/// What the trust window should display once the background probe has resolved.
pub(super) enum TrustPrompt<'a> {
    Unknown {
        server: &'a str,
        fingerprint: &'a str,
    },
    Changed {
        server: &'a str,
        old: &'a str,
        new: &'a str,
    },
    Failed {
        server: &'a str,
        error: &'a str,
    },
}

/// The user's response to the trust dialog.
pub(super) enum TrustChoice {
    Pending,
    Cancel,
    Trust,
}

/// Result of [`poll_step`]: what the main draw loop should do after polling the probe.
#[derive(Default)]
pub(super) struct ProbeOutcome {
    /// Cert is unchanged from the pinned one — connect immediately without showing the modal.
    pub silent_connect: bool,
}

/// What the TOFU overlay reports back to the caller.
#[derive(Default)]
pub(super) struct TofuDrawResult {
    pub should_connect: bool,
    pub to_store: Option<(String, String)>,
}

impl<'ui> Ctx<'ui> {
    /// Renders the TOFU decision UI directly into the current ImGui window.
    pub(super) fn trust_content(&self, view: TrustPrompt) -> TrustChoice {
        const AMBER: [f32; 4] = [1.0, 0.8, 0.2, 1.0];
        const RED: [f32; 4] = [1.0, 0.4, 0.4, 1.0];

        match view {
            TrustPrompt::Unknown {
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
            TrustPrompt::Changed { server, old, new } => {
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
            TrustPrompt::Failed { server, error } => {
                self.ui.text_colored(RED, "Could not reach server");
                self.ui.spacing();
                self.ui
                    .text_wrapped(format!("Failed to retrieve the certificate from {server}:"));
                self.ui.text_wrapped(error);
                self.ui.spacing();
                if self.ui.button("Close##trust") {
                    return TrustChoice::Cancel;
                }
                TrustChoice::Pending
            }
        }
    }

    fn trust_buttons(&self) -> TrustChoice {
        if self.ui.button("Cancel##trust") {
            return TrustChoice::Cancel;
        }
        self.ui.same_line();
        const RED_BTN:     [f32; 4] = [0.75, 0.15, 0.15, 1.0];
        const RED_BTN_HOV: [f32; 4] = [0.90, 0.25, 0.25, 1.0];
        const RED_BTN_ACT: [f32; 4] = [0.60, 0.10, 0.10, 1.0];
        let _c0 = self.ui.push_style_color(imgui::StyleColor::Button,        RED_BTN);
        let _c1 = self.ui.push_style_color(imgui::StyleColor::ButtonHovered, RED_BTN_HOV);
        let _c2 = self.ui.push_style_color(imgui::StyleColor::ButtonActive,  RED_BTN_ACT);
        if self.ui.button("Trust##trust") {
            return TrustChoice::Trust;
        }
        TrustChoice::Pending
    }
}

// ── Overlay rendering ─────────────────────────────────────────────────────────

/// Renders the TOFU decision UI as an imgui overlay window inside the current frame.
///
/// Called from `GuiState::draw` so the full mouse-down → mouse-up cycle for the
/// Trust/Cancel buttons is visible in one imgui frame.
/// `pos` and `size` are in imgui virtual-screen coordinates (already computed by the caller).
pub(super) fn render_tofu_overlay(
    ui: &imgui::Ui,
    pad_r: f32,
    trust_state: &mut TrustState,
    pos: [f32; 2],
    size: [f32; 2],
    server: &str,
) -> TofuDrawResult {
    let fw = (size[0] - LABEL_COL_X - pad_r).max(80.0);
    let p = Ctx { ui, fw };
    let mut result = TofuDrawResult::default();
    ui.window("Server Certificate")
        .position(pos, imgui::Condition::Always)
        .size(size, imgui::Condition::Always)
        .title_bar(true)
        .resizable(false)
        .movable(false)
        .build(|| {
            result = render_decision(trust_state, &p, server);
        });
    result
}

// ── Trust modal types and rendering ──────────────────────────────────────────

/// Called when Connect is clicked and no explicit Server CA is configured.
/// Transitions `trust_state` to `Probing` and spawns a background thread that
/// fetches the server's TLS certificate.
pub(super) fn start_probe(
    trust_state: &mut TrustState,
    probe_slot: &ProbeSlot,
    known_key: String,
    known_stored: Option<String>,
    host: &str,
    port: u16,
) {
    let gen = PROBE_GEN.fetch_add(1, Ordering::Relaxed);
    *trust_state = TrustState::Probing {
        gen,
        key: known_key,
        stored: known_stored,
    };
    let slot = Arc::clone(probe_slot);
    let host = host.trim().to_owned();
    std::thread::spawn(move || {
        let r = mumble::probe_server_cert(&host, port).map_err(|e| e.to_string());
        *slot.lock().unwrap() = Some((gen, r));
    });
}

/// Polls the probe slot every frame from the main draw.
pub(super) fn poll_step(trust_state: &mut TrustState, probe_slot: &ProbeSlot) -> ProbeOutcome {
    let (gen, key, stored) = match std::mem::replace(trust_state, TrustState::Idle) {
        TrustState::Probing { gen, key, stored } => (gen, key, stored),
        other => {
            *trust_state = other;
            return ProbeOutcome::default();
        }
    };
    let result = {
        let mut guard = probe_slot.lock().unwrap();
        if guard.as_ref().is_some_and(|(g, _)| *g == gen) {
            guard.take().map(|(_, r)| r)
        } else {
            None
        }
    };
    let Some(result) = result else {
        *trust_state = TrustState::Probing { gen, key, stored }; // still running
        return ProbeOutcome::default();
    };
    let (new_state, silent, show) = resolve_probe(result, key, stored);
    *trust_state = new_state;
    let _ = show;
    ProbeOutcome { silent_connect: silent }
}

/// Renders the TOFU decision UI from inside an imgui window.
pub(super) fn render_decision(
    trust_state: &mut TrustState,
    p: &Ctx<'_>,
    server: &str,
) -> TofuDrawResult {
    let TrustState::Decide(d) = trust_state else {
        return TofuDrawResult::default();
    };
    let view = match &d.kind {
        TrustKind::Unknown { fingerprint } => TrustPrompt::Unknown {
            server,
            fingerprint,
        },
        TrustKind::Changed { old, new } => TrustPrompt::Changed { server, old, new },
        TrustKind::Failed { error } => TrustPrompt::Failed { server, error },
    };
    match p.trust_content(view) {
        TrustChoice::Pending => TofuDrawResult::default(),
        TrustChoice::Cancel => {
            *trust_state = TrustState::Idle;
            TofuDrawResult::default()
        }
        TrustChoice::Trust => {
            // `Trust` is only reachable via `trust_buttons()`, which is only called for
            // `TrustPrompt::Unknown` and `TrustPrompt::Changed`. Both of those are constructed from
            // `TrustKind` variants whose `pem` is always `Some` (the probe succeeded and returned
            // a PEM). `TrustPrompt::Failed` — the only variant with `pem: None` — shows "Close"
            // only, so `TrustChoice::Trust` is never returned from it. Therefore `to_store` is
            // always `Some` here and `connect` is always `true`. If a future `TrustKind` variant
            // with `pem: None` accidentally uses `trust_buttons()`, the dialog would close
            // silently without connecting — add a "Close"-only button to that variant instead.
            let to_store = d.pem.as_ref().map(|pem| (d.key.clone(), pem.clone()));
            let connect = to_store.is_some();
            *trust_state = TrustState::Idle;
            TofuDrawResult { should_connect: connect, to_store }
        }
    }
}

// ── private helpers ───────────────────────────────────────────────────────────

/// Converts a completed probe result into the next `TrustState`.
/// Returns `(new_state, silent_connect, want_show_tofu)`.
fn resolve_probe(
    result: Result<mumble::ProbedCert, String>,
    key: String,
    stored: Option<String>,
) -> (TrustState, bool, bool) {
    let probed = match result {
        Err(error) => {
            return (
                TrustState::Decide(PendingTrust {
                    key,
                    pem: None,
                    kind: TrustKind::Failed { error },
                }),
                false,
                true,
            );
        }
        Ok(p) => p,
    };

    let new_fp = probed.sha256;
    let pem = match String::from_utf8(probed.pem) {
        Ok(s) => s,
        Err(e) => {
            let error = format!("server certificate encoding error: {e}");
            return (
                TrustState::Decide(PendingTrust {
                    key,
                    pem: None,
                    kind: TrustKind::Failed { error },
                }),
                false,
                true,
            );
        }
    };
    let old_fp = stored.as_deref().and_then(|p| {
        mumble::cert_fingerprint(p.as_bytes())
            .map_err(|e| warn!("stored cert for {key} could not be fingerprinted (corrupt?): {e}"))
            .ok()
    });

    match old_fp {
        Some(old) if old == new_fp => (TrustState::Idle, true, false),
        Some(old) => (
            TrustState::Decide(PendingTrust {
                key,
                pem: Some(pem),
                kind: TrustKind::Changed { old, new: new_fp },
            }),
            false,
            true,
        ),
        None => (
            TrustState::Decide(PendingTrust {
                key,
                pem: Some(pem),
                kind: TrustKind::Unknown {
                    fingerprint: new_fp,
                },
            }),
            false,
            true,
        ),
    }
}
