//! Orchestrates the Mumble VoIP stack with multi-source Radio Relay.

pub mod audio;
pub mod voip;

use self::audio::{create_linux_sink, start_capture, start_loopback_capture, start_playback};
use self::voip::client::{ClientRole, MumbleVoipClient, VoipClientStatus};
use crate::state::{CockpitState, SharedCockpitZone};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{broadcast, mpsc};

pub use self::voip::client::VoipClientStatus as ClientStatus;

/// Maps a display label (e.g. "Voice", "IC") to that client's live connection status.
pub type VoipStatuses = Arc<Mutex<HashMap<String, Arc<Mutex<VoipClientStatus>>>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TestClient {
    #[default]
    All,
    Voice,
    Ic,
    Pa,
    Radio,
}

pub async fn run_mumble_stack(
    state: Arc<Mutex<CockpitState>>,
    user_name: String,
    session_id: String,
    mic_gain: Arc<AtomicU32>,
    denoise: bool,
    radio_source: Option<String>,
    auto_sink: bool,
    test_client: TestClient,
    sine_input: bool,
    test_pos: Option<[f32; 3]>,
    server_addr: SocketAddr,
    ambient_output: Option<String>,
    ic_output: Option<String>,
    statuses: VoipStatuses,
) {
    // 1. MIC Chain
    let (mic_tx, _) = broadcast::channel::<Vec<f32>>(128);
    let mic_tx_clone = mic_tx.clone();
    let d_mic = denoise;
    std::thread::spawn(move || {
        let (sync_tx, mut sync_rx) = mpsc::channel(128);
        if sine_input {
            audio::start_sine_capture(sync_tx, mic_gain);
        } else {
            start_capture(sync_tx, d_mic, mic_gain, 0.0, None);
        }
        while let Some(frame) = sync_rx.blocking_recv() {
            let _ = mic_tx_clone.send(frame);
        }
    });

    // Wait for the primary mic capture to fully establish itself in the OS mixer
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 2. RADIO Chain
    let final_radio_source = if auto_sink {
        create_linux_sink()
    } else {
        radio_source
    };

    let radio_tx = if matches!(test_client, TestClient::All | TestClient::Radio)
        && final_radio_source.is_some()
    {
        Some(radio_loopback_sender(final_radio_source.unwrap()))
    } else {
        None
    };

    // 3. Playback Mixers — ambient and IC each route to their own output device.
    //    Radio received audio shares the ambient mixer (no separate radio output).
    let (ambient_pb_tx, ambient_pb_rx) = mpsc::channel(1024);
    let (ic_pb_tx, ic_pb_rx) = mpsc::channel(1024);
    let ic_monitor_tx = ic_pb_tx.clone();
    start_playback(ambient_pb_rx, ambient_output);
    start_playback(ic_pb_rx, ic_output);

    // Helper: allocate a status slot and register it in the shared map.
    let mk_status = |label: &str| -> Arc<Mutex<VoipClientStatus>> {
        let slot = Arc::new(Mutex::new(VoipClientStatus::Connecting));
        statuses
            .lock()
            .unwrap()
            .insert(label.to_string(), Arc::clone(&slot));
        slot
    };

    // 4. Voice Client (natural speech, spatialised)
    if matches!(test_client, TestClient::All | TestClient::Voice) {
        let fbo_ch = format!("{}_ambient_fbo", session_id);
        let aircraft_ch = format!("{}_ambient_aircraft", session_id);
        let initial_zone = state.lock().unwrap().zone;
        let initial_ambient_ch = match initial_zone {
            SharedCockpitZone::InFbo => fbo_ch.clone(),
            SharedCockpitZone::AroundOrInAircraft => aircraft_ch.clone(),
        };
        let st_a = Arc::clone(&state);
        let mic_rx_a = mic_tx.subscribe();
        let pb_tx_a = ambient_pb_tx.clone();
        let un_a = user_name.clone();
        let sid_a = session_id.clone();
        let status_a = mk_status("Voice");
        tokio::spawn(async move {
            let client = MumbleVoipClient {
                username: format!("{}_voice", un_a),
                context: format!("{}_ambient", sid_a),
                role: ClientRole::Voice,
                voip_status: status_a,
                target_channel: initial_ambient_ch,
                zone_channels: Some((fbo_ch, aircraft_ch)),
                denoise,
                test_pos,
            };
            let _ = client.run(server_addr, st_a, mic_rx_a, pb_tx_a).await;
        });
    }

    if test_client == TestClient::Voice {
        return;
    }

    // 5. Intercom Client
    if !matches!(test_client, TestClient::Pa | TestClient::Radio) {
        let st_i = Arc::clone(&state);
        let mic_rx_i = mic_tx.subscribe();
        let pb_tx_i = ic_pb_tx;
        let un_i = user_name.clone();
        let sid_i = session_id.clone();
        let status_i = mk_status("IC");
        tokio::spawn(async move {
            let client = MumbleVoipClient {
                username: format!("{}_ic", un_i),
                context: format!("{}_ic", sid_i),
                role: ClientRole::Ic,
                voip_status: status_i,
                target_channel: format!("{}_ic", sid_i),
                zone_channels: None,
                denoise,
                test_pos,
            };
            let _ = client.run(server_addr, st_i, mic_rx_i, pb_tx_i).await;
        });
    } // end IC guard

    // 6. PA (Public Address) Client — always in aircraft channel
    // TODO: PA is broadcast from multiple speakers (PSUs above each seat, lavatory ceiling).
    //       A single fixed point source does not accurately simulate this. Consider rendering
    //       PA as a non-positional flat mix, or blending several source positions, to replicate
    //       the enveloping "speakers everywhere" effect. Door attenuation should also be
    //       bypassed since the speakers are inside the fuselage.
    const PA_POSITION: [f32; 3] = [0.0, 0.0, 0.0]; // placeholder until rendering is redesigned
    if !matches!(test_client, TestClient::Ic | TestClient::Radio) {
        let st_pa = Arc::clone(&state);
        let mic_rx_pa = mic_tx.subscribe();
        let pb_tx_pa = ambient_pb_tx.clone();
        let un_pa = user_name.clone();
        let sid_pa = session_id.clone();
        let status_pa = mk_status("PA");
        tokio::spawn(async move {
            let client = MumbleVoipClient {
                username: format!("{}_PA", un_pa),
                context: format!("{}_ambient", sid_pa),
                role: ClientRole::Pa,
                voip_status: status_pa,
                target_channel: format!("{}_ambient_aircraft", sid_pa),
                zone_channels: None,
                denoise,
                test_pos: Some(PA_POSITION),
            };
            let _ = client.run(server_addr, st_pa, mic_rx_pa, pb_tx_pa).await;
        });
    } // end PA guard

    // 7. Radio Relay Client + local COM monitor
    // (radio_tx is a cloned Sender into the shared loopback channel — no new PW stream)
    if let Some(rtx) = radio_tx {
        let st_r = Arc::clone(&state);
        let radio_rx = rtx.subscribe();
        let pb_tx_r = ambient_pb_tx.clone();
        let un_r = user_name.clone();
        let sid_r = session_id.clone();
        let status_r = mk_status("Radio");
        const RADIO_SPEAKER_POSITION: [f32; 3] = [0.0, 0.9, 6.8]; // XP [0, 0.9, -6.8], Z negated
        tokio::spawn(async move {
            let client = MumbleVoipClient {
                username: format!("{}_radio", un_r),
                context: format!("{}_ambient", sid_r),
                role: ClientRole::Radio { has_source: true },
                voip_status: status_r,
                target_channel: format!("{}_ambient_aircraft", sid_r),
                zone_channels: None,
                denoise,
                test_pos: Some(RADIO_SPEAKER_POSITION),
            };
            let _ = client.run(server_addr, st_r, radio_rx, pb_tx_r).await;
        });

        // Always mirror the radio source to the IC output device so pilots hear
        // COM audio through their designated IC headphone output.
        // TX gating (com1_rx / com2_rx) is handled separately in on_mic_pcm.
        let mut monitor_rx = rtx.subscribe();
        tokio::spawn(async move {
            loop {
                match monitor_rx.recv().await {
                    Ok(pcm) => {
                        let stereo: Vec<f32> = pcm.iter().flat_map(|&s| [s, s]).collect();
                        let _ = ic_monitor_tx.send(stereo).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

/// Returns a `Sender` into the shared radio loopback broadcast channel.
/// The underlying PipeWire capture stream and forwarding thread are created only once
/// per process regardless of how many times `run_mumble_stack` is called.
fn radio_loopback_sender(source_name: String) -> broadcast::Sender<Vec<f32>> {
    static TX: OnceLock<broadcast::Sender<Vec<f32>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, _) = broadcast::channel::<Vec<f32>>(128);
        let tx_fwd = tx.clone();
        std::thread::spawn(move || {
            let (sync_tx, mut sync_rx) = mpsc::channel(128);
            start_loopback_capture(source_name, sync_tx);
            while let Some(frame) = sync_rx.blocking_recv() {
                let _ = tx_fwd.send(frame);
            }
        });
        tx
    })
    .clone()
}
