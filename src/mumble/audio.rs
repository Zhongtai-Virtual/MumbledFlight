//! Audio engine with strict 48kHz hardware enforcement and high-fidelity downmixing.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tokio::sync::mpsc;
use std::sync::{Arc, Mutex};

pub fn list_audio_devices() {
    println!("\n--- [ Audio Host & Device Discovery ] ---");
    for host_id in cpal::available_hosts() {
        println!("\nHost: {:?}", host_id);
        if let Ok(host) = cpal::host_from_id(host_id) {
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

#[cfg(target_os = "linux")]
fn get_all_source_outputs() -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    if let Ok(output) = std::process::Command::new("pactl").args(&["list", "short", "source-outputs"]).output() {
        let s = String::from_utf8_lossy(&output.stdout);
        for line in s.lines() {
            if let Some(id) = line.split_whitespace().next() {
                ids.insert(id.to_string());
            }
        }
    }
    ids
}

#[cfg(not(target_os = "linux"))]
fn get_all_source_outputs() -> std::collections::HashSet<String> { std::collections::HashSet::new() }

use std::io::Read;
pub fn start_parec_capture(tx: mpsc::Sender<Vec<f32>>, device_name: String) {
    std::thread::spawn(move || {
        println!("[Audio:Capture] Using 'parec' for loopback device: {}", device_name);
        let mut child = std::process::Command::new("parec")
            .args(&["--device", &device_name, "--format=float32le", "--rate=48000", "--channels=1"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("Failed to start parec.");
        let mut stdout = child.stdout.take().unwrap();
        let mut buffer = [0u8; 960 * 4];
        loop {
            match stdout.read_exact(&mut buffer) {
                Ok(_) => {
                    let mut frame = Vec::with_capacity(960);
                    for chunk in buffer.chunks_exact(4) {
                        frame.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]).clamp(-1.0, 1.0));
                    }
                    let _ = tx.try_send(frame);
                }
                Err(_) => break,
            }
        }
    });
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
        let mut needs_linux_move = false;
        let mut target_name = String::new();
        let pre_stream_ids = get_all_source_outputs();

        let device = if let Some(ref filter) = device_name_filter {
            target_name = filter.clone();
            if filter == "MumblingRadio.monitor" && cfg!(target_os = "linux") {
                needs_linux_move = true;
                host.default_input_device().expect("No default input found")
            } else {
                let mut found = None;
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
                found.expect(&format!("Could not find device: {}", filter))
            }
        } else {
            host.default_input_device().expect("No default input found")
        };

        // --- ENFORCE STRICT 48kHz HARDWARE CAPTURE ---
        // This avoids low-quality nearest-neighbor resampling.
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
                // High-Fidelity Downmix
                if num_channels > 1 {
                    for chunk in data.chunks_exact(num_channels) {
                        let mut sum = 0.0;
                        for i in 0..num_channels { sum += chunk[i]; }
                        capture_buffer.push((sum / num_channels as f32) * gain); 
                    }
                } else {
                    for &s in data { capture_buffer.push(s * gain); }
                }

                // Packetize into exactly 20ms chunks (960 samples)
                while capture_buffer.len() >= 960 {
                    let frame: Vec<f32> = capture_buffer.drain(..960).collect();
                    let _ = tx.try_send(frame);
                }
            },
            |err| eprintln!("[Audio:Capture] Error: {}", err),
            None
        ).expect("Failed to build input stream");

        stream.play().expect("Failed to start input stream");

        if needs_linux_move {
            std::thread::spawn(move || {
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    let post_stream_ids = get_all_source_outputs();
                    for id in post_stream_ids {
                        if !pre_stream_ids.contains(&id) {
                            let _ = std::process::Command::new("pactl").args(&["move-source-output", &id, &target_name]).status();
                            return;
                        }
                    }
                }
            });
        }

        std::mem::forget(stream);
        loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
    });
}

pub fn start_playback(mut rx: mpsc::Receiver<Vec<f32>>) {
    let host = cpal::default_host();
    let device = host.default_output_device().expect("No output device found");
    
    // Enforce 48kHz on Playback too
    let supported_configs = device.supported_output_configs().expect("Failed to get configs");
    let config = supported_configs
        .filter(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000)
        .next()
        .expect("HARDWARE ERROR: Your playback device does not support 48,000Hz.")
        .with_sample_rate(cpal::SampleRate(48000));
        
    let device_channels = config.channels() as usize;
    println!("[Audio:Out] Sink: {} (Strict 48kHz, {} channels)", device.name().unwrap_or_default(), device_channels);

    let pending_samples = Arc::new(Mutex::new(Vec::<f32>::new()));
    let pending_samples_cb = Arc::clone(&pending_samples);

    std::thread::spawn(move || {
        while let Some(stereo_frame_48k) = rx.blocking_recv() {
            let mut lock = pending_samples.lock().unwrap();
            
            // Stereo input to hardware channels
            if device_channels == 2 {
                lock.extend_from_slice(&stereo_frame_48k);
            } else {
                for chunk in stereo_frame_48k.chunks_exact(2) {
                    let mono = (chunk[0] + chunk[1]) * 0.5;
                    for _ in 0..device_channels { lock.push(mono); }
                }
            }

            // Cap buffer at 100ms to keep latency low
            let max_buffered = 4800 * device_channels;
            if lock.len() > max_buffered {
                let to_drain = lock.len() - (max_buffered / 2);
                let _ = lock.drain(..to_drain);
            }
        }
    });

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            let mut lock = pending_samples_cb.lock().unwrap();
            let available = lock.len();
            let to_write = std::cmp::min(available, data.len());
            if to_write > 0 {
                for (i, sample) in lock.drain(..to_write).enumerate() { data[i] = sample; }
            }
            for i in to_write..data.len() { data[i] = 0.0; }
        },
        |err| eprintln!("[Audio:Out] Error: {}", err),
        None
    ).expect("Failed to build output stream");

    stream.play().expect("Failed to start output stream");
    std::mem::forget(stream);
}
