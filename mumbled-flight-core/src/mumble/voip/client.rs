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
use super::spatial::{compute_stereo_gains, encode_pos};

const MUMBLE_VERSION: u32 = 0x00010400;

pub struct MumbleVoipClient {
    pub username: String,
    pub context: String,
    pub is_ic: bool,
    pub is_radio: bool,
    pub target_channel: String,
    #[allow(dead_code)]
    pub denoise: bool,
    pub test_pos: Option<[f32; 3]>,
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
        let udp = UdpSocket::bind("0.0.0.0:0").await?;
        udp.connect(&server_addr).await?;

        let mut session  = Session::new()?;
        let mut udp_buf  = vec![0u8; 2048];
        let mut tcp_ping = tokio::time::interval(Duration::from_secs(5));
        let mut udp_ping = tokio::time::interval(Duration::from_secs(1));

        info!("[VoIP:{}] Listening for voice...", self.username);
        loop {
            tokio::select! {
                _ = tcp_ping.tick() => session.send_tcp_ping(&mut control).await,
                _ = udp_ping.tick() => session.send_udp_ping(&udp),
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
        Ok(())
    }

    pub(super) async fn connect(&self, addr: SocketAddr) -> Result<Control> {
        let tcp = TcpStream::connect(&addr).await?;
        let connector = native_tls::TlsConnector::builder()
            .identity(self.generate_temp_identity()?)
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
        auth.set_opus(true);
        control.send(ControlPacket::Authenticate(Box::new(auth))).await?;

        Ok(control)
    }

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
        if self.is_ic                        { None }
        else if self.is_radio                { Some(encode_pos(0.0, 0.0, 0.0)) }
        else if let Some(tp) = self.test_pos { Some(encode_pos(tp[0], tp[1], tp[2])) }
        else                                 { Some(encode_pos(s.pos[0], s.pos[1], -s.pos[2])) }
    }

    pub(super) fn spatialize(
        &self,
        mono: &[f32],
        source_pos: Option<[f32; 3]>,
        state: &Arc<Mutex<CockpitState>>,
        remote_sid: u32,
    ) -> Vec<f32> {
        let (lpos, lrot, door, door_lav) = {
            let s = state.lock().unwrap();
            ([s.pos[0], s.pos[1], -s.pos[2]], s.rot, s.door, s.door_lav)
        };

        let (gain_l, gain_r, debug_msg) = match source_pos {
            None => (0.5f32, 0.5f32, format!("[Spatial:{}] no pos data", remote_sid)),
            Some(spos) => {
                let (gl, gr) = compute_stereo_gains(spos, lpos, lrot, door, door_lav);
                let [lx, ly, lz] = lpos;
                let [sx, sy, sz] = spos;
                let dist = ((sx-lx).powi(2) + (sy-ly).powi(2) + (sz-lz).powi(2)).sqrt();
                (gl, gr, format!(
                    "[Spatial:{}] dist={dist:.2}m dX={:.2} dY={:.2} dZ={:.2} \
                     door={door:.2} lav={door_lav:.2} L={gl:.3} R={gr:.3}",
                    remote_sid, sx-lx, sy-ly, sz-lz,
                ))
            }
        };

        static PACKET_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if PACKET_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 400 == 0 {
            log::debug!("{}", debug_msg);
        }

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
