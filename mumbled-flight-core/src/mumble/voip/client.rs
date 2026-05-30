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


//! MumbleVoipClient — static config, connection, and audio processing.

use anyhow::{anyhow, Result};
use audiopus::coder::Encoder;
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use log::info;
use mumble_protocol::control::{msgs, ClientControlCodec, ControlPacket};
use mumble_protocol::crypt::CryptState;
use mumble_protocol::voice::{Clientbound, Serverbound, VoicePacket, VoicePacketPayload};
use native_tls::Identity;
use openssl::asn1::Asn1Time;
use openssl::hash::MessageDigest;
use openssl::pkcs12::Pkcs12;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use openssl::stack::Stack;
use openssl::x509::store::X509StoreBuilder;
use openssl::x509::{X509, X509Name, X509StoreContext};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc};
use tokio_native_tls::TlsConnector;
use tokio_util::codec::Framed;

use crate::state::CockpitState;
use super::session::{Control, Session};
use super::spatial::{aircraft_skin_gains, compute_stereo_gains, encode_pos, is_inside_aircraft, pa_gain, xplane_to_mumble};

const MUMBLE_VERSION: u32 = 0x00010400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoipClientStatus {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientRole {
    Voice,
    Ic,
    Pa,
    /// `has_source` — false when the radio source device is "Disabled" (no loopback capture).
    Radio { has_source: bool },
}

pub struct MumbleVoipClient {
    pub username: String,
    pub context: String,
    pub role: ClientRole,
    pub voip_status: Arc<Mutex<VoipClientStatus>>,
    pub target_channel: String,
    /// Voice client only: (fbo_channel_name, aircraft_channel_name) for zone switching.
    /// None for IC, PA, and radio clients — they never switch channels.
    pub zone_channels: Option<(String, String)>,
    pub test_pos: Option<[f32; 3]>,
    /// Mumble server password (empty = none). Sent in the Authenticate message on connect.
    pub password: String,
    /// Optional user-supplied client certificate used as the TLS identity. When `None`, a
    /// throwaway self-signed identity is generated per connection. Shared by all four clients.
    pub client_cert: Option<Arc<ClientCert>>,
    /// Optional trust anchor(s) used to verify the server's certificate. When `None`, the
    /// server certificate is **not** verified (the connection is unauthenticated).
    pub server_trust: Option<Arc<ServerTrust>>,
    /// Stereo width for spatialized audio: 0.0 = mono, 1.0 = full spatial. Live-adjustable.
    pub spatial_width: Arc<std::sync::atomic::AtomicU32>,
}

/// A user-supplied client certificate (PKCS#12 / `.p12`) used as the Mumble TLS identity —
/// the native, strong alternative to password auth. Optionally passphrase-protected.
pub struct ClientCert {
    pkcs12: Vec<u8>,
    passphrase: String,
}

impl ClientCert {
    /// Reads a PKCS#12 file and eagerly validates it opens with `passphrase`, so a wrong
    /// path/passphrase fails fast with a clear error instead of four silent connect failures.
    pub fn load(path: &std::path::Path, passphrase: &str) -> Result<Self> {
        let pkcs12 = std::fs::read(path)
            .map_err(|e| anyhow!("reading client certificate '{}': {e}", path.display()))?;
        Identity::from_pkcs12(&pkcs12, passphrase)
            .map_err(|e| anyhow!("invalid client certificate or passphrase: {e}"))?;
        Ok(Self { pkcs12, passphrase: passphrase.to_string() })
    }

    fn identity(&self) -> Result<Identity> {
        Identity::from_pkcs12(&self.pkcs12, &self.passphrase)
            .map_err(|e| anyhow!("client certificate: {e}"))
    }
}

/// Trust anchors for verifying the Mumble **server's** certificate: either the server's own
/// certificate (exact pinning) or the root/intermediate CA(s) that issued it. Provide a PEM
/// (one cert or a bundle) or a single DER cert.
///
/// `native-tls` cannot drop the system trust store, so it can't express "trust ONLY this
/// cert/CA". We therefore complete the handshake and verify the server's presented certificate
/// against *only* these anchors with openssl — a self-signed anchor pins exactly that cert; a
/// CA anchor accepts any leaf that chains to it.
pub struct ServerTrust {
    anchors: Vec<u8>,
}

impl ServerTrust {
    /// Reads and validates a PEM/DER certificate file containing the server cert or its CA(s).
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let anchors = std::fs::read(path)
            .map_err(|e| anyhow!("reading server CA/cert '{}': {e}", path.display()))?;
        if Self::parse(&anchors)?.is_empty() {
            return Err(anyhow!("no certificates found in '{}'", path.display()));
        }
        Ok(Self { anchors })
    }

    /// Parse the anchor bytes as a PEM bundle (root + intermediates) or a single DER cert.
    fn parse(bytes: &[u8]) -> Result<Vec<X509>> {
        if let Ok(stack) = X509::stack_from_pem(bytes) {
            if !stack.is_empty() {
                return Ok(stack);
            }
        }
        let der = X509::from_der(bytes)
            .map_err(|e| anyhow!("not a PEM or DER certificate: {e}"))?;
        Ok(vec![der])
    }

    /// Verifies the server's leaf certificate (DER) against the trusted anchors.
    fn verify(&self, peer_der: &[u8]) -> Result<()> {
        let leaf = X509::from_der(peer_der)
            .map_err(|e| anyhow!("parsing server certificate: {e}"))?;
        let mut builder = X509StoreBuilder::new()?;
        for anchor in Self::parse(&self.anchors)? {
            builder.add_cert(anchor)?;
        }
        let store = builder.build();
        let chain = Stack::new()?;
        let mut ctx = X509StoreContext::new()?;
        let mut reason = None;
        let verified = ctx.init(&store, &leaf, &chain, |c| {
            let ok = c.verify_cert()?;
            if !ok {
                reason = Some(c.error());
            }
            Ok(ok)
        })?;
        if verified {
            Ok(())
        } else {
            Err(anyhow!(
                "server certificate not trusted by the provided CA/cert: {}",
                reason.map(|r| r.to_string()).unwrap_or_default()
            ))
        }
    }
}

