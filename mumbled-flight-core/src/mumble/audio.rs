//! High-Performance Audio Engine for MumbledFlight.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, debug, error, warn};
use tokio::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::collections::VecDeque;
use std::io::Read;

// Serialises the PIPEWIRE_NODE env-var mutation + CPAL device-open sequence.
// std::env::set_var is unsound when called concurrently from multiple threads;
// holding this lock across set_var → build_*_stream prevents the race.
fn pipewire_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

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

/// Name of the virtual PipeWire sink used for radio relay capture.
/// Shared with the plugin's device enumeration filter — change here propagates everywhere.
pub const VIRTUAL_SINK_NAME: &str = "MumblingRadio";

pub fn create_linux_sink() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        const S: &str = VIRTUAL_SINK_NAME;

        // Reuse if already present — avoids accumulating duplicate modules across reconnects.
        if let Ok(out) = std::process::Command::new("pactl").args(&["list", "short", "sinks"]).output() {
            let text = String::from_utf8_lossy(&out.stdout);
            if text.lines().any(|l| l.split_whitespace().nth(1) == Some(S)) {
                info!("[Audio:Linux] Reusing existing {S} virtual sink");
                return Some(format!("{S}.monitor"));
            }
        }

        info!("[Audio:Linux] Creating {S} virtual sink...");
        let status = std::process::Command::new("pactl")
            .args(&[
                "load-module", "module-null-sink",
                &format!("sink_name={S}"),
                "format=float32le", "rate=48000", "channels=2",
                &format!("sink_properties=device.description={S}"),
            ])
            .status();

        if let Ok(s) = status {
            if s.success() { return Some(format!("{S}.monitor")); }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    { None }
}

fn pipewire_device(node: Option<&str>, input: bool) -> cpal::Device {
    // Route to a specific PipeWire node when requested; clear the var otherwise
    // so a previous selection from the same process doesn't leak into the next stream.
    unsafe {
        match node {
            Some(n) => std::env::set_var("PIPEWIRE_NODE", n),
            None    => std::env::remove_var("PIPEWIRE_NODE"),
        }
    }
    let host = cpal::default_host();
    let is_pw = |d: &cpal::Device| d.name().unwrap_or_default().to_lowercase() == "pipewire";
    if input {
        host.input_devices().ok()
            .and_then(|mut d| d.find(is_pw))
            .or_else(|| {
                warn!("[Audio] 'pipewire' input device not found, falling back to system default");
                host.default_input_device()
            })
            .expect("No PipeWire input device")
    } else {
        host.output_devices().ok()
            .and_then(|mut d| d.find(is_pw))
            .or_else(|| {
                warn!("[Audio] 'pipewire' output device not found, falling back to system default");
                host.default_output_device()
            })
            .expect("No PipeWire output device")
    }
}

/// Generates a 500 Hz sine tone via ffmpeg and feeds it into the mic pipeline.
/// Requires ffmpeg on PATH. Intended for CLI test/debug mode only.
pub fn start_sine_capture(tx: mpsc::Sender<Vec<f32>>, gain: f32) {
    std::thread::spawn(move || {
        let mut child = match std::process::Command::new("ffmpeg")
            .args([
                "-f", "lavfi",
                "-i", "sine=f=500:r=48000",
                "-f", "f32le", "-ar", "48000", "-ac", "1",
                "-",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => { error!("[Audio:Sine] ffmpeg spawn failed: {e}"); return; }
        };

        info!("[Audio:Sine] 500 Hz sine tone active (gain {gain:.1}x)");

        let mut stdout = child.stdout.take().unwrap();
        const SAMPLES: usize = 960; // 20 ms @ 48 kHz
        const FRAME_DUR: std::time::Duration = std::time::Duration::from_millis(20);
        let mut buf = [0u8; SAMPLES * 4]; // f32le = 4 bytes/sample
        let mut next = std::time::Instant::now();
        loop {
            if stdout.read_exact(&mut buf).is_err() { break; }
            let frame: Vec<f32> = buf.chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * gain)
                .collect();
            let _ = tx.try_send(frame);
            // Sleep until the next 20 ms boundary to match real capture pacing.
            next += FRAME_DUR;
            if let Some(remaining) = next.checked_duration_since(std::time::Instant::now()) {
                std::thread::sleep(remaining);
            }
        }
        let _ = child.wait(); // reap so it doesn't linger as a zombie
    });
}

