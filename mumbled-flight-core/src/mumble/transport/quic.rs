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

//! QUIC backend — **design sketch, not yet implemented**.
//!
//! # One connection, unreliable datagrams
//!
//! A QUIC backend opens **one** `quinn` connection per pilot and carries the four roles as
//! flows multiplexed over it. The critical design rule: **voice rides QUIC DATAGRAM frames
//! (RFC 9221), not streams.** QUIC streams are reliable and ordered, which is wrong for
//! real-time audio — a retransmitted Opus frame arrives too late to play and only adds
//! head-of-line latency. DATAGRAMs give unreliable delivery plus QUIC's TLS 1.3, congestion
//! control, and connection migration.
//!
//! | [`ClientRole`](crate::mumble::voip::client::ClientRole) | QUIC mapping |
//! |------|--------------|
//! | `Voice` | Opus in DATAGRAM frames tagged with a role/stream id; pose alongside; gated by [`tx_decision`](super::tx_decision) |
//! | `Ic`    | Opus DATAGRAMs, gated by IC toggle; rendered flat on RX |
//! | `Pa`    | Opus DATAGRAMs, gated by PA PTT; TX-only |
//! | `Radio` | Opus DATAGRAMs from the radio loopback; TX-only |
//!
//! A reliable bidirectional QUIC stream carries control/signaling (room join keyed on
//! [`session_id`](super::TransportContext::session_id), peer presence, pose updates if not
//! inlined with audio).
//!
//! # Topology choice
//!
//! - **Two-pilot cockpit (small N):** a direct `quinn` connection between peers is the sweet
//!   spot — lowest latency, single connection, TLS 1.3, four tracks. The
//!   [`ServerTrust`](crate::mumble::voip::client::ServerTrust) pinning maps cleanly onto a
//!   rustls certificate verifier (quinn uses rustls).
//! - **Crowded FBO (many listeners):** a relay is still required. Options are a custom QUIC
//!   forwarder (you implement the SFU-style selective forwarding yourself) or **Media-over-QUIC
//!   (MoQ)** pub/sub relays — conceptually the right fit, but the spec is still in draft as of
//!   early 2026.
//!
//! Unlike an SFU (relay-always), QUIC can pursue "P2P when possible, relay on fallback", but
//! that hole-punching is not as turnkey as WebRTC's ICE and would be built on top of `quinn`.
//!
//! # What is reused unchanged
//!
//! Identical to the WebRTC backend: [`tx_decision`](super::tx_decision), `voip::spatial` +
//! `spatialize`, Opus at [`OPUS_FRAME_SAMPLES`](crate::mumble::OPUS_FRAME_SAMPLES), and
//! `voip::xplane_to_mumble`. The backend only moves `(mono_pcm, position, role)` over the wire.
//!
//! # Sketch of `run`
//!
//! ```text
//! 1. Open one quinn connection (with_datagram enabled); verify the server cert via a rustls
//!    verifier seeded from server_trust.
//! 2. Open a reliable control stream; join room `session_id`, learn peers.
//! 3. For each role, subscribe to mic_tx (or radio_tx) and, per frame, consult
//!    tx_decision(role, &state); encode Opus; send as a DATAGRAM tagged with the role id and
//!    (for Voice/Radio) xplane_to_mumble(state.pos).
//! 4. On inbound DATAGRAMs: demux by role id, decode Opus → mono, render via spatialize()
//!    (or flat for IC), push to ambient_pb_tx / ic_pb_tx.
//! 5. Mirror the local radio loopback into ic_pb_tx, as the Mumble backend does.
//! ```
//!
//! Implementing this means adding `quinn` to the core crate and standing up a rendezvous /
//! relay; until then `run` returns an error.

use futures::future::BoxFuture;

use super::{TransportContext, VoipTransport};

/// QUIC transport (one connection, unreliable DATAGRAM flows). Not yet implemented.
pub struct QuicTransport;

impl VoipTransport for QuicTransport {
    fn name(&self) -> &'static str {
        "quic"
    }

    fn run(self: Box<Self>, _ctx: TransportContext) -> BoxFuture<'static, anyhow::Result<()>> {
        Box::pin(async move {
            anyhow::bail!(
                "quic transport is not yet implemented; see \
                 mumble::transport::quic module docs for the design"
            )
        })
    }
}
