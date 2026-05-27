//! High-Stability Audio Engine with Hysteresis Jitter Management.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;

pub fn list_audio_devices() {
    println!("\n--- [ Audio Host & Device Discovery ] ---");
    for host_id in cpal::available_hosts() {
        println!("\nHost: {:?}", host_id);
        if let Ok(host) = host_from_id(host_id) {
            println!("  [ Input Devices ]:");
            if let Ok(devices) = host.input_devices() {
                for dev in devices { println!("    - {}", dev.name().unwrap_or_default()); }
            }
            println!("  [ Output Devices ]:");
            if let Ok(devices) = host.output_devices() {
                for dev in devices { println!("    - {}", dev.name().unwrap_or_default()); }
            }
        }
    }
    println!("------------------------------------------\n");
}

fn host_from_id(id: cpal::HostId) -> Result<cpal::Host, cpal::HostUnavailable> {
    cpal::host_from_id(id)
}

pub fn create_linux_sink() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("pactl").args(&["list", "short", "modules"]).output().map(|out| {
            let s = String::from_utf8_lossy(&out.stdout);
            for line in s.lines() {
                if line.contains("sink_name=MumblingRadio") {
                    if let Some(id) = line.split_whitespace().next() {
                        let _ = std::process::Command::new("pactl").args(&["unload-module", id]).status();
                    }
                }
            }
        });

        println!("[Audio:Linux] Creating MumblingRadio virtual device (48kHz float32le)...");
        let status = std::process::Command::new("pactl")
            .args(&["load-module", "module-null-sink", "sink_name=MumblingRadio", "format=float32le", "rate=48000", "channels=2", "sink_properties=device.description=MumblingRadio"])
            .status();
        
        if let Ok(s) = status {
            if s.success() { return Some("MumblingRadio.monitor".to_string()); }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    { None }
}

pub fn start_capture(
    tx: mpsc::Sender<Vec<f32>>, 
    _denoise: bool, 
    gain: f32, 
    _gate_threshold: f32,
    device_name_filter: Option<String>,
    _is_loopback: bool
) {
    std::thread::spawn(move || {
        let host = cpal::default_host();
        let mut found = None;
        if let Some(ref filter) = device_name_filter {
            for host_id in cpal::available_hosts() {
                if let Ok(h) = cpal::host_from_id(host_id) {
                    if let Ok(devices) = h.input_devices() {
                        for d in devices {
                            if d.name().unwrap_or_default().to_lowercase().contains(&filter.to_lowercase()) {
                                found = Some(d); break;
                            }
                        }
                    }
                }
                if found.is_some() { break; }
            }
        }
        let device = found.unwrap_or_else(|| host.default_input_device().expect("No default input found"));

        let supported_configs = device.supported_input_configs().expect("Failed to get configs");
        let config = supported_configs
            .filter(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000)
            .next()
            .expect("HARDWARE ERROR: Your device does not natively support 48,000Hz.")
            .with_sample_rate(cpal::SampleRate(48000));

        let num_channels = config.channels() as usize;
        println!("[Audio:Capture] Active: {} (Strict 48kHz, {} channels)", device.name().unwrap_or_default(), num_channels);

        let mut capture_buffer = Vec::new();
        let stream = device.build_input_stream(
            &config.into(),
            move |data: &[f32], _| {
                if num_channels > 1 {
                    for chunk in data.chunks_exact(num_channels) {
                        let mut sum = 0.0;
                        for i in 0..num_channels { sum += chunk[i]; }
                        capture_buffer.push((sum / num_channels as f32) * gain); 
                    }
                } else {
                    for &s in data { capture_buffer.push(s * gain); }
                }

                while capture_buffer.len() >= 960 {
                    let frame: Vec<f32> = capture_buffer.drain(..960).collect();
                    let _ = tx.try_send(frame);
                }
            },
            |err| eprintln!("[Audio:Capture] Error: {}", err),
            None
        ).expect("Failed to build input stream");

        stream.play().expect("Failed to start input stream");
        std::mem::forget(stream);
        loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
    });
}

pub fn start_playback(mut rx: mpsc::Receiver<Vec<f32>>) {
    let host = cpal::default_host();
    let device = host.default_output_device().expect("No output device found");
    
    let supported_configs = device.supported_output_configs().expect("Failed to get configs");
    let config = supported_configs
        .filter(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000)
        .next()
        .expect("HARDWARE ERROR: Your playback device does not support 48,000Hz.")
        .with_sample_rate(cpal::SampleRate(48000));
        
    let device_channels = config.channels() as usize;
    println!("[Audio:Out] Sink: {} ({:?} Host, Strict 48kHz, {} channels)", 
        device.name().unwrap_or_default(), host.id(), device_channels);

    // High-Performance Ring Buffer
    let pending_samples = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let pending_samples_cb = Arc::clone(&pending_samples);

    // HYSTERESIS BUFFER CONFIG:
    // We wait for 400ms of audio (channel-aware) before starting/resuming.
    let prime_threshold = 19200 * device_channels; 
    let is_playing = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let is_playing_cb = Arc::clone(&is_playing);

    std::thread::spawn(move || {
        let mut last_log = std::time::Instant::now();
        while let Some(stereo_frame_48k) = rx.blocking_recv() {
            let mut lock = pending_samples.lock().unwrap();
            
            if device_channels == 2 {
                lock.extend(stereo_frame_48k);
            } else {
                for chunk in stereo_frame_48k.chunks_exact(2) {
                    let mono = (chunk[0] + chunk[1]) * 0.5;
                    for _ in 0..device_channels { lock.push_back(mono); }
                }
            }

            // Start playing once we hit the threshold
            if !is_playing.load(std::sync::atomic::Ordering::SeqCst) && lock.len() >= prime_threshold {
                is_playing.store(true, std::sync::atomic::Ordering::SeqCst);
                println!("[Audio:Out] Buffer primed (400ms). Stream active.");
            }

            // High-Stability Buffer Cap (1 second)
            let max_buffered = 48000 * device_channels;
            if lock.len() > max_buffered {
                let to_drain = lock.len() - max_buffered;
                let aligned_drain = to_drain - (to_drain % device_channels);
                for _ in 0..aligned_drain { lock.pop_front(); }
            }

            if last_log.elapsed().as_secs() >= 5 {
                let ms = lock.len() as f32 / (48.0 * device_channels as f32);
                println!("[Audio:Out] Buffer Level: {:.1}ms", ms);
                last_log = std::time::Instant::now();
            }
        }
    });

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            let mut lock = pending_samples_cb.lock().unwrap();
            
            // HYSTERESIS BLOCK LOGIC:
            // 1. Only play if we are in 'playing' state.
            // 2. Only play if we have enough for the WHOLE hardware block.
            // This turns "shredding" into clean, rare drops.
            if !is_playing_cb.load(std::sync::atomic::Ordering::SeqCst) || lock.len() < data.len() {
                for x in data.iter_mut() { *x = 0.0; }
                
                // If we ran out while playing, stop and wait for a full refill
                if is_playing_cb.load(std::sync::atomic::Ordering::SeqCst) {
                    is_playing_cb.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                return;
            }

            // O(K) Removal (Atomic Alignment)
            for i in 0..data.len() {
                data[i] = lock.pop_front().unwrap_or(0.0);
            }
        },
        |err| eprintln!("[Audio:Out] Error: {}", err),
        None
    ).expect("Failed to build output stream");

    stream.play().expect("Failed to start output stream");
    std::mem::forget(stream);
}
