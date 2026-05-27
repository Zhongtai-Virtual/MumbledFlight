//! Orchestrates the Mumble VoIP stack with multi-source Radio Relay.

pub mod audio;
pub mod voip;

use std::sync::{Arc, Mutex};
use std::net::SocketAddr;
use tokio::sync::{broadcast, mpsc};
use crate::state::CockpitState;
use self::voip::MumbleVoipClient;
use self::audio::{start_capture, start_playback, create_linux_sink};

pub async fn run_mumble_stack(
    state: Arc<Mutex<CockpitState>>,
    user_name: String,
    session_id: String,
    gain: f32,
    denoise: bool,
    radio_source: Option<String>,
    auto_sink: bool,
    single_client: bool,
    server_addr: SocketAddr,
) {
    // 1. MIC Chain
    let (mic_tx, _) = broadcast::channel::<Vec<f32>>(128);
    let mic_tx_clone = mic_tx.clone();
    let d_mic = denoise;
    std::thread::spawn(move || {
        let (sync_tx, mut sync_rx) = mpsc::channel(128);
        start_capture(sync_tx, d_mic, gain, 0.0, None, false);
        while let Some(frame) = sync_rx.blocking_recv() {
            let _ = mic_tx_clone.send(frame);
        }
    });

    // Wait for the primary mic capture to fully establish itself in the OS mixer
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // 2. RADIO Chain
    let final_radio_source = if auto_sink { create_linux_sink() } else { radio_source };
    
    let radio_tx = if !single_client && final_radio_source.is_some() {
        let source_name = final_radio_source.unwrap();
        let (tx, _) = broadcast::channel::<Vec<f32>>(128);
        let tx_clone = tx.clone();
        std::thread::spawn(move || {
            let (sync_tx, mut sync_rx) = mpsc::channel(128);
            
            #[cfg(target_os = "linux")]
            {
                crate::mumble::audio::start_parec_capture(sync_tx, source_name);
            }
            #[cfg(not(target_os = "linux"))]
            {
                crate::mumble::audio::start_capture(sync_tx, false, 1.0, 0.0, Some(source_name), true);
            }

            while let Some(frame) = sync_rx.blocking_recv() {
                let _ = tx_clone.send(frame);
            }
        });
        Some(tx)
    } else {
        None
    };

    // 3. Playback Mixer
    let (playback_tx, playback_rx) = mpsc::channel(1024);
    start_playback(playback_rx);

    // 4. Ambient Client
    let st_a = Arc::clone(&state);
    let mic_rx_a = mic_tx.subscribe();
    let pb_tx_a = playback_tx.clone();
    let un_a = user_name.clone();
    let sid_a = session_id.clone();
    tokio::spawn(async move {
        let client = MumbleVoipClient {
            username: format!("{}_ambient", un_a),
            context: format!("{}_ambient", sid_a),
            is_ic: false,
            is_radio: false,
            target_channel: format!("{}_ambient", sid_a),
            denoise,
        };
        let _ = client.run(server_addr, st_a, mic_rx_a, pb_tx_a).await;
    });

    if single_client {
        return;
    }

    // 5. Intercom Client
    let st_i = Arc::clone(&state);
    let mic_rx_i = mic_tx.subscribe();
    let pb_tx_i = playback_tx.clone();
    let un_i = user_name.clone();
    let sid_i = session_id.clone();
    tokio::spawn(async move {
        let client = MumbleVoipClient {
            username: format!("{}_ic", un_i),
            context: format!("{}_ic", sid_i),
            is_ic: true,
            is_radio: false,
            target_channel: format!("{}_ic", sid_i),
            denoise,
        };
        let _ = client.run(server_addr, st_i, mic_rx_i, pb_tx_i).await;
    });

    // 6. Radio Relay Client
    if let Some(rtx) = radio_tx {
        let st_r = Arc::clone(&state);
        let radio_rx = rtx.subscribe();
        let pb_tx_r = playback_tx.clone();
        let un_r = user_name.clone();
        let sid_r = session_id.clone();
        tokio::spawn(async move {
            let client = MumbleVoipClient {
                username: format!("{}_radio", un_r),
                context: format!("{}_radio", sid_r),
                is_ic: false,
                is_radio: true,
                target_channel: format!("{}_radio", sid_r),
                denoise,
            };
            let _ = client.run(server_addr, st_r, radio_rx, pb_tx_r).await;
        });
    }
}
