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

//! Per-frame TOFU probe-and-decide flow, extracted from the draw loop.
//!
//! Three entry points:
//! - [`start_probe`] — called when Connect is clicked with no Server CA; spawns the background
//!   cert-fetch thread.
//! - [`poll_step`] — called every frame from the main draw; polls the probe result and returns
//!   whether to show the TOFU window.
//! - [`render_decision`] — called every frame from the TOFU window's draw callback; renders the
//!   decision UI and returns the user's action.

use log::warn;
use mumbled_flight_core::mumble;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::super::trust::{ProbeSlot, TrustDecide, TrustKind, TrustState};
use super::widgets::Ctx;

// ── Trust content rendering ───────────────────────────────────────────────────

/// What the trust window should display once the background probe has resolved.
pub(super) enum TrustView<'a> {
    Unknown { server: &'a str, fingerprint: &'a str },
    Changed { server: &'a str, old: &'a str, new: &'a str },
    Failed { server: &'a str, error: &'a str },
}

/// The user's response to the trust dialog.
pub(super) enum TrustChoice {
    Pending,
    Cancel,
    Trust,
}

/// What the TOFU window's draw callback reports back to the caller.
#[derive(Default)]
pub(super) struct TofuWindowAction {
    pub should_connect: bool,
    pub to_store: Option<(String, String)>,
    pub close: bool,
}

impl<'ui> Ctx<'ui> {
    /// Renders the TOFU decision UI directly into the current ImGui window.
    pub(super) fn trust_content(&self, view: TrustView) -> TrustChoice {
        const AMBER: [f32; 4] = [1.0, 0.8, 0.2, 1.0];
        const RED: [f32; 4] = [1.0, 0.4, 0.4, 1.0];

        match view {
            TrustView::Unknown { server, fingerprint } => {
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
                self.ui.text_wrapped(format!("Failed to retrieve the certificate from {server}:"));
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
        if self.ui.button("Trust##trust") {
            return TrustChoice::Trust;
        }
        TrustChoice::Pending
    }
}

static PROBE_GEN: AtomicU64 = AtomicU64::new(0);

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
    *trust_state = TrustState::Probing { gen, key: known_key, stored: known_stored };
    let slot = Arc::clone(probe_slot);
    let host = host.trim().to_owned();
    std::thread::spawn(move || {
        let r = mumble::probe_server_cert(&host, port).map_err(|e| e.to_string());
        *slot.lock().unwrap() = Some((gen, r));
    });
}

/// Polls the probe slot every frame from the main draw.
/// Returns `(silent_connect, want_show_tofu_window)`.
pub(super) fn poll_step(trust_state: &mut TrustState, probe_slot: &ProbeSlot) -> (bool, bool) {
    let TrustState::Probing { gen, key, stored } =
        std::mem::replace(trust_state, TrustState::Idle)
    else {
        return (false, false);
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
        return (false, false);
    };
    let (new_state, silent, show) = resolve_probe(result, key, stored);
    *trust_state = new_state;
    (silent, show)
}

/// Renders the TOFU decision UI from inside the TOFU XPLM window's draw callback.
/// Called every frame while the TOFU window is visible.
pub(super) fn render_decision(
    trust_state: &mut TrustState,
    p: &Ctx<'_>,
    server: &str,
) -> TofuWindowAction {
    let TrustState::Decide(d) = trust_state else {
        return TofuWindowAction { close: true, ..Default::default() };
    };
    let view = match &d.kind {
        TrustKind::Unknown { fingerprint } => TrustView::Unknown { server, fingerprint },
        TrustKind::Changed { old, new } => TrustView::Changed { server, old, new },
        TrustKind::Failed { error } => TrustView::Failed { server, error },
    };
    match p.trust_content(view) {
        TrustChoice::Pending => TofuWindowAction::default(),
        TrustChoice::Cancel => {
            *trust_state = TrustState::Idle;
            TofuWindowAction { close: true, ..Default::default() }
        }
        TrustChoice::Trust => {
            let to_store = d.pem.as_ref().map(|pem| (d.key.clone(), pem.clone()));
            let connect = to_store.is_some();
            *trust_state = TrustState::Idle;
            TofuWindowAction { should_connect: connect, to_store, close: true }
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
                TrustState::Decide(TrustDecide { key, pem: None, kind: TrustKind::Failed { error } }),
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
                TrustState::Decide(TrustDecide { key, pem: None, kind: TrustKind::Failed { error } }),
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
            TrustState::Decide(TrustDecide {
                key,
                pem: Some(pem),
                kind: TrustKind::Changed { old, new: new_fp },
            }),
            false,
            true,
        ),
        None => (
            TrustState::Decide(TrustDecide {
                key,
                pem: Some(pem),
                kind: TrustKind::Unknown { fingerprint: new_fp },
            }),
            false,
            true,
        ),
    }
}
