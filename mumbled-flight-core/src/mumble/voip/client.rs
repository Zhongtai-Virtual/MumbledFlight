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
use openssl::x509::{X509, X509Name};
use std::marker::PhantomData;
use std::net::SocketAddr;
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

impl MumbleVoipClient {
    pub async fn run(
        &self,
        server_addr: SocketAddr,
        state: Arc<Mutex<CockpitState>>,
        mut audio_rx: broadcast::Receiver<Vec<f32>>,
        playback_tx: mpsc::Sender<Vec<f32>>,
    ) -> Result<()> {
        info!("[VoIP:{}] Connecting to {}...", self.username, server_addr);
        let mut control = self.connect(server_addr).await?;
        *self.voip_status.lock().unwrap() = VoipClientStatus::Connected;
        let udp = UdpSocket::bind("0.0.0.0:0").await?;
        udp.connect(&server_addr).await?;

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

    pub(super) async fn connect(&self, addr: SocketAddr) -> Result<Control> {
        let tcp = TcpStream::connect(&addr).await?;
        let identity = match &self.client_cert {
            Some(cert) => cert.identity()?,
            None => self.generate_temp_identity()?,
        };
        let connector = native_tls::TlsConnector::builder()
            .identity(identity)
            .danger_accept_invalid_certs(true)
            .build()?;
        let tls = TlsConnector::from(connector)
            .connect(&addr.ip().to_string(), tcp)
            .await?;
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

    /// Builds a throwaway PKCS#12 protected by `passphrase`.
    fn make_pkcs12(passphrase: &str) -> Vec<u8> {
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
        let cert = cb.build();
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
}
