//! High-Performance Audio Engine for MumblingCockpit.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, debug, error};
use tokio::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;

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

        info!("[Audio:Linux] Creating MumblingRadio virtual device...");
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

fn select_input_device(filter: Option<&str>) -> cpal::Device {
    let host = cpal::default_host();
    if let Some(f) = filter {
        if let Ok(mut devs) = host.input_devices() {
            if let Some(d) = devs.find(|d| d.name().unwrap_or_default().to_lowercase().contains(f)) {
                return d;
            }
        }
    }
    host.default_input_device().expect("No default input device")
}

fn select_output_device(preferred: Option<&str>) -> cpal::Device {
    let host = cpal::default_host();
    if let Some(f) = preferred {
        if let Ok(mut devs) = host.output_devices() {
            if let Some(d) = devs.find(|d| d.name().unwrap_or_default().to_lowercase().contains(f)) {
                return d;
            }
        }
    }
    host.output_devices().ok()
        .and_then(|mut d| d.find(|d| {
            let name = d.name().unwrap_or_default().to_lowercase();
            (name == "pulse" || name == "pipewire") &&
            d.supported_output_configs().map(|mut c| c.any(|cfg| cfg.channels() == 2)).unwrap_or(false)
        }))
        .or_else(|| host.default_output_device())
        .expect("No suitable output device")
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
        let device = select_input_device(device_name_filter.as_deref());

        let supported_configs = device.supported_input_configs().expect("Failed to get configs");
        let config = supported_configs
            .filter(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000)
            .next()
            .expect("HARDWARE ERROR: Your device does not natively support 48,000Hz.")
            .with_sample_rate(cpal::SampleRate(48000));

        let num_channels = config.channels() as usize;
        info!("[Audio:Capture] Active: {} (48kHz, {} ch)", device.name().unwrap_or_default(), num_channels);

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
            |err| error!("[Audio:Capture] {}", err),
            None
        ).expect("Failed to build input stream");

        stream.play().expect("Failed to start input stream");
        std::mem::forget(stream);
        loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
    });
}

pub fn start_playback(mut rx: mpsc::Receiver<Vec<f32>>, preferred_device: Option<String>) {
    let device = select_output_device(preferred_device.as_deref());
    
    let supported_configs = device.supported_output_configs().expect("Failed to get configs");
    let config = supported_configs
        .filter(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000)
        .find(|c| c.channels() == 2)
        .or_else(|| device.supported_output_configs().unwrap().next())
        .expect("HARDWARE ERROR: Could not find a valid output config.")
        .with_sample_rate(cpal::SampleRate(48000));
        
    let device_channels = config.channels() as usize;
    info!("[Audio:Out] Sink: {} (48kHz, {} ch)", device.name().unwrap_or_default(), device_channels);

    // REAL-TIME RING BUFFER: Optimized for O(1) removals.
    let pending_samples = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let pending_samples_cb = Arc::clone(&pending_samples);

    // Initial Prime: Wait for 150ms of audio (channel-aware)
    let hwm = 7200 * device_channels; 
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

            if lock.len() >= hwm {
                is_playing.store(true, std::sync::atomic::Ordering::SeqCst);
            }

            // High-Stability Buffer Cap (500ms)
            let max_buffered = 24000 * device_channels;
            if lock.len() > max_buffered {
                let to_drain = lock.len() - max_buffered;
                let aligned_drain = to_drain - (to_drain % device_channels);
                let _ = lock.drain(..aligned_drain);
            }

            if last_log.elapsed().as_secs() >= 5 {
                debug!("[Audio:Out] buffer {:.1}ms", lock.len() as f32 / (48.0 * device_channels as f32));
                last_log = std::time::Instant::now();
            }
        }
    });

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            let mut lock = pending_samples_cb.lock().unwrap();
            let available = lock.len();
            
            // HYSTERESIS BLOCK LOGIC:
            // Only play if we have hit the high water mark AND can fulfill the whole request.
            // This prevents the high-frequency clicking (shredding).
            if !is_playing_cb.load(std::sync::atomic::Ordering::SeqCst) || available < data.len() {
                for x in data.iter_mut() { *x = 0.0; }
                if is_playing_cb.load(std::sync::atomic::Ordering::SeqCst) && available < data.len() {
                    is_playing_cb.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                return;
            }

            // Bulk copy from buffer
            for (i, sample) in lock.drain(..data.len()).enumerate() {
                data[i] = sample;
            }
        },
        |err| error!("[Audio:Out] {}", err),
        None
    ).expect("Failed to build output stream");

    stream.play().expect("Failed to start output stream");
    std::mem::forget(stream);
}