pub fn start_capture(
    tx: mpsc::Sender<Vec<f32>>,
    _denoise: bool,
    gain: f32,
    _gate_threshold: f32,
    device_name_filter: Option<String>,
    _is_loopback: bool,
) {
    std::thread::spawn(move || {
        // Hold the env lock across set_var → build_input_stream → play() so that
        // PIPEWIRE_NODE is not mutated by a concurrent stream-open in another thread.
        let stream = {
            let _guard = pipewire_env_lock().lock().unwrap();
            let device = pipewire_device(device_name_filter.as_deref(), true);
            let config = device.supported_input_configs()
                .expect("Failed to get configs")
                .filter(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000)
                .next()
                .expect("HARDWARE ERROR: Your device does not natively support 48,000Hz.")
                .with_sample_rate(cpal::SampleRate(48000));
            let num_channels = config.channels() as usize;
            info!("[Audio:Capture] Active: {} (48kHz, {} ch)", device.name().unwrap_or_default(), num_channels);
            let mut capture_buffer = Vec::new();
            let s = device.build_input_stream(
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
                None,
            ).expect("Failed to build input stream");
            s.play().expect("Failed to start input stream");
            s
        }; // PIPEWIRE_NODE lock released — stream is already connected
        loop { std::thread::sleep(std::time::Duration::from_secs(1)); }
        drop(stream);
    });
}

pub fn start_playback(mut rx: mpsc::Receiver<Vec<f32>>, preferred_device: Option<String>) {
    // Build and run entirely inside one thread — CPAL Stream is !Send on ALSA,
    // so creating and dropping it in the same thread avoids any Send requirement.
    std::thread::spawn(move || {
        let pending_samples    = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let pending_samples_cb = Arc::clone(&pending_samples);
        let is_playing         = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let is_playing_cb      = Arc::clone(&is_playing);

        let (stream, device_channels) = {
            let _guard = pipewire_env_lock().lock().unwrap();
            let device = pipewire_device(preferred_device.as_deref(), false);

            let supported_configs = device.supported_output_configs().expect("Failed to get configs");
            let config = supported_configs
                .filter(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000)
                .find(|c| c.channels() == 2)
                .or_else(|| device.supported_output_configs().unwrap().next())
                .expect("HARDWARE ERROR: Could not find a valid output config.")
                .with_sample_rate(cpal::SampleRate(48000));

            let device_channels = config.channels() as usize;
            info!("[Audio:Out] Sink: {} (48kHz, {} ch)", device.name().unwrap_or_default(), device_channels);

            let s = device.build_output_stream(
                &config.into(),
                move |data: &mut [f32], _| {
                    let mut lock = pending_samples_cb.lock().unwrap();
                    let available = lock.len();
                    if !is_playing_cb.load(std::sync::atomic::Ordering::SeqCst) || available < data.len() {
                        for x in data.iter_mut() { *x = 0.0; }
                        if is_playing_cb.load(std::sync::atomic::Ordering::SeqCst) && available < data.len() {
                            is_playing_cb.store(false, std::sync::atomic::Ordering::SeqCst);
                        }
                        return;
                    }
                    for (i, sample) in lock.drain(..data.len()).enumerate() {
                        data[i] = sample;
                    }
                },
                |err| error!("[Audio:Out] {}", err),
                None,
            ).expect("Failed to build output stream");
            s.play().expect("Failed to start output stream");
            (s, device_channels)
        }; // PIPEWIRE_NODE lock released — stream is already connected

        let hwm = 7200 * device_channels;

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
        drop(stream);
    });
}
