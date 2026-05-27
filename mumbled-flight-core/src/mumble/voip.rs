//! Core VoIP implementation with Mumble's Official Spatial Algorithm.

use anyhow::{Result, anyhow};
use audiopus::{
    coder::{Decoder, Encoder}, packet::Packet,
    Application, Bandwidth, Bitrate, Channels, MutSignals, SampleRate,
};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
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
use std::collections::HashMap;
use std::io::Cursor;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc};
use tokio_native_tls::TlsConnector;
use tokio_util::codec::Framed;
use crate::state::CockpitState;
use log::{debug, info};

const MUMBLE_VERSION: u32 = 0x00010400;

type Control = Framed<tokio_native_tls::TlsStream<TcpStream>, ClientControlCodec>;

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

// ── Pure audio math ───────────────────────────────────────────────────────────

/// Mumble's calcGain: maps dot ∈ [-1, 1] → gain ∈ [0.25, 1.0].
fn calc_gain(dot: f32) -> f32 {
    let df = (dot + 1.0) * 0.5;
    df + (1.0 - df) * 0.25
}

/// Returns 1.0 when source and listener are on the same side of the door,
/// otherwise scales by how open it is.
fn door_attenuation(listener_z: f32, source_z: f32, door_z: f32, open: f32, open_threshold: f32) -> f32 {
    if (listener_z - door_z) * (source_z - door_z) >= 0.0 {
        return 1.0;
    }
    0.15 + 0.85 * (open / open_threshold).min(1.0)
}

fn decode_opus_packet(decoder: &mut Decoder, data: &[u8]) -> Option<Vec<f32>> {
    let packet = Packet::try_from(data).ok()?;
    let mut pcm = vec![0i16; 5760];
    let len = decoder
        .decode(Some(packet), MutSignals::try_from(pcm.as_mut_slice()).unwrap(), false)
        .ok()?;
    pcm.truncate(len);
    Some(pcm.iter().map(|&x| x as f32 / 32767.0).collect())
}

fn parse_position(pos_bytes: Option<Bytes>) -> Option<[f32; 3]> {
    let mut rdr = Cursor::new(pos_bytes?);
    Some([
        rdr.read_f32::<LittleEndian>().ok()?,
        rdr.read_f32::<LittleEndian>().ok()?,
        rdr.read_f32::<LittleEndian>().ok()?,
    ])
}

fn encode_pos(x: f32, y: f32, z: f32) -> Bytes {
    let mut buf = Vec::with_capacity(12);
    let _ = buf.write_f32::<LittleEndian>(x);
    let _ = buf.write_f32::<LittleEndian>(y);
    let _ = buf.write_f32::<LittleEndian>(z);
    Bytes::from(buf)
}

/// Computes (left_gain, right_gain) from source/listener positions and head orientation.
fn compute_stereo_gains(
    source_pos: [f32; 3],
    listener_pos: [f32; 3],
    listener_rot: [f32; 3],
    door: f32,
    door_lav: f32,
) -> (f32, f32) {
    let [lx, ly, lz] = listener_pos;
    let [sx, sy, sz] = source_pos;
    let [h_psi, h_the, h_phi] = listener_rot;

    let (dx, dy, dz) = (sx - lx, sy - ly, sz - lz);
    let dist = (dx * dx + dy * dy + dz * dz).sqrt();

    if dist < 0.01 {
        return (1.0, 1.0);
    }

    let (nx, ny, nz) = (dx / dist, dy / dist, dz / dist);
    let (hh, hp, hr) = (h_psi.to_radians(), h_the.to_radians(), h_phi.to_radians());

    let fwd = [hp.cos() * hh.sin(), hp.sin(), hp.cos() * hh.cos()];
    let top = [
        -hr.sin() * hh.cos() + hp.sin() * hr.cos() * hh.sin(),
         hp.cos() * hr.cos(),
         hr.sin() * hh.sin()  + hp.sin() * hr.cos() * hh.cos(),
    ];
    let right = [
        top[1] * fwd[2] - top[2] * fwd[1],
        top[2] * fwd[0] - top[0] * fwd[2],
        top[0] * fwd[1] - top[1] * fwd[0],
    ];

    let dot_r = nx * right[0] + ny * right[1] + nz * right[2];

    const MIN_DIST: f32 = 1.5;
    const MAX_DIST: f32 = 8.0;
    let datt = if dist <= MIN_DIST {
        1.0
    } else if dist >= MAX_DIST {
        0.0
    } else {
        let t = 1.0 - (dist - MIN_DIST) / (MAX_DIST - MIN_DIST);
        t * t
    };

    let door_att = door_attenuation(lz, sz, 4.1, door, 0.95)
                 * door_attenuation(lz, sz, -0.43, door_lav, 1.0);

    (calc_gain(-dot_r) * datt * door_att, calc_gain(dot_r) * datt * door_att)
}

