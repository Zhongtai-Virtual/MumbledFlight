//! Orchestrates the Mumble VoIP stack with multi-source Radio Relay.

pub mod audio;
pub mod voip;

use self::audio::{create_linux_sink, start_capture, start_loopback_capture, start_playback};
use self::voip::client::{ClientRole, MumbleVoipClient};
use crate::state::{CockpitState, SharedCockpitZone};
use std::net::SocketAddr;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TestClient {
    #[default]
    All,
    Ambient,
    Ic,
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

    let radio_tx = if test_client == TestClient::All && final_radio_source.is_some() {
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

    // 4. Ambient Client
    if test_client != TestClient::Ic {
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
        tokio::spawn(async move {
            let client = MumbleVoipClient {
                username: format!("{}_ambient", un_a),
                context: format!("{}_ambient", sid_a),
                role: ClientRole::Ambient,
                target_channel: initial_ambient_ch,
                zone_channels: Some((fbo_ch, aircraft_ch)),
                denoise,
                test_pos,
            };
            let _ = client.run(server_addr, st_a, mic_rx_a, pb_tx_a).await;
        });
    }

    if test_client == TestClient::Ambient {
        return;
    }

    // 5. Intercom Client
    let st_i = Arc::clone(&state);
    let mic_rx_i = mic_tx.subscribe();
    let pb_tx_i = ic_pb_tx;
    let un_i = user_name.clone();
    let sid_i = session_id.clone();
    tokio::spawn(async move {
        let client = MumbleVoipClient {
            username: format!("{}_ic", un_i),
            context: format!("{}_ic", sid_i),
            role: ClientRole::Ic,
            target_channel: format!("{}_ic", sid_i),
            zone_channels: None,
            denoise,
            test_pos,
        };
        let _ = client.run(server_addr, st_i, mic_rx_i, pb_tx_i).await;
    });

    // 6. PA (Public Address) Client — fixed cabin position, always in aircraft channel
    // TODO: replace with actual cabin speaker position once confirmed
    const PA_POSITION: [f32; 3] = [0.0, 0.0, 0.0];
    let st_pa = Arc::clone(&state);
    let mic_rx_pa = mic_tx.subscribe();
    let pb_tx_pa = ambient_pb_tx.clone();
    let un_pa = user_name.clone();
    let sid_pa = session_id.clone();
    tokio::spawn(async move {
        let client = MumbleVoipClient {
            username: format!("{}_PA", un_pa),
            context: format!("{}_ambient", sid_pa),
            role: ClientRole::Pa,
            target_channel: format!("{}_ambient_aircraft", sid_pa),
            zone_channels: None,
            denoise,
            test_pos: Some(PA_POSITION),
        };
        let _ = client.run(server_addr, st_pa, mic_rx_pa, pb_tx_pa).await;
    });

    // 7. Radio Relay Client + local COM monitor
    // (radio_tx is a cloned Sender into the shared loopback channel — no new PW stream)
    if let Some(rtx) = radio_tx {
        let st_r = Arc::clone(&state);
        let radio_rx = rtx.subscribe();
        let pb_tx_r = ambient_pb_tx.clone();
        let un_r = user_name.clone();
        let sid_r = session_id.clone();
        tokio::spawn(async move {
            let client = MumbleVoipClient {
                username: format!("{}_radio", un_r),
                context: format!("{}_radio", sid_r),
                role: ClientRole::Radio { has_source: true },
                target_channel: format!("{}_radio", sid_r),
                zone_channels: None,
                denoise,
                test_pos,
            };
            let _ = client.run(server_addr, st_r, radio_rx, pb_tx_r).await;
        });

        // Mirror the radio source to the IC output device when COM RX is active,
        // so pilots hear the COM audio through their designated IC headphone output.
        let mut monitor_rx = rtx.subscribe();
        let st_m = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                match monitor_rx.recv().await {
                    Ok(pcm) => {
                        let active = {
                            let s = st_m.lock().unwrap();
                            s.com1_rx || s.com2_rx
                        };
                        if active {
                            let stereo: Vec<f32> = pcm.iter().flat_map(|&s| [s, s]).collect();
                            let _ = ic_monitor_tx.send(stereo).await;
                        }
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
