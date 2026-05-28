//! Per-connection Mumble protocol state machine.

use anyhow::Result;
use audiopus::{
    coder::{Decoder, Encoder},
    Bandwidth, Bitrate, Channels, SampleRate,
};
use bytes::BytesMut;
use futures::SinkExt;
use log::{debug, info};
use mumble_protocol::control::{msgs, ClientControlCodec, ControlPacket};
use mumble_protocol::crypt::CryptState;
use mumble_protocol::voice::{Clientbound, Serverbound, VoicePacket, VoicePacketPayload};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio_native_tls::TlsStream;
use tokio_util::codec::Framed;

use crate::state::{CockpitState, SharedCockpitZone};
use super::client::MumbleVoipClient;
use super::spatial::{decode_opus_packet, parse_position};

pub type Control = Framed<TlsStream<TcpStream>, ClientControlCodec>;

pub struct Session {
    pub crypt:            Option<CryptState<Serverbound, Clientbound>>,
    pub my_session:       Option<u32>,
    pub channels:         HashMap<String, u32>,
    pub channel_joined:   bool,
    pub encoder:          Encoder,
    pub voice_seq:        u64,
    pub was_transmitting: bool,
    pub decoders:         HashMap<u32, Decoder>,
    // Zone-channel tracking for ambient clients (None on IC/radio clients).
    current_zone:         Option<SharedCockpitZone>,
    pending_channel:      Option<(String, SharedCockpitZone)>,
}