impl MumbleVoipClient {
    pub async fn run(
        &self,
        host: &str,
        port: u16,
        state: Arc<Mutex<CockpitState>>,
        mut audio_rx: broadcast::Receiver<Vec<f32>>,
        playback_tx: mpsc::Sender<Vec<f32>>,
    ) -> Result<()> {
        info!("[VoIP:{}] Connecting to {}:{}...", self.username, host, port);
        let mut control = self.connect(host, port).await?;
        *self.voip_status.lock().unwrap() = VoipClientStatus::Connected;
        let udp = UdpSocket::bind("0.0.0.0:0").await?;
        udp.connect((host, port)).await?;

        let mut session    = Session::new()?;
        let mut udp_buf    = vec![0u8; 2048];
        let mut tcp_ping   = tokio::time::interval(Duration::from_secs(5));
        let mut udp_ping   = tokio::time::interval(Duration::from_secs(1));
        let mut zone_check = tokio::time::interval(Duration::from_secs(5));

        info!("[VoIP:{}] Listening for voice...", self.username);
        loop {
            tokio::select! {
                _ = tcp_ping.tick()   => session.send_tcp_ping(&mut control).await,
                _ = udp_ping.tick()   => session.send_udp_ping(&udp),
                _ = zone_check.tick() => session.check_zone_channel(self, &state, &mut control).await?,
                result = udp.recv_from(&mut udp_buf) => {
                    let Ok((len, _)) = result else { continue };
                    session.on_udp_recv(&udp_buf, len, self, &state, &playback_tx).await;
                }
                result = control.next() => {
                    let Some(Ok(msg)) = result else { break };
                    session.on_control_msg(msg, self, &mut control).await?;
                }
                result = audio_rx.recv() => {
                    match result {
                        Ok(pcm) => session.on_mic_pcm(pcm, self, &udp, &state).await?,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed)    => break,
                    }
                }
            }
        }
        *self.voip_status.lock().unwrap() = VoipClientStatus::Disconnected;
        Ok(())
    }

    pub(super) async fn connect(&self, host: &str, port: u16) -> Result<Control> {
        let tcp = TcpStream::connect((host, port)).await?;
        let identity = match &self.client_cert {
            Some(cert) => cert.identity()?,
            None => self.generate_temp_identity()?,
        };
        // Complete the handshake unconditionally; if trust anchors are configured we verify the
        // server's certificate against them ourselves below (native-tls can't restrict trust to
        // only a user-supplied cert/CA). Hostname checks are skipped since Mumble servers are
        // typically reached by IP with a self-signed cert that has no matching SAN.
        let connector = native_tls::TlsConnector::builder()
            .identity(identity)
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()?;
        let tls = TlsConnector::from(connector)
            .connect(host, tcp)
            .await?;
        if let Some(trust) = &self.server_trust {
            let peer = tls
                .get_ref()
                .peer_certificate()
                .map_err(|e| anyhow!("reading server certificate: {e}"))?
                .ok_or_else(|| anyhow!("server presented no certificate"))?;
            let der = peer.to_der().map_err(|e| anyhow!("encoding server certificate: {e}"))?;
            trust.verify(&der)?;
            info!("[VoIP:{}] server certificate verified", self.username);
        }
        let mut control = Framed::new(tls, ClientControlCodec::new());

        let mut version = msgs::Version::new();
        version.set_version(MUMBLE_VERSION);
        version.set_release("MumbledFlight".to_string());
        control.send(ControlPacket::Version(Box::new(version))).await?;

        let mut auth = msgs::Authenticate::new();
        auth.set_username(self.username.clone());
        if !self.password.is_empty() {
            auth.set_password(self.password.clone());
        }
        auth.set_opus(true);
        control.send(ControlPacket::Authenticate(Box::new(auth))).await?;

        Ok(control)
    }

    // Encoder/seq/crypt are borrowed from the caller's Session; grouping them would just move
    // the plumbing around, so the argument list is intentionally wide here.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send_audio(
        &self,
        pcm: &[f32],
        encoder: &mut Encoder,
        voice_seq: &mut u64,
        udp: &UdpSocket,
        crypt: &mut CryptState<Serverbound, Clientbound>,
        state: &Arc<Mutex<CockpitState>>,
        end: bool,
    ) -> Result<()> {
        let pcm_i16: Vec<i16> = pcm.iter().map(|&x| (x.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
        let mut opus_out = vec![0u8; 1024];
        let len = encoder.encode(&pcm_i16, &mut opus_out)?;
        opus_out.truncate(len);

        let pos_bytes = self.position_bytes(&state.lock().unwrap());
        let pkt = VoicePacket::<Serverbound>::Audio {
            _dst: PhantomData,
            target: 0,
            session_id: (),
            seq_num: *voice_seq,
            payload: VoicePacketPayload::Opus(Bytes::from(opus_out), end),
            position_info: pos_bytes,
        };
        let mut dest = BytesMut::new();
        crypt.encrypt(pkt, &mut dest);
        let _ = udp.send(&dest).await;
        *voice_seq += 2;
        Ok(())
    }

    fn position_bytes(&self, s: &CockpitState) -> Option<Bytes> {
        match self.role {
            ClientRole::Ic | ClientRole::Pa => None,
            _ => if let Some(tp) = self.test_pos {
                Some(encode_pos(tp[0], tp[1], tp[2]))
            } else {
                let [x, y, z] = xplane_to_mumble(s.pos);
                Some(encode_pos(x, y, z))
            },
        }
    }

    pub(super) fn spatialize(
        &self,
        mono: &[f32],
        source_pos: Option<[f32; 3]>,
        state: &Arc<Mutex<CockpitState>>,
        remote_sid: u32,
    ) -> Vec<f32> {
        let (lpos, lrot, door, door_lav, door_main) = {
            let s = state.lock().unwrap();
            (xplane_to_mumble(s.pos), s.rot, s.door, s.door_lav, s.door_main)
        };

        let (gain_l, gain_r, debug_msg) = match source_pos {
            None => {
                // PA: omnidirectional, equal-power flat stereo. pa_gain handles the cabin /
                // through-the-door / outside-the-hull attenuation (see spatial::pa_gain).
                let g = pa_gain(lpos, door, door_main);
                let outside = !is_inside_aircraft(lpos);
                (g, g, format!(
                    "[Spatial:PA:{}] outside={outside} door={door:.2} door_main={door_main:.2} \
                     dist_from_cabin_door={:.2} gain={g:.3}",
                    remote_sid, (lpos[2] - 4.1).max(0.0),
                ))
            }
            Some(spos) => {
                let listener_inside = is_inside_aircraft(lpos);
                let source_inside   = is_inside_aircraft(spos);
                let (gl, gr) = if listener_inside != source_inside {
                    aircraft_skin_gains(spos, lpos, lrot, door, door_lav, door_main)
                } else {
                    compute_stereo_gains(spos, lpos, lrot, door, door_lav)
                };
                let [lx, ly, lz] = lpos;
                let [sx, sy, sz] = spos;
                let dist = ((sx-lx).powi(2) + (sy-ly).powi(2) + (sz-lz).powi(2)).sqrt();
                let skin = listener_inside != source_inside;
                (gl, gr, format!(
                    "[Spatial:{}] dist={dist:.2}m dX={:.2} dY={:.2} dZ={:.2} \
                     door={door:.2} lav={door_lav:.2} door_main={door_main:.2} skin={skin} L={gl:.3} R={gr:.3}",
                    remote_sid, sx-lx, sy-ly, sz-lz,
                ))
            }
        };

        static PACKET_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if PACKET_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed).is_multiple_of(400) {
            log::debug!("{}", debug_msg);
        }

        let width = f32::from_bits(self.spatial_width.load(std::sync::atomic::Ordering::Relaxed));
        let (gain_l, gain_r) = if (width - 1.0).abs() > 1e-4 {
            let mid = (gain_l + gain_r) * 0.5;
            (
                (mid + (gain_l - mid) * width).max(0.0),
                (mid + (gain_r - mid) * width).max(0.0),
            )
        } else {
            (gain_l, gain_r)
        };

        let mut out = Vec::with_capacity(mono.len() * 2);
        for &s in mono { out.push(s * gain_l); out.push(s * gain_r); }
        out
    }

    fn generate_temp_identity(&self) -> Result<Identity> {
        let rsa  = Rsa::generate(2048)?;
        let pkey = PKey::from_rsa(rsa)?;
        let mut name_builder = X509Name::builder()?;
        name_builder.append_entry_by_text("CN", &self.username)?;
        let x509_name = name_builder.build();
        let mut cert_builder = X509::builder()?;
        cert_builder.set_version(2)?;
        cert_builder.set_subject_name(&x509_name)?;
        cert_builder.set_issuer_name(&x509_name)?;
        cert_builder.set_pubkey(&pkey)?;
        let not_before = Asn1Time::days_from_now(0)?;
        let not_after  = Asn1Time::days_from_now(365)?;
        cert_builder.set_not_before(&not_before)?;
        cert_builder.set_not_after(&not_after)?;
        cert_builder.sign(&pkey, MessageDigest::sha256())?;
        let cert = cert_builder.build();
        let p12  = Pkcs12::builder().name(&self.username).pkey(&pkey).cert(&cert).build2("")?;
        Identity::from_pkcs12(&p12.to_der()?, "").map_err(|e| anyhow!("Identity error: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a throwaway self-signed certificate + its key.
    fn gen_cert() -> (X509, PKey<openssl::pkey::Private>) {
        let rsa = Rsa::generate(2048).unwrap();
        let pkey = PKey::from_rsa(rsa).unwrap();
        let mut nb = X509Name::builder().unwrap();
        nb.append_entry_by_text("CN", "test").unwrap();
        let name = nb.build();
        let mut cb = X509::builder().unwrap();
        cb.set_version(2).unwrap();
        cb.set_subject_name(&name).unwrap();
        cb.set_issuer_name(&name).unwrap();
        cb.set_pubkey(&pkey).unwrap();
        cb.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
        cb.set_not_after(&Asn1Time::days_from_now(1).unwrap()).unwrap();
        cb.sign(&pkey, MessageDigest::sha256()).unwrap();
        (cb.build(), pkey)
    }

    /// Builds a throwaway PKCS#12 protected by `passphrase`.
    fn make_pkcs12(passphrase: &str) -> Vec<u8> {
        let (cert, pkey) = gen_cert();
        Pkcs12::builder().name("test").pkey(&pkey).cert(&cert).build2(passphrase).unwrap()
            .to_der().unwrap()
    }

    #[test]
    fn loads_valid_pkcs12_and_rejects_wrong_passphrase() {
        let path = std::env::temp_dir().join("mumbledflight_clientcert_test.p12");
        std::fs::write(&path, make_pkcs12("secret")).unwrap();

        assert!(ClientCert::load(&path, "secret").is_ok());
        assert!(ClientCert::load(&path, "wrong").is_err(), "wrong passphrase must fail");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_reports_clear_error() {
        // `.err().unwrap()` rather than `.unwrap_err()` so ClientCert needn't be Debug.
        let err = ClientCert::load(std::path::Path::new("/no/such/mf_cert.p12"), "")
            .err()
            .expect("missing file must error");
        assert!(err.to_string().contains("reading client certificate"));
    }

    #[test]
    fn server_trust_pins_exact_cert_and_rejects_others() {
        let (cert, _) = gen_cert();
        let trust = ServerTrust { anchors: cert.to_pem().unwrap() };
        // The server presenting exactly the pinned cert verifies.
        assert!(trust.verify(&cert.to_der().unwrap()).is_ok(), "pinned cert must verify");
        // A different (untrusted) cert is rejected.
        let (other, _) = gen_cert();
        assert!(trust.verify(&other.to_der().unwrap()).is_err(), "unknown cert must be rejected");
    }
}
