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
//! - [`advance`] — called every frame; polls the probe result and/or renders the decision modal.
//! - [`needs_dim`] — true when the modal dim overlay should cover the window.

use log::warn;
use mumbled_flight_core::mumble;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::super::{TrustDecide, TrustKind, TrustState};
use super::panels::{TrustChoice, TrustView, TRUST_POPUP_ID};
use super::widgets::Ctx;

static PROBE_GEN: AtomicU64 = AtomicU64::new(0);

type ProbeSlot = Arc<Mutex<Option<(u64, Result<mumble::ProbedCert, String>)>>>;

/// What the draw loop needs back from a single [`advance`] tick.
pub(super) struct TofuResult {
    /// Set `should_connect = true` in the draw loop when this is true.
    pub should_connect: bool,
    /// Call `known_hosts.insert_and_save(key, pem)` when this is `Some`.
    pub to_store: Option<(String, String)>,
}

/// True when the modal dim overlay should be drawn (a decision dialog is open).
pub(super) fn needs_dim(trust_state: &TrustState) -> bool {
    matches!(trust_state, TrustState::Decide(_))
}

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

/// Runs one frame of the TOFU flow: polls the probe slot, then renders the decision modal.
/// Must be called every frame while `trust_state` may be non-Idle.
pub(super) fn advance(
    trust_state: &mut TrustState,
    probe_slot: &ProbeSlot,
    ui: &imgui::Ui,
    p: &Ctx<'_>,
    server: &str,
) -> TofuResult {
    let silent_connect = poll_probe(trust_state, probe_slot, ui);
    let (user_connect, to_store) = render_decision(trust_state, p, server);
    TofuResult { should_connect: silent_connect || user_connect, to_store }
}

// ── private helpers ───────────────────────────────────────────────────────────

/// Checks the probe slot. If a matching result has arrived, transitions `trust_state` out of
/// `Probing`. Returns `true` when the cert is unchanged and the connection can proceed silently.
fn poll_probe(trust_state: &mut TrustState, probe_slot: &ProbeSlot, ui: &imgui::Ui) -> bool {
    let TrustState::Probing { gen, key, stored } = std::mem::replace(trust_state, TrustState::Idle)
    else {
        return false;
    };
    let result = {
        let mut guard = probe_slot.lock().unwrap();
        if guard.as_ref().map_or(false, |(g, _)| *g == gen) {
            guard.take().map(|(_, r)| r)
        } else {
            None
        }
    };
    let Some(result) = result else {
        *trust_state = TrustState::Probing { gen, key, stored }; // still running
        return false;
    };
    let (new_state, silent) = resolve_probe(result, key, stored, ui);
    *trust_state = new_state;
    silent
}

/// Converts a completed probe result into the next `TrustState`. Returns the new state and
/// whether to connect immediately without showing a dialog (cert unchanged).
fn resolve_probe(
    result: Result<mumble::ProbedCert, String>,
    key: String,
    stored: Option<String>,
    ui: &imgui::Ui,
) -> (TrustState, bool) {
    let probed = match result {
        Err(error) => {
            ui.open_popup(TRUST_POPUP_ID);
            return (
                TrustState::Decide(TrustDecide { key, pem: None, kind: TrustKind::Failed { error } }),
                false,
            );
        }
        Ok(p) => p,
    };

    let new_fp = probed.sha256;
    let pem = String::from_utf8_lossy(&probed.pem).into_owned();
    let old_fp = stored.as_deref().and_then(|p| {
        mumble::cert_fingerprint(p.as_bytes())
            .map_err(|e| warn!("stored cert for {key} could not be fingerprinted (corrupt?): {e}"))
            .ok()
    });

    match old_fp {
        Some(old) if old == new_fp => (TrustState::Idle, true),
        Some(old) => {
            ui.open_popup(TRUST_POPUP_ID);
            (
                TrustState::Decide(TrustDecide {
                    key,
                    pem: Some(pem),
                    kind: TrustKind::Changed { old, new: new_fp },
                }),
                false,
            )
        }
        None => {
            ui.open_popup(TRUST_POPUP_ID);
            (
                TrustState::Decide(TrustDecide {
                    key,
                    pem: Some(pem),
                    kind: TrustKind::Unknown { fingerprint: new_fp },
                }),
                false,
            )
        }
    }
}

/// Renders the trust-decision modal when `trust_state` is `Decide`.
/// Returns `(should_connect, cert_to_store)`.
fn render_decision(
    trust_state: &mut TrustState,
    p: &Ctx<'_>,
    server: &str,
) -> (bool, Option<(String, String)>) {
    let TrustState::Decide(d) = trust_state else {
        return (false, None);
    };
    let view = match &d.kind {
        TrustKind::Unknown { fingerprint } => TrustView::Unknown { server, fingerprint },
        TrustKind::Changed { old, new } => TrustView::Changed { server, old, new },
        TrustKind::Failed { error } => TrustView::Failed { server, error },
    };
    match p.trust_modal(view) {
        TrustChoice::Pending => (false, None),
        TrustChoice::Cancel => {
            *trust_state = TrustState::Idle;
            (false, None)
        }
        TrustChoice::Trust => {
            let to_store = d.pem.as_ref().map(|pem| (d.key.clone(), pem.clone()));
            let connect = to_store.is_some();
            *trust_state = TrustState::Idle;
            (connect, to_store)
        }
    }
}
