//! Core VoIP implementation with Studio-Grade Opus and Plugin Identity.

use anyhow::{Result, anyhow};
use audiopus::{coder::Encoder, coder::Decoder, Application, Bitrate, Channels, SampleRate, packet::Packet, MutSignals, Bandwidth};
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
    pub denoise: bool,
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
        self.perform_handshake(&mut control).await?;

        let udp_socket = UdpSocket::bind("0.0.0.0:0").await?;
        udp_socket.connect(&server_addr).await?;
        
        let mut crypt_state: Option<CryptState<Serverbound, Clientbound>> = None;
        let mut last_key: Option<[u8; 16]> = None;
        let mut my_session: Option<u32> = None;
        let mut channels: HashMap<String, u32> = HashMap::new();
        let mut moved_to_channel = false;

        // --- STUDIO-GRADE OPUS SETTINGS ---
        let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Audio)?;
        encoder.set_bitrate(Bitrate::BitsPerSecond(128000))?;
        encoder.set_bandwidth(Bandwidth::Fullband)?; // 20Hz to 20kHz
        encoder.set_complexity(10)?; // Max complexity for best quality
        
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
                    } else {
                        let _ = udp_socket.send(&[0; 12]).await;
                    }
                }

                result = udp_socket.recv_from(&mut udp_recv_buf) => {
                    if let Ok((len, _)) = result {
                        if let Some(ref mut cs) = crypt_state {
                            let mut src = BytesMut::from(&udp_recv_buf[..len]);
                            match cs.decrypt(&mut src) {
                                Ok(Ok(VoicePacket::Audio { session_id, payload, position_info, .. })) => {
                                    if self.is_radio { continue; }
                                    if let VoicePacketPayload::Opus(data, _) = payload {
                                        let decoder = decoders.entry(session_id).or_insert_with(|| {
                                            Decoder::new(SampleRate::Hz48000, Channels::Mono).expect("Failed to create decoder")
                                        });

                                        // Mumble clients can send Opus frames up to 120ms (5760 samples at 48kHz)
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

                                                let stereo_frame = self.spatialize(&mono_f32, source_pos, &state);
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
                                } else if let Some(key) = last_key {
                                    let c_nonce: [u8; 16] = setup.get_client_nonce().try_into().unwrap_or([0; 16]);
                                    let s_nonce: [u8; 16] = setup.get_server_nonce().try_into().unwrap_or([0; 16]);
                                    crypt_state = Some(CryptState::new_from(key, c_nonce, s_nonce));
                                }
                            }
                            ControlPacket::ChannelState(cs) => {
                                if cs.has_name() && cs.has_channel_id() {
                                    channels.insert(cs.get_name().to_string(), cs.get_channel_id());
                                    if !moved_to_channel && cs.get_name() == self.target_channel {
                                        if let Some(session) = my_session {
                                            let mut move_msg = msgs::UserState::new();
                                            move_msg.set_session(session);
                                            move_msg.set_channel_id(cs.get_channel_id());
                                            let _ = control.send(ControlPacket::UserState(Box::new(move_msg))).await;
                                            moved_to_channel = true;
                                        }
                                    }
                                }
                            }
                            ControlPacket::ServerSync(sync) => {
                                my_session = Some(sync.get_session());
                                let mut user_state = msgs::UserState::new();
                                user_state.set_plugin_context(self.context.as_bytes().to_vec());
                                // --- SYNCED IDENTITY METADATA ---
                                user_state.set_plugin_identity(self.username.clone());
                                control.send(ControlPacket::UserState(Box::new(user_state))).await?;
                                
                                if let Some(&cid) = channels.get(&self.target_channel) {
                                    let mut move_msg = msgs::UserState::new();
                                    move_msg.set_session(sync.get_session());
                                    move_msg.set_channel_id(cid);
                                    control.send(ControlPacket::UserState(Box::new(move_msg))).await?;
                                    moved_to_channel = true;
                                    println!("[VoIP:{}] Joined channel: {}", self.username, self.target_channel);
                                } else {
                                    let mut create_msg = msgs::ChannelState::new();
                                    create_msg.set_parent(0);
                                    create_msg.set_name(self.target_channel.clone());
                                    create_msg.set_temporary(true);
                                    control.send(ControlPacket::ChannelState(Box::new(create_msg))).await?;
                                }
                            }
                            _ => {}
                        },
                        _ => break,
                    }
                }

                result = audio_rx.recv() => {
                    match result {
                        Ok(pcm) => {
                            if let Some(ref mut cs) = crypt_state {
                                let is_active = {
                                    let s = state.lock().unwrap();
                                    if self.is_radio { s.spkr }
                                    else if self.is_ic { s.ic || s.pa } 
                                    else { true }
                                };

                                if is_active {
                                    if !was_transmitting {
                                        println!("[VoIP:{}] Start Transmission", self.username);
                                    }
                                    self.process_audio_packet(&pcm, &mut encoder, &mut voice_seq, &udp_socket, cs, &state, false).await?;
                                    was_transmitting = true;
                                } else if was_transmitting {
                                    println!("[VoIP:{}] Stop Transmission", self.username);
                                    self.process_audio_packet(&pcm, &mut encoder, &mut voice_seq, &udp_socket, cs, &state, true).await?;
                                    was_transmitting = false;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            eprintln!("[VoIP:{}] WARNING: Audio channel lagged by {} frames.", self.username, n);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        Ok(())
    }

    fn spatialize(&self, mono: &[f32], source_pos: Option<[f32; 3]>, state: &Arc<Mutex<CockpitState>>) -> Vec<f32> {
        let mut output = Vec::with_capacity(mono.len() * 2);
        let (lx, ly, lz, psi) = {
            let s = state.lock().unwrap();
            (s.pos[0], s.pos[1], s.pos[2], s.rot[0])
        };
        let (left_gain, right_gain) = if let Some([sx, sy, sz]) = source_pos {
            let dx = sx - lx; let dy = sy - ly; let dz = sz - (-lz); 
            let dist = (dx*dx + dy*dy + dz*dz).sqrt().max(0.1);
            let volume = (1.0 / dist).min(1.0);
            let head_rad = psi.to_radians();
            let angle_to_source = dx.atan2(dz);
            let relative_angle = angle_to_source - head_rad;
            let pan = relative_angle.sin();
            ((1.0 - pan).min(1.0) * volume, (1.0 + pan).min(1.0) * volume)
        } else { (1.0, 1.0) };
        for &sample in mono { output.push(sample * left_gain); output.push(sample * right_gain); }
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

    async fn perform_handshake<S>(&self, control: &mut Framed<S, ClientControlCodec>) -> Result<()> 
    where S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin 
    {
        let mut version = msgs::Version::new();
        version.set_version(MUMBLE_VERSION);
        version.set_release("MumblingCockpit".to_string());
        control.send(ControlPacket::Version(Box::new(version))).await?;
        let mut auth = msgs::Authenticate::new();
        auth.set_username(self.username.clone());
        auth.set_opus(true);
        control.send(ControlPacket::Authenticate(Box::new(auth))).await?;
        Ok(())
    }

    async fn process_audio_packet(
        &self,
        pcm: &[f32],
        encoder: &mut Encoder,
        voice_seq: &mut u64,
        udp_socket: &UdpSocket,
        crypt: &mut CryptState<Serverbound, Clientbound>,
        _state: &Arc<Mutex<CockpitState>>,
        last_bit: bool,
    ) -> Result<()> {
        // MUST CLAMP! Any value outside [-1.0, 1.0] will cause severe integer overflow/wrap-around distortion when cast to i16.
        let pcm_i16: Vec<i16> = pcm.iter().map(|&x| (x.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
        let mut opus_out = vec![0u8; 1024];
        let len = encoder.encode(&pcm_i16, &mut opus_out)?;
        opus_out.truncate(len);
        let pos_bytes = {
            let s = _state.lock().unwrap();
            if self.is_ic { 
                None 
            } else if self.is_radio {
                // Radio is fixed at aircraft CG [0, 0, 0]
                let mut buf = Vec::new();
                let _ = buf.write_f32::<LittleEndian>(0.0);
                let _ = buf.write_f32::<LittleEndian>(0.0);
                let _ = buf.write_f32::<LittleEndian>(0.0);
                Some(Bytes::from(buf))
            } else {
                let mut buf = Vec::new();
                let _ = buf.write_f32::<LittleEndian>(s.pos[0]);
                let _ = buf.write_f32::<LittleEndian>(s.pos[1]);
                let _ = buf.write_f32::<LittleEndian>(-s.pos[2]);
                Some(Bytes::from(buf))
            }
        };
        let voice_packet = VoicePacket::<Serverbound>::Audio { _dst: PhantomData, target: 0, session_id: (), seq_num: *voice_seq, payload: VoicePacketPayload::Opus(Bytes::from(opus_out), last_bit), position_info: pos_bytes };
        let mut dest = BytesMut::new();
        crypt.encrypt(voice_packet, &mut dest);
        let _ = udp_socket.send(&dest).await;
        // Mumble sequence numbers are in 10ms units.
        // We are sending 960 samples at 48kHz, which is exactly 20ms.
        *voice_seq += 2;
        Ok(())
    }
}
