//! Core VoIP implementation with Mumble's Official Spatial Algorithm.

use anyhow::{Result, anyhow};
use audiopus::{coder::Encoder, coder::Decoder, packet::Packet, Application, Bitrate, Channels, SampleRate, MutSignals, Bandwidth};
use byteorder::{LittleEndian, WriteBytesExt, ReadBytesExt};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use mumble_protocol::control::{msgs, ControlPacket, ClientControlCodec};
use mumble_protocol::crypt::CryptState;
use mumble_protocol::voice::{Serverbound, Clientbound, VoicePacket, VoicePacketPayload};
use native_tls::Identity;
use openssl::asn1::Asn1Time;
use openssl::hash::MessageDigest;
use openssl::pkcs12::Pkcs12;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use openssl::x509::{X509Name, X509};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc};
use tokio_native_tls::TlsConnector;
use tokio_util::codec::Framed;
use crate::state::CockpitState;
use std::marker::PhantomData;
use std::io::Cursor;

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
        println!("[VoIP:{}] Connecting to {}...", self.username, server_addr);

        let identity = self.generate_temp_identity()?;
        let tcp_stream = TcpStream::connect(&server_addr).await?;
        let connector = native_tls::TlsConnector::builder().identity(identity).danger_accept_invalid_certs(true).build()?;
        let connector = TlsConnector::from(connector);
        let domain = server_addr.ip().to_string();
        let tls_stream = connector.connect(&domain, tcp_stream).await?;
        
        let mut control = Framed::new(tls_stream, ClientControlCodec::new());

        // Handshake
        let mut version = msgs::Version::new();
        version.set_version(MUMBLE_VERSION);
        version.set_release("MumblingCockpit".to_string());
        control.send(ControlPacket::Version(Box::new(version))).await?;

        let mut auth = msgs::Authenticate::new();
        auth.set_username(self.username.clone());
        auth.set_opus(true);
        control.send(ControlPacket::Authenticate(Box::new(auth))).await?;

        let udp_socket = UdpSocket::bind("0.0.0.0:0").await?;
        udp_socket.connect(&server_addr).await?;
        
        let mut crypt_state: Option<CryptState<Serverbound, Clientbound>> = None;
        let mut last_key: Option<[u8; 16]> = None;
        let mut my_session: Option<u32> = None;
        let mut channels: HashMap<String, u32> = HashMap::new();
        let mut moved_to_channel = false;

        let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(64000))?;
        encoder.set_bandwidth(Bandwidth::Fullband)?;
        
        let mut voice_seq = 0u64;
        let mut was_transmitting = false;

        let mut decoders: HashMap<u32, Decoder> = HashMap::new();
        let mut udp_recv_buf = vec![0u8; 2048];

        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(5));
        let mut udp_ping_interval = tokio::time::interval(std::time::Duration::from_secs(1));

        println!("[VoIP:{}] Listening for voice...", self.username);

        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    let mut ping = msgs::Ping::new();
                    ping.set_timestamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64);
                    let _ = control.send(ControlPacket::Ping(Box::new(ping))).await;
                }

                _ = udp_ping_interval.tick() => {
                    if let Some(ref mut cs) = crypt_state {
                        let ping_packet = VoicePacket::<Serverbound>::Ping { 
                            timestamp: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 
                        };
                        let mut dest = BytesMut::new();
                        cs.encrypt(ping_packet, &mut dest);
                        let _ = udp_socket.send(&dest).await;
                    }
                }

                result = udp_socket.recv_from(&mut udp_recv_buf) => {
                    if let Ok((len, _)) = result {
                        if let Some(ref mut cs) = crypt_state {
                            let mut src = BytesMut::from(&udp_recv_buf[..len]);
                            match cs.decrypt(&mut src) {
                                Ok(Ok(VoicePacket::Audio { session_id, payload, position_info, .. })) => {
                                    if let Some(me) = my_session { if session_id == me { continue; } }
                                    if self.is_radio { continue; }

                                    if let VoicePacketPayload::Opus(data, _) = payload {
                                        let decoder = decoders.entry(session_id).or_insert_with(|| {
                                            println!("[VoIP:{}] Detected remote speaker (Session: {})", self.username, session_id);
                                            Decoder::new(SampleRate::Hz48000, Channels::Mono).expect("Failed to create decoder")
                                        });

                                        let mut output_pcm = vec![0i16; 5760];
                                        if let Ok(packet) = Packet::try_from(&data[..]) {
                                            if let Ok(len) = decoder.decode(Some(packet), MutSignals::try_from(&mut output_pcm[..]).unwrap(), false) {
                                                output_pcm.truncate(len);
                                                let mono_f32: Vec<f32> = output_pcm.iter().map(|&x| x as f32 / 32767.0).collect();
                                                
                                                let source_pos = if let Some(pos_bytes) = position_info {
                                                    let mut rdr = Cursor::new(pos_bytes);
                                                    if let (Ok(x), Ok(y), Ok(z)) = (rdr.read_f32::<LittleEndian>(), rdr.read_f32::<LittleEndian>(), rdr.read_f32::<LittleEndian>()) {
                                                        Some([x, y, z])
                                                    } else { None }
                                                } else { None };

                                                let stereo_frame = self.spatialize(&mono_f32, source_pos, &state, session_id);
                                                let _ = playback_tx.send(stereo_frame).await;
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                result = control.next() => {
                    match result {
                        Some(Ok(msg)) => match msg {
                            ControlPacket::CryptSetup(setup) => {
                                let key_raw = setup.get_key();
                                if !key_raw.is_empty() {
                                    let key: [u8; 16] = key_raw.try_into().unwrap();
                                    let c_nonce: [u8; 16] = setup.get_client_nonce().try_into().unwrap();
                                    let s_nonce: [u8; 16] = setup.get_server_nonce().try_into().unwrap();
                                    last_key = Some(key);
                                    crypt_state = Some(CryptState::new_from(key, c_nonce, s_nonce));
                                }
                            }
                            ControlPacket::ChannelState(cs) => {
                                if cs.has_name() && cs.has_channel_id() {
                                    channels.insert(cs.get_name().to_string(), cs.get_channel_id());
                                }
                            }
                            ControlPacket::ServerSync(sync) => {
                                my_session = Some(sync.get_session());
                                let mut user_state = msgs::UserState::new();
                                user_state.set_session(sync.get_session());
                                user_state.set_plugin_context(self.context.as_bytes().to_vec());
                                user_state.set_plugin_identity(self.username.clone());
                                println!("[VoIP:{}] Context active: '{}'", self.username, self.context);
                                control.send(ControlPacket::UserState(Box::new(user_state))).await?;
                                
                                if let Some(&cid) = channels.get(&self.target_channel) {
                                    let mut move_msg = msgs::UserState::new();
                                    move_msg.set_session(sync.get_session());
                                    move_msg.set_channel_id(cid);
                                    control.send(ControlPacket::UserState(Box::new(move_msg))).await?;
                                    moved_to_channel = true;
                                }
                            }
                            _ => {}
                        },
                        _ => break,
                    }
                }

                pcm_result = audio_rx.recv() => {
                    match pcm_result {
                        Ok(pcm) => {
                            if let Some(ref mut cs) = crypt_state {
                                let is_active = {
                                    let s = state.lock().unwrap();
                                    if self.is_radio { s.spkr }
                                    else if self.is_ic { s.ic || s.pa } 
                                    else { true }
                                };

                                if is_active {
                                    self.process_audio_packet(&pcm, &mut encoder, &mut voice_seq, &udp_socket, cs, &state, false).await?;
                                    was_transmitting = true;
                                } else if was_transmitting {
                                    self.process_audio_packet(&pcm, &mut encoder, &mut voice_seq, &udp_socket, cs, &state, true).await?;
                                    was_transmitting = false;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        Ok(())
    }

    async fn process_audio_packet(&self, pcm: &[f32], encoder: &mut Encoder, voice_seq: &mut u64, udp_socket: &UdpSocket, crypt: &mut CryptState<Serverbound, Clientbound>, _state: &Arc<Mutex<CockpitState>>, last_bit: bool) -> Result<()> {
        let pcm_i16: Vec<i16> = pcm.iter().map(|&x| (x.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
        let mut opus_out = vec![0u8; 1024];
        let len = encoder.encode(&pcm_i16, &mut opus_out)?;
        opus_out.truncate(len);

        let pos_bytes = {
            let s = _state.lock().unwrap();
            if self.is_ic { 
                None 
            } else if let Some(tp) = self.test_pos {
                let mut buf = Vec::new();
                let _ = buf.write_f32::<LittleEndian>(tp[0]);
                let _ = buf.write_f32::<LittleEndian>(tp[1]);
                let _ = buf.write_f32::<LittleEndian>(tp[2]);
                Some(Bytes::from(buf))
            } else if self.is_radio {
                let mut buf = Vec::new();
                for _ in 0..3 { let _ = buf.write_f32::<LittleEndian>(0.0); }
                Some(Bytes::from(buf))
            } else {
                let mut buf = Vec::new();
                // Mumble Space: +X Right, +Y Up, +Z Forward.
                // X-Plane Space: +X Right, +Y Up, -Z Forward.
                let x = s.pos[0]; let y = s.pos[1]; let z = -s.pos[2];
                let _ = buf.write_f32::<LittleEndian>(x);
                let _ = buf.write_f32::<LittleEndian>(y);
                let _ = buf.write_f32::<LittleEndian>(z);
                Some(Bytes::from(buf))
            }
        };

        let voice_packet = VoicePacket::<Serverbound>::Audio { _dst: PhantomData, target: 0, session_id: (), seq_num: *voice_seq, payload: VoicePacketPayload::Opus(Bytes::from(opus_out), last_bit), position_info: pos_bytes };
        let mut dest = BytesMut::new();
        crypt.encrypt(voice_packet, &mut dest);
        let _ = udp_socket.send(&dest).await;
        *voice_seq += 2;
        Ok(())
    }

    fn spatialize(&self, mono: &[f32], source_pos: Option<[f32; 3]>, state: &Arc<Mutex<CockpitState>>, remote_sid: u32) -> Vec<f32> {
        // Read listener state: pos is already in Mumble space (+Z forward, -pilots_head_z)
        let (lx, ly, lz, h_psi, h_the, h_phi, door, door_lav) = {
            let s = state.lock().unwrap();
            (s.pos[0], s.pos[1], -s.pos[2], s.rot[0], s.rot[1], s.rot[2], s.door, s.door_lav)
        };

        // Default: centered mono. Only changes when position data arrives.
        let mut gains = (0.5f32, 0.5f32);
        let mut debug_line = format!("[Spatial:{}] no pos data", remote_sid);

        if let Some([sx, sy, sz]) = source_pos {
            let dx = sx - lx;
            let dy = sy - ly;
            let dz = sz - lz;
            let dist = (dx*dx + dy*dy + dz*dz).sqrt();

            if dist < 0.01 {
                // Source and listener at the same point — full volume, centered.
                gains = (1.0, 1.0);
            } else {
                // Normalized direction vector from listener to source (Mumble aircraft-local space).
                let nx = dx / dist;
                let ny = dy / dist;
                let nz = dz / dist;

                // pilots_head_psi/the/phi are relative to the aircraft body axes (0 = forward),
                // the same frame as pilots_head_x/y/z positions.  Using h_psi directly keeps
                // the orientation vectors in aircraft-local space — consistent with [nx,ny,nz].
                // (Subtracting plane_psi would rotate the basis into world space, making the
                // dot product with aircraft-local positions meaningless.)
                let hh = h_psi.to_radians();
                let hp = h_the.to_radians();
                let hr = h_phi.to_radians();

                // Build listener orientation vectors in Mumble/aircraft-local space.
                // These are the same formulas used by the X-Plane plugin to populate
                // the Mumble Link shared memory (fCameraFront / fCameraTop).
                let fwd = [
                    hp.cos() * hh.sin(),   // x
                    hp.sin(),               // y
                    hp.cos() * hh.cos(),   // z  (+Z = aircraft-forward in Mumble space)
                ];
                let top = [
                    -hr.sin() * hh.cos() + hp.sin() * hr.cos() * hh.sin(),
                    hp.cos() * hr.cos(),
                    hr.sin() * hh.sin()  + hp.sin() * hr.cos() * hh.cos(),
                ];

                // right = top × fwd   (matches Mumble: right = cameraAxis.crossProduct(cameraDir))
                let right = [
                    top[1] * fwd[2] - top[2] * fwd[1],
                    top[2] * fwd[0] - top[0] * fwd[2],
                    top[0] * fwd[1] - top[1] * fwd[0],
                ];

                // In headphone mode Mumble places speakers at (±1, 0, 0) in listener-local
                // space, which after head-orientation rotation become ±right in world/cockpit
                // space.  dot_r = how much the source direction aligns with the right ear.
                let dot_r =  nx * right[0] + ny * right[1] + nz * right[2];
                let dot_l = -dot_r;

                // Mumble's calcGain: maps dot ∈ [-1, 1] → gain ∈ [0.25, 1.0].
                // Derived from AudioOutput.cpp::calcGain() for the near-field case
                // (distance < fAudioMinDistance, bloom = 0):
                //   dotfactor = (dot + 1) / 2
                //   gain = dotfactor + (1 - dotfactor) * 0.25   = 0.75*dotfactor + 0.25
                let calc_gain = |dot: f32| -> f32 {
                    let df = (dot + 1.0) * 0.5;
                    df + (1.0 - df) * 0.25
                };

                // Distance attenuation: full volume up to 1.5 m, fades to zero at 8 m.
                // Power-law curve mirrors Mumble's log-distance model at cockpit scale.
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

                // Door attenuation: each door blocks sound when source and listener are
                // on opposite sides of it and the door is not fully open.
                // Mumble z = -pilots_head_z, so cabin door z=4.1, lavatory door z=-0.43.
                let door_att = {
                    // Cabin door: pilots_head_z=-4.1 → Mumble z=4.1
                    // 0=closed→0.15, 0.95=panel removed→1.0, 1.0=stored→1.0
                    const CABIN_Z: f32 = 4.1;
                    let cabin_att = if (lz - CABIN_Z) * (sz - CABIN_Z) < 0.0 {
                        let t = (door / 0.95).min(1.0);
                        0.15 + 0.85 * t
                    } else {
                        1.0
                    };
                    // Lavatory door: pilots_head_z=0.43 → Mumble z=-0.43
                    // 0=closed→0.15, 1=open→1.0
                    const LAV_Z: f32 = -0.43;
                    let lav_att = if (lz - LAV_Z) * (sz - LAV_Z) < 0.0 {
                        0.15 + 0.85 * door_lav
                    } else {
                        1.0
                    };
                    cabin_att * lav_att
                };

                gains = (calc_gain(dot_l) * datt * door_att, calc_gain(dot_r) * datt * door_att);
                debug_line = format!(
                    "[Spatial:{}] dist={:.2}m dX={:.2} dY={:.2} dZ={:.2} headPsi={:.1}° dot_R={:.3} door={:.2} lav={:.2} L={:.3} R={:.3}",
                    remote_sid, dist, dx, dy, dz, h_psi, dot_r, door, door_lav, gains.0, gains.1
                );
            }
        }

        static PACKET_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if PACKET_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 400 == 0 {
            println!("{}", debug_line);
        }

        let mut output = Vec::with_capacity(mono.len() * 2);
        for &s in mono {
            output.push(s * gains.0);
            output.push(s * gains.1);
        }
        output
    }

    fn generate_temp_identity(&self) -> Result<Identity> {
        let rsa = Rsa::generate(2048)?;
        let pkey = PKey::from_rsa(rsa)?;
        let mut name_builder = X509Name::builder()?;
        name_builder.append_entry_by_text("CN", &self.username)?;
        let x509_name = name_builder.build();
        let mut cert_builder = X509::builder()?;
        cert_builder.set_version(2)?;
        cert_builder.set_subject_name(&x509_name)?;
        cert_builder.set_issuer_name(&x509_name)?;
        cert_builder.set_pubkey(&pkey)?;
        let now = Asn1Time::days_from_now(0)?;
        let later = Asn1Time::days_from_now(365)?;
        cert_builder.set_not_before(&now)?;
        cert_builder.set_not_after(&later)?;
        cert_builder.sign(&pkey, MessageDigest::sha256())?;
        let cert = cert_builder.build();
        let p12 = Pkcs12::builder().build("", &self.username, &pkey, &cert)?;
        let der = p12.to_der()?;
        Identity::from_pkcs12(&der, "").map_err(|e| anyhow!("Identity error: {}", e))
    }
}
