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

//! Transport abstraction — the pluggable boundary between the audio/cockpit stack and the
//! wire protocol that carries voice between participants.
//!
//! # Why this exists
//!
//! `run_mumble_stack` does two separable things:
//!
//! 1. **Audio + cockpit plumbing** (transport-agnostic): mic capture, the radio loopback,
//!    the ambient/IC playback mixers, and the shared [`CockpitState`]. This is identical no
//!    matter how bytes reach the other pilot.
//! 2. **Moving voice between participants** (transport-specific): connecting, authenticating,
//!    encrypting, encoding, attaching position, routing per "channel", and demuxing inbound
//!    audio back to the right renderer.
//!
//! Everything in (2) is what a [`VoipTransport`] owns. `run_mumble_stack` now builds a
//! [`TransportContext`] from (1) and hands it to the selected backend. The spatial math
//! (`voip::spatial`), audio I/O (`audio`), cockpit state (`state`), and both frontends
//! (CLI / plugin) are untouched by the choice of backend.
//!
//! # The three backends
//!
//! | [`TransportKind`] | Module | Connections per pilot | Status |
//! |-------------------|--------|-----------------------|--------|
//! | [`TransportKind::Mumble`]   | [`mumble`] | **4** (Mumble allows one channel per connection) | implemented — the current production path |
//! | [`TransportKind::WebRtcSfu`] | [`webrtc`] | **1** (multi-track over BUNDLE; an SFU fans out) | sketch / not implemented |
//! | [`TransportKind::Quic`]      | [`quic`]   | **1** (DATAGRAM flows over one QUIC connection) | sketch / not implemented |
//!
//! The Mumble backend needs four sockets only because Mumble couples "one user = one channel".
//! Both WebRTC and QUIC multiplex many media tracks over a single connection, so they collapse
//! the four `*_voice` / `*_ic` / `*_PA` / `*_radio` clients into **one** session carrying four
//! labelled tracks. See the per-backend module docs for the full mapping.
//!
//! # What is shared across backends
//!
//! The role semantics deliberately live *outside* any backend so they are written once:
//!
//! - **TX gating** — [`tx_decision`] resolves, per [`ClientRole`], whether this track should
//!   transmit right now and at what gain. Every backend calls it before encoding a frame.
//! - **Spatialization** — `voip::spatial` + `MumbleVoipClient::spatialize` render inbound mono
//!   PCM to stereo using cockpit geometry. A backend only needs to deliver `(mono, position,
//!   role)`; the rendering is identical.
//! - **Codec & coordinate convention** — Opus at 48 kHz mono ([`crate::mumble::OPUS_FRAME_SAMPLES`])
//!   and `voip::xplane_to_mumble` are reused verbatim.

use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::sync::{broadcast, mpsc};

use super::voip::client::{ClientCert, ClientRole, ServerTrust};
use super::{TestClient, VoipStatuses};
use crate::state::CockpitState;

mod mumble;
mod quic;
mod webrtc;

/// Selects which wire protocol backend [`build`] instantiates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportKind {
    /// The current production path: four Mumble client connections per pilot.
    #[default]
    Mumble,
    /// WebRTC + SFU: one connection, four media tracks, server-side selective forwarding.
    WebRtcSfu,
    /// QUIC: one connection, unreliable DATAGRAM flows, custom relay or Media-over-QUIC.
    Quic,
}

/// All transport-agnostic handles a backend needs to bring a pilot online.
///
/// Built by `run_mumble_stack` after the mic, radio, and playback chains are wired, then moved
/// into [`VoipTransport::run`]. Every field is cheaply cloneable / `'static` so a backend can
/// fan it out across as many tasks (or as few connections) as its design calls for.
pub struct TransportContext {
    pub state: Arc<Mutex<CockpitState>>,
    pub user_name: String,
    pub session_id: String,
    /// Shared secret sent at authentication time (empty = none).
    pub password: String,
    /// Optional client certificate used as the TLS/DTLS identity.
    pub client_cert: Option<Arc<ClientCert>>,
    /// Optional trust anchor(s) for verifying the server's certificate.
    pub server_trust: Option<Arc<ServerTrust>>,
    pub server_host: String,
    pub server_port: u16,
    /// Microphone PCM, broadcast to every TX track. Backends call `.subscribe()` per track.
    pub mic_tx: broadcast::Sender<Vec<f32>>,
    /// Radio-loopback PCM (COM relay), if a radio source is configured.
    pub radio_tx: Option<broadcast::Sender<Vec<f32>>>,
    /// Ambient playback sink (Voice + PA + Radio RX).
    pub ambient_pb_tx: mpsc::Sender<Vec<f32>>,
    /// Intercom playback sink (flat headphone mix + radio monitor).
    pub ic_pb_tx: mpsc::Sender<Vec<f32>>,
    /// Per-track live connection status, surfaced in the GUI.
    pub statuses: VoipStatuses,
    /// Live-adjustable stereo width for spatialized playback.
    pub spatial_width: Arc<AtomicU32>,
    /// Which role(s) to bring up (`All` in normal use; a single role for CLI `--test`).
    pub test_client: TestClient,
    /// Fixed position override for test clients.
    pub test_pos: Option<[f32; 3]>,
}

/// A pluggable voice transport. One implementation per wire protocol.
///
/// `run` follows the existing fire-and-forget contract: it brings up the per-track tasks
/// (spawning onto the tokio runtime) and returns `Ok(())` once they are established; the tasks
/// then live for the duration of the process / until reconnect. An `Err` means the stack could
/// not be started at all.
pub trait VoipTransport: Send + Sync {
    /// Short identifier for logs (`"mumble"`, `"webrtc-sfu"`, `"quic"`).
    fn name(&self) -> &'static str;

    /// Bring the pilot online over this transport using `ctx`.
    fn run(self: Box<Self>, ctx: TransportContext) -> BoxFuture<'static, anyhow::Result<()>>;
}

/// Instantiates the backend for `kind`.
pub fn build(kind: TransportKind) -> Box<dyn VoipTransport> {
    match kind {
        TransportKind::Mumble => Box::new(mumble::MumbleTransport),
        TransportKind::WebRtcSfu => Box::new(webrtc::WebrtcSfuTransport),
        TransportKind::Quic => Box::new(quic::QuicTransport),
    }
}

/// The canonical TX gate, shared by every backend.
///
/// Resolves, for a given [`ClientRole`] and the current cockpit state, whether this track
/// should be transmitting and the linear gain to apply before encoding. The per-role *predicates*
/// themselves (`should_transmit_*`, `spkr_vol`) live on [`CockpitState`]; this function only
/// maps a role to the right predicate so the mapping is not duplicated across transports.
///
/// Returns `(is_active, tx_gain)`.
pub fn tx_decision(role: ClientRole, s: &CockpitState) -> (bool, f32) {
    match role {
        ClientRole::Radio { has_source } => (s.should_transmit_radio(has_source), s.spkr_vol),
        ClientRole::Ic => (s.should_transmit_ic(), 1.0),
        ClientRole::Pa => (s.should_transmit_pa(), 2.0),
        ClientRole::Voice => (true, 1.0),
    }
}