// ── Session — mutable per-connection protocol state ───────────────────────────

struct Session {
    crypt:            Option<CryptState<Serverbound, Clientbound>>,
    my_session:       Option<u32>,
    channels:         HashMap<String, u32>,
    channel_joined:   bool,
    encoder:          Encoder,
    voice_seq:        u64,
    was_transmitting: bool,
    decoders:         HashMap<u32, Decoder>,
}

impl Session {
    fn new() -> Result<Self> {
        let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(64000))?;
        encoder.set_bandwidth(Bandwidth::Fullband)?;
        Ok(Self {
            crypt: None,
            my_session: None,
            channels: HashMap::new(),
            channel_joined: false,
            encoder,
            voice_seq: 0,
            was_transmitting: false,
            decoders: HashMap::new(),
        })
    }

    fn on_crypt_setup(&mut self, setup: &msgs::CryptSetup) {
        let key_raw = setup.get_key();
        if key_raw.is_empty() { return; }
        let key:     [u8; 16] = match key_raw.try_into()                  { Ok(k) => k, _ => return };
        let c_nonce: [u8; 16] = match setup.get_client_nonce().try_into() { Ok(n) => n, _ => return };
        let s_nonce: [u8; 16] = match setup.get_server_nonce().try_into() { Ok(n) => n, _ => return };
        self.crypt = Some(CryptState::new_from(key, c_nonce, s_nonce));
    }

    async fn on_channel_state(
        &mut self,
        cs: &msgs::ChannelState,
        target: &str,
        control: &mut Control,
    ) -> Result<()> {
        if !cs.has_name() || !cs.has_channel_id() { return Ok(()); }
        let name = cs.get_name().to_string();
        let cid  = cs.get_channel_id();
        self.channels.insert(name.clone(), cid);

        if self.channel_joined || name != target { return Ok(()); }
        let Some(sess) = self.my_session else { return Ok(()) };
        let mut msg = msgs::UserState::new();
        msg.set_session(sess);
        msg.set_channel_id(cid);
        control.send(ControlPacket::UserState(Box::new(msg))).await?;
        self.channel_joined = true;
        info!("[VoIP] Joined channel '{}' (id {})", name, cid);
        Ok(())
    }

    async fn on_server_sync(
        &mut self,
        sync: &msgs::ServerSync,
        client: &MumbleVoipClient,
        control: &mut Control,
    ) -> Result<()> {
        self.my_session = Some(sync.get_session());

        let mut state_msg = msgs::UserState::new();
        state_msg.set_session(sync.get_session());
        state_msg.set_plugin_context(client.context.as_bytes().to_vec());
        state_msg.set_plugin_identity(client.username.clone());
        control.send(ControlPacket::UserState(Box::new(state_msg))).await?;
        info!("[VoIP:{}] Context active: '{}'", client.username, client.context);

        if let Some(&cid) = self.channels.get(&client.target_channel) {
            let mut move_msg = msgs::UserState::new();
            move_msg.set_session(sync.get_session());
            move_msg.set_channel_id(cid);
            control.send(ControlPacket::UserState(Box::new(move_msg))).await?;
            self.channel_joined = true;
            info!("[VoIP:{}] Joined existing channel '{}'", client.username, client.target_channel);
        } else {
            info!("[VoIP:{}] Channel '{}' not found, requesting creation", client.username, client.target_channel);
            let mut ch = msgs::ChannelState::new();
            ch.set_parent(0);
            ch.set_name(client.target_channel.clone());
            ch.set_temporary(true);
            control.send(ControlPacket::ChannelState(Box::new(ch))).await?;
        }
        Ok(())
    }

    async fn on_control_msg(
        &mut self,
        msg: ControlPacket<Clientbound>,
        client: &MumbleVoipClient,
        control: &mut Control,
    ) -> Result<()> {
        match msg {
            ControlPacket::CryptSetup(s)   => self.on_crypt_setup(&s),
            ControlPacket::ChannelState(c) => self.on_channel_state(&c, &client.target_channel, control).await?,
            ControlPacket::ServerSync(s)   => self.on_server_sync(&s, client, control).await?,
            _ => {}
        }
        Ok(())
    }

    async fn on_udp_recv(
        &mut self,
        buf: &[u8],
        len: usize,
        client: &MumbleVoipClient,
        state: &Arc<Mutex<CockpitState>>,
        playback_tx: &mpsc::Sender<Vec<f32>>,
    ) {
        // Decrypt in an inner block so the crypt borrow ends before we touch decoders.
        let packet = {
            let Some(cs) = self.crypt.as_mut() else { return };
            let mut src = BytesMut::from(&buf[..len]);
            match cs.decrypt(&mut src) { Ok(Ok(p)) => p, _ => return }
        };
        let VoicePacket::Audio { session_id, payload, position_info, .. } = packet else { return };
        if self.my_session == Some(session_id) || client.is_radio { return; }
        let VoicePacketPayload::Opus(data, _) = payload else { return };

        let decoder = self.decoders.entry(session_id).or_insert_with(|| {
            debug!("[VoIP:{}] Detected remote speaker (session {})", client.username, session_id);
            Decoder::new(SampleRate::Hz48000, Channels::Mono).expect("decoder")
        });
        let Some(mono) = decode_opus_packet(decoder, &data) else { return };
        let stereo = client.spatialize(&mono, parse_position(position_info), state, session_id);
        let _ = playback_tx.send(stereo).await;
    }

    async fn on_mic_pcm(
        &mut self,
        pcm: Vec<f32>,
        client: &MumbleVoipClient,
        udp: &UdpSocket,
        state: &Arc<Mutex<CockpitState>>,
    ) -> Result<()> {
        let is_active = {
            let s = state.lock().unwrap();
            if client.is_radio   { s.spkr }
            else if client.is_ic { s.ic || s.pa }
            else                 { true }
        };
        let Some(cs) = self.crypt.as_mut() else { return Ok(()) };
        if is_active {
            client.send_audio(&pcm, &mut self.encoder, &mut self.voice_seq, udp, cs, state, false).await?;
            self.was_transmitting = true;
        } else if self.was_transmitting {
            client.send_audio(&pcm, &mut self.encoder, &mut self.voice_seq, udp, cs, state, true).await?;
            self.was_transmitting = false;
        }
        Ok(())
    }

    async fn send_tcp_ping(&mut self, control: &mut Control) {
        let mut ping = msgs::Ping::new();
        ping.set_timestamp(unix_now_ms());
        let _ = control.send(ControlPacket::Ping(Box::new(ping))).await;
    }

    fn send_udp_ping(&mut self, udp: &UdpSocket) {
        let Some(cs) = self.crypt.as_mut() else { return };
        let pkt = VoicePacket::<Serverbound>::Ping { timestamp: unix_now_ms() };
        let mut dest = BytesMut::new();
        cs.encrypt(pkt, &mut dest);
        let _ = udp.try_send(&dest);
    }
}