impl Session {
    pub fn new() -> Result<Self> {
        let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, audiopus::Application::Voip)?;
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
            current_zone: None,
            pending_channel: None,
        })
    }

    pub fn on_crypt_setup(&mut self, setup: &msgs::CryptSetup) {
        let key_raw = setup.get_key();
        if key_raw.is_empty() { return; }
        let key:     [u8; 16] = match key_raw.try_into()                  { Ok(k) => k, _ => return };
        let c_nonce: [u8; 16] = match setup.get_client_nonce().try_into() { Ok(n) => n, _ => return };
        let s_nonce: [u8; 16] = match setup.get_server_nonce().try_into() { Ok(n) => n, _ => return };
        self.crypt = Some(CryptState::new_from(key, c_nonce, s_nonce));
    }

    pub async fn on_channel_state(
        &mut self,
        cs: &msgs::ChannelState,
        target: &str,
        control: &mut Control,
    ) -> Result<()> {
        if !cs.has_name() || !cs.has_channel_id() { return Ok(()); }
        let name = cs.get_name().to_string();
        let cid  = cs.get_channel_id();
        self.channels.insert(name.clone(), cid);

        // Initial join on first discovery of the target channel.
        if !self.channel_joined && name == target {
            let Some(sess) = self.my_session else { return Ok(()) };
            let mut msg = msgs::UserState::new();
            msg.set_session(sess);
            msg.set_channel_id(cid);
            control.send(ControlPacket::UserState(Box::new(msg))).await?;
            self.channel_joined = true;
            info!("[VoIP] Joined channel '{}' (id {})", name, cid);
        }

        // Pending zone-channel move — channel was just created by the server.
        if let Some((ref pending_name, _)) = self.pending_channel.clone() {
            if name == *pending_name {
                self.send_channel_move(cid, control).await?;
                self.pending_channel = None;
                // current_zone is updated when the server echoes back our UserState.
            }
        }

        Ok(())
    }

    /// Sends a UserState channel-move request. Does NOT update current_zone —
    /// that only happens when the server echoes the move back via on_user_state.
    async fn send_channel_move(&mut self, cid: u32, control: &mut Control) -> Result<()> {
        let Some(sess) = self.my_session else { return Ok(()) };
        let mut msg = msgs::UserState::new();
        msg.set_session(sess);
        msg.set_channel_id(cid);
        control.send(ControlPacket::UserState(Box::new(msg))).await?;
        Ok(())
    }

    /// Server echoed our own UserState — confirm current_zone if we moved to a zone channel.
    fn on_user_state(&mut self, us: &msgs::UserState, client: &MumbleVoipClient) {
        if self.my_session != Some(us.get_session()) { return; }
        if !us.has_channel_id() { return; }
        let cid = us.get_channel_id();

        let Some((ref fbo_ch, ref aircraft_ch)) = client.zone_channels else { return };
        let confirmed = if self.channels.get(fbo_ch) == Some(&cid) {
            Some(SharedCockpitZone::InFbo)
        } else if self.channels.get(aircraft_ch) == Some(&cid) {
            Some(SharedCockpitZone::AroundOrInAircraft)
        } else {
            None
        };

        if let Some(zone) = confirmed {
            if self.current_zone != Some(zone) {
                self.current_zone = Some(zone);
                info!("[VoIP:{}] Zone channel confirmed: {:?}", client.username, zone);
            }
        }
    }

    /// Called periodically for ambient clients. Retries on every tick until the server
    /// confirms the move via UserState — no silent failure, unlimited retry.
    pub async fn check_zone_channel(
        &mut self,
        client: &MumbleVoipClient,
        state: &Arc<Mutex<CockpitState>>,
        control: &mut Control,
    ) -> Result<()> {
        let Some((ref fbo_ch, ref aircraft_ch)) = client.zone_channels else { return Ok(()) };
        let zone = state.lock().unwrap().zone;
        if self.current_zone == Some(zone) { return Ok(()); }

        let ch_name = match zone {
            SharedCockpitZone::InFbo              => fbo_ch.clone(),
            SharedCockpitZone::AroundOrInAircraft => aircraft_ch.clone(),
        };

        // Retry the move via cached channel ID on every tick.
        if let Some(&cid) = self.channels.get(&ch_name) {
            self.send_channel_move(cid, control).await?;
        }

        // Send a creation request only once per target channel to avoid spamming.
        // pending_channel is cleared either when ChannelState arrives (creation confirmed)
        // or when the zone target changes (a different ch_name is needed).
        let already_pending = self.pending_channel.as_ref().map(|(n, _)| n.as_str()) == Some(ch_name.as_str());
        if !already_pending {
            self.pending_channel = Some((ch_name.clone(), zone));
            let mut ch = msgs::ChannelState::new();
            ch.set_parent(0);
            ch.set_name(ch_name.clone());
            ch.set_temporary(true);
            control.send(ControlPacket::ChannelState(Box::new(ch))).await?;
            info!("[VoIP:{}] Requesting zone channel '{}' ({:?})", client.username, ch_name, zone);
        }
        Ok(())
    }

    fn on_channel_remove(&mut self, cr: &msgs::ChannelRemove) {
        let cid = cr.get_channel_id();
        self.channels.retain(|_, v| *v != cid);
        debug!("[VoIP] Channel {} removed", cid);
    }

    pub async fn on_server_sync(
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

    pub async fn on_control_msg(
        &mut self,
        msg: ControlPacket<Clientbound>,
        client: &MumbleVoipClient,
        control: &mut Control,
    ) -> Result<()> {
        match msg {
            ControlPacket::CryptSetup(s)       => self.on_crypt_setup(&s),
            ControlPacket::ChannelState(c)     => self.on_channel_state(&c, &client.target_channel, control).await?,
            ControlPacket::ChannelRemove(c)    => self.on_channel_remove(&c),
            ControlPacket::ServerSync(s)       => self.on_server_sync(&s, client, control).await?,
            ControlPacket::UserState(u)        => self.on_user_state(&u, client),
            // PermissionDenied on zone-channel creation means the channel already exists.
            // The move retry via cached ID will fire on the next check_zone_channel tick.
            ControlPacket::PermissionDenied(_) => {}
            _ => {}
        }
        Ok(())
    }

    pub async fn on_udp_recv(
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

    pub async fn on_mic_pcm(
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

    pub async fn send_tcp_ping(&mut self, control: &mut Control) {
        let mut ping = msgs::Ping::new();
        ping.set_timestamp(unix_now_ms());
        let _ = control.send(ControlPacket::Ping(Box::new(ping))).await;
    }

    pub fn send_udp_ping(&mut self, udp: &UdpSocket) {
        let Some(cs) = self.crypt.as_mut() else { return };
        let pkt = VoicePacket::<Serverbound>::Ping { timestamp: unix_now_ms() };
        let mut dest = BytesMut::new();
        cs.encrypt(pkt, &mut dest);
        let _ = udp.try_send(&dest);
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
