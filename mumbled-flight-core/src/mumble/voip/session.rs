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

use crate::state::CockpitState;
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
            ControlPacket::CryptSetup(s)   => self.on_crypt_setup(&s),
            ControlPacket::ChannelState(c) => self.on_channel_state(&c, &client.target_channel, control).await?,
            ControlPacket::ServerSync(s)   => self.on_server_sync(&s, client, control).await?,
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
