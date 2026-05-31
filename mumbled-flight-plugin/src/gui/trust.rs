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

//! TOFU connect-flow state machine types, shared between `gui` and `gui::draw::tofu`.

use mumbled_flight_core::mumble::ProbedCert;
use std::sync::{Arc, Mutex};

/// Slot a background probe thread writes its result into, tagged with the probe's generation so a
/// stale result from a superseded/cancelled probe is ignored.
pub(super) type ProbeSlot = Arc<Mutex<Option<(u64, Result<ProbedCert, String>)>>>;

/// TOFU connect-flow state machine.
pub(super) enum TrustState {
    /// No trust decision in progress.
    Idle,
    /// Background cert probe running.
    Probing {
        gen: u64,
        key: String,
        /// Previously-pinned PEM for this server, if any.
        stored: Option<String>,
    },
    /// Probe done; a modal is asking the user to decide.
    Decide(TrustDecide),
}

pub(super) struct TrustDecide {
    pub(super) key: String,
    /// PEM to persist on Trust; `None` for the failure case (nothing to store).
    pub(super) pem: Option<String>,
    pub(super) kind: TrustKind,
}

pub(super) enum TrustKind {
    /// Server never trusted before — show its fingerprint.
    Unknown { fingerprint: String },
    /// Pinned cert differs from the one now presented — possible MITM.
    Changed { old: String, new: String },
    /// The probe itself failed (unreachable, handshake error, …).
    Failed { error: String },
}
