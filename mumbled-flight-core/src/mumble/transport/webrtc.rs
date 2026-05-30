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

//! WebRTC + SFU backend — **design sketch, not yet implemented**.
//!
//! # One connection, four tracks
//!
//! Where the Mumble backend opens four connections (one per channel), a WebRTC backend opens
//! **one** `RTCPeerConnection` per pilot to a Selective Forwarding Unit (SFU) and carries the
//! four roles as four labelled media tracks multiplexed over RTP BUNDLE:
//!
//! | [`ClientRole`](crate::mumble::voip::client::ClientRole) | WebRTC mapping |
//! |------|----------------|
//! | `Voice` | send-track, gated by [`tx_decision`](super::tx_decision); pose on a DataChannel; SFU forwards only to same-zone subscribers |
//! | `Ic`    | send-track, gated by IC toggle; RX rendered flat (no spatialization) |
//! | `Pa`    | send-track, gated by PA PTT; TX-only |
//! | `Radio` | send-track from the radio loopback; TX-only |
//!
//! A track consumes uplink only while its gate is open, exactly as the Mumble clients only
//! emit packets while transmitting.
//!
//! # What the SFU replaces
//!
//! - **Rendezvous** — Mumble's "same Flight ID → same channels" becomes an SFU *room* keyed on
//!   [`session_id`](super::TransportContext::session_id). A small signaling server (WebSocket /
//!   HTTPS) exchanges SDP offers/answers and ICE candidates to join that room.
//! - **Server-side fan-out** — the SFU forwards each pilot's tracks to the others *without
//!   mixing*, so inbound streams stay individual and the existing spatialization is unchanged.
//!   This is the property that lets the crowded `*_ambient_fbo` case scale (a P2P mesh cannot).
//! - **Zone routing** — the Voice client's FBO↔aircraft channel move becomes a subscription
//!   change in the room, so the SFU keeps pre-filtering who you hear.
//! - **Encryption & NAT traversal** — DTLS-SRTP (mandatory) and ICE = STUN + TURN replace
//!   `mumble-protocol`'s crypt and the current direct-TCP/UDP connect. The existing
//!   [`ServerTrust`](crate::mumble::voip::client::ServerTrust) pinning maps onto a rustls /
//!   DTLS certificate verifier.
//!
//! # What is reused unchanged
//!
//! [`tx_decision`](super::tx_decision) for TX gating, `voip::spatial` + `spatialize` for RX
//! rendering, Opus at [`OPUS_FRAME_SAMPLES`](crate::mumble::OPUS_FRAME_SAMPLES), and
//! `voip::xplane_to_mumble` for the coordinate convention. The backend only has to deliver
//! `(mono_pcm, position, role)` inbound and pull `(mono_pcm, position)` outbound.
//!
//! # Sketch of `run`
//!
//! ```text
//! 1. Open one RTCPeerConnection (webrtc-rs) to the SFU; complete DTLS, verify server cert.
//! 2. Join the room `session_id` via the signaling server.
//! 3. Add four outbound Opus tracks (Voice/IC/PA/Radio). For each, subscribe to mic_tx
//!    (or radio_tx) and, per frame, consult tx_decision(role, &state) before sending.
//! 4. Open a DataChannel for pose; publish xplane_to_mumble(state.pos) for Voice/Radio.
//! 5. On each inbound track, decode Opus → mono, read the sender's pose from its DataChannel,
//!    render via spatialize() (or flat for IC), and push to ambient_pb_tx / ic_pb_tx.
//! 6. Mirror the local radio loopback into ic_pb_tx, as the Mumble backend does.
//! ```
//!
//! Implementing this means adding `webrtc` (webrtc-rs) to the core crate and standing up the
//! signaling server + STUN/TURN; until then `run` returns an error.

use futures::future::BoxFuture;

use super::{TransportContext, VoipTransport};

/// WebRTC + SFU transport (one connection, four tracks). Not yet implemented.
pub struct WebrtcSfuTransport;

impl VoipTransport for WebrtcSfuTransport {
    fn name(&self) -> &'static str {
        "webrtc-sfu"
    }

    fn run(self: Box<Self>, _ctx: TransportContext) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            anyhow::bail!(
                "webrtc-sfu transport is not yet implemented; see \
                 mumble::transport::webrtc module docs for the design"
            )
        })
    }
}