// ── MumbleVoipClient — static config + entry point ───────────────────────────

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
                    let Ok(pcm) = result else { break };
                    session.on_mic_pcm(pcm, self, &udp, &state).await?;
                }
            }
        }
        Ok(())
    }

    async fn connect(&self, addr: SocketAddr) -> Result<Control> {
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
        version.set_release("MumblingCockpit".to_string());
        control.send(ControlPacket::Version(Box::new(version))).await?;

        let mut auth = msgs::Authenticate::new();
        auth.set_username(self.username.clone());
        auth.set_opus(true);
        control.send(ControlPacket::Authenticate(Box::new(auth))).await?;

        Ok(control)
    }

    async fn send_audio(
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
        if self.is_ic           { None }
        else if self.is_radio   { Some(encode_pos(0.0, 0.0, 0.0)) }
        else if let Some(tp) = self.test_pos { Some(encode_pos(tp[0], tp[1], tp[2])) }
        else                    { Some(encode_pos(s.pos[0], s.pos[1], -s.pos[2])) }
    }

    fn spatialize(
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
                let msg = format!(
                    "[Spatial:{}] dist={dist:.2}m dX={:.2} dY={:.2} dZ={:.2} \
                     door={door:.2} lav={door_lav:.2} L={gl:.3} R={gr:.3}",
                    remote_sid, sx-lx, sy-ly, sz-lz,
                );
                (gl, gr, msg)
            }
        };

        static PACKET_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if PACKET_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 400 == 0 {
            debug!("{}", debug_msg);
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
