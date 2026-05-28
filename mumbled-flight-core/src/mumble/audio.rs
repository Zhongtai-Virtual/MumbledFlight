//! High-Performance Audio Engine for MumbledFlight.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, debug, error, warn};
use tokio::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicU32, Ordering};
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

/// Info about a PipeWire audio sink returned by [`enumerate_pw_sinks`].
pub struct PwSinkInfo {
    pub name: String,
    pub description: String,
}

/// Enumerate PipeWire audio sinks (output devices) via the PW registry.
#[cfg(target_os = "linux")]
pub fn enumerate_pw_sinks() -> Vec<PwSinkInfo> {
    use pipewire as pw;
    pw_enumerate(|props| {
        if props.get(*pw::keys::MEDIA_CLASS) != Some("Audio/Sink") { return None; }
        let name = props.get(*pw::keys::NODE_NAME)?.to_string();
        let desc = props.get(*pw::keys::NODE_DESCRIPTION).unwrap_or(&name).to_string();
        Some(PwSinkInfo { name, description: desc })
    })
}

#[cfg(not(target_os = "linux"))]
pub fn enumerate_pw_sinks() -> Vec<PwSinkInfo> { vec![] }

/// Enumerate PipeWire audio sources (input devices, monitors excluded) via the PW registry.
#[cfg(target_os = "linux")]
pub fn enumerate_pw_sources() -> Vec<String> {
    use pipewire as pw;
    pw_enumerate(|props| {
        if props.get(*pw::keys::MEDIA_CLASS) != Some("Audio/Source") { return None; }
        Some(props.get(*pw::keys::NODE_NAME)?.to_string())
    })
}

#[cfg(not(target_os = "linux"))]
pub fn enumerate_pw_sources() -> Vec<String> { vec![] }

/// Enumerate sinks and sources in a single PipeWire round-trip.
/// Prefer this over calling [`enumerate_pw_sinks`] and [`enumerate_pw_sources`] separately.
#[cfg(target_os = "linux")]
pub fn enumerate_pw_devices() -> (Vec<PwSinkInfo>, Vec<String>) {
    use pipewire as pw;
    use std::cell::RefCell;
    use std::rc::Rc;

    pw::init();
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else { return (vec![], vec![]) };
    let Ok(context)  = pw::context::ContextRc::new(&mainloop, None) else { return (vec![], vec![]) };
    let Ok(core)     = context.connect_rc(None) else { return (vec![], vec![]) };
    let Ok(registry) = core.get_registry() else { return (vec![], vec![]) };

    let sinks:   Rc<RefCell<Vec<PwSinkInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let sources: Rc<RefCell<Vec<String>>>     = Rc::new(RefCell::new(Vec::new()));
    let sinks_cl   = sinks.clone();
    let sources_cl = sources.clone();

    let _reg = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            match props.get(*pw::keys::MEDIA_CLASS) {
                Some("Audio/Sink") => {
                    if let Some(name) = props.get(*pw::keys::NODE_NAME) {
                        let desc = props.get(*pw::keys::NODE_DESCRIPTION).unwrap_or(name).to_string();
                        sinks_cl.borrow_mut().push(PwSinkInfo { name: name.to_string(), description: desc });
                    }
                }
                Some("Audio/Source") => {
                    if let Some(name) = props.get(*pw::keys::NODE_NAME) {
                        sources_cl.borrow_mut().push(name.to_string());
                    }
                }
                _ => {}
            }
        })
        .register();

    let Ok(pending) = core.sync(0) else { return (vec![], vec![]) };
    let ml = mainloop.clone();
    let _done = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == 0 && seq == pending { ml.quit(); }
        })
        .register();

    mainloop.run();

    (
        Rc::try_unwrap(sinks).ok().map(|r| r.into_inner()).unwrap_or_default(),
        Rc::try_unwrap(sources).ok().map(|r| r.into_inner()).unwrap_or_default(),
    )
}

#[cfg(not(target_os = "linux"))]
pub fn enumerate_pw_devices() -> (Vec<PwSinkInfo>, Vec<String>) { (vec![], vec![]) }

/// Enumerate PW globals, mapping each node's properties through `mapper`.
/// Runs a throw-away PW mainloop that quits after the initial registry dump.
#[cfg(target_os = "linux")]
fn pw_enumerate<T, F>(mapper: F) -> Vec<T>
where
    T: 'static,
    F: Fn(&pipewire::spa::utils::dict::DictRef) -> Option<T> + 'static,
{
    use pipewire as pw;
    use std::cell::RefCell;
    use std::rc::Rc;

    pw::init();
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else { return vec![] };
    let Ok(context)  = pw::context::ContextRc::new(&mainloop, None) else { return vec![] };
    let Ok(core)     = context.connect_rc(None) else { return vec![] };
    let Ok(registry) = core.get_registry() else { return vec![] };

    let results: Rc<RefCell<Vec<T>>> = Rc::new(RefCell::new(Vec::new()));
    let results_cl = results.clone();

    let _reg = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            if let Some(v) = mapper(props) {
                results_cl.borrow_mut().push(v);
            }
        })
        .register();

    // sync(0) round-trips through the server; the done event fires after all
    // current registry globals have been delivered.
    let Ok(pending) = core.sync(0) else { return vec![] };
    let ml = mainloop.clone();
    let _done = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == 0 && seq == pending {
                ml.quit();
            }
        })
        .register();

    mainloop.run();

    Rc::try_unwrap(results)
        .ok()
        .map(|rc| rc.into_inner())
        .unwrap_or_default()
}

/// Creates (or reuses) the `MumblingRadio` virtual null-sink via the PipeWire API.
/// Returns the monitor source name used by [`start_loopback_capture`].
/// The sink is kept alive by a background thread for the life of the process.
pub fn create_linux_sink() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // Only cache success — a None result (transient PW failure) must not
        // permanently disable the radio relay for the session.
        static SINK: OnceLock<String> = OnceLock::new();
        if let Some(monitor) = SINK.get() {
            return Some(monitor.clone());
        }

        const S: &str = VIRTUAL_SINK_NAME;
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(1);

        std::thread::spawn(move || {
            if let Err(e) = pw_null_sink_loop(S, ready_tx) {
                error!("[Audio:Linux] PipeWire null-sink error: {e}");
            }
        });

        let monitor = format!("{S}.monitor");
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(()) => {
                info!("[Audio:Linux] {monitor} ready");
                let _ = SINK.set(monitor.clone());
                Some(monitor)
            }
            Err(_) => {
                warn!("[Audio:Linux] Timed out waiting for {S} virtual sink");
                None
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    { None }
}

/// Connects to PipeWire, checks whether `sink_name` already exists in the current registry,
/// and creates it only if absent.  Signals `ready_tx` once the sink is confirmed present,
/// then runs the mainloop forever to keep any newly-created node alive.
///
/// Two-phase design:
///   Phase 1 — sync-round-trip to collect the initial global dump; quit when done fires.
///   Phase 2 — create the node if not seen in phase 1, then run forever.
#[cfg(target_os = "linux")]
fn pw_null_sink_loop(
    sink_name: &'static str,
    ready_tx: std::sync::mpsc::SyncSender<()>,
) -> Result<(), pipewire::Error> {
    use pipewire as pw;
    use std::cell::Cell;
    use std::rc::Rc;

    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context  = pw::context::ContextRc::new(&mainloop, None)?;
    let core     = context.connect_rc(None)?;
    let registry = core.get_registry()?;

    let sink_seen = Rc::new(Cell::new(false));
    let sink_seen_cl = sink_seen.clone();
    let tx = Rc::new(Cell::new(Some(ready_tx)));
    let tx_cl = tx.clone();

    // Fires for every global — both current ones (phase 1) and new ones (phase 2).
    let _reg = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            if props.get(*pw::keys::MEDIA_CLASS) == Some("Audio/Sink")
                && props.get(*pw::keys::NODE_NAME) == Some(sink_name)
            {
                sink_seen_cl.set(true);
                if let Some(sender) = tx_cl.take() {
                    let _ = sender.send(());
                }
            }
        })
        .register();

    // Phase 1: sync round-trip so the server flushes all current registry globals
    // before we decide whether to create the node.
    let pending = core.sync(0)?;
    let ml = mainloop.clone();
    let _done = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == 0 && seq == pending {
                ml.quit(); // end phase 1
            }
        })
        .register();

    mainloop.run(); // returns once the initial global dump is complete

    // Phase 2: create the node only if not already present.
    let _node: Option<pw::node::Node> = if sink_seen.get() {
        info!("[Audio:Linux] Reusing existing {sink_name} virtual sink");
        None
    } else {
        info!("[Audio:Linux] Creating {sink_name} virtual sink via PipeWire...");
        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            "audio.rate"           => "48000",
            "audio.channels"       => "2",
            "audio.format"         => "F32LE",
        };
        props.insert(*pw::keys::FACTORY_NAME, "support.null-audio-sink");
        props.insert(*pw::keys::NODE_NAME, sink_name);
        props.insert(*pw::keys::NODE_DESCRIPTION, sink_name);
        // Proxy must stay alive — dropping it destroys the node in the PW graph.
        Some(core.create_object::<pw::node::Node>("adapter", &props)?)
    };

    // Run forever: keeps the created node alive (or just stays connected if reusing).
    mainloop.run();
    Ok(())
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
pub fn start_sine_capture(tx: mpsc::Sender<Vec<f32>>, mic_gain: Arc<AtomicU32>) {
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

        info!("[Audio:Sine] 500 Hz sine tone active");

        let mut stdout = child.stdout.take().unwrap();
        const SAMPLES: usize = 960; // 20 ms @ 48 kHz
        const FRAME_DUR: std::time::Duration = std::time::Duration::from_millis(20);
        let mut buf = [0u8; SAMPLES * 4]; // f32le = 4 bytes/sample
        let mut next = std::time::Instant::now();
        loop {
            if stdout.read_exact(&mut buf).is_err() { break; }
            let gain = f32::from_bits(mic_gain.load(Ordering::Relaxed));
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

/// Captures audio from a named PipeWire source using the native PipeWire API.
/// For monitor sources (e.g. "MumblingRadio.monitor") the ".monitor" suffix is stripped
/// and STREAM_CAPTURE_SINK is set, because in native PipeWire the monitor ports live on
/// the sink node itself — there is no separate monitor node.
pub fn start_loopback_capture(source_name: String, tx: mpsc::Sender<Vec<f32>>) {
    #[cfg(target_os = "linux")]
    std::thread::spawn(move || {
        // std::sync channel for the RT callback (no async runtime in RT thread).
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(256);

        {
            let src = source_name.clone();
            std::thread::spawn(move || {
                if let Err(e) = pw_capture_loop(&src, raw_tx) {
                    error!("[Audio:Loopback] PipeWire error: {e}");
                }
            });
        }

        // Accumulate variable-sized PipeWire buffers into 960-sample Opus frames.
        let mut accum: Vec<f32> = Vec::new();
        for chunk in raw_rx {
            accum.extend_from_slice(&chunk);
            while accum.len() >= 960 {
                let frame: Vec<f32> = accum.drain(..960).collect();
                let _ = tx.try_send(frame);
            }
        }
    });

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source_name, tx);
        warn!("[Audio:Loopback] Loopback capture is only supported on Linux (via PipeWire)");
    }
}

#[cfg(target_os = "linux")]
fn pw_capture_loop(
    source_name: &str,
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
) -> Result<(), pipewire::Error> {
    use pipewire as pw;
    use pw::spa::{
        param::{audio::{AudioFormat, AudioInfoRaw}, ParamType},
        pod::{Object, Pod, Value, serialize::PodSerializer},
        utils::{Direction, SpaTypes},
    };

    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context  = pw::context::ContextRc::new(&mainloop, None)?;
    let core     = context.connect_rc(None)?;

    // "MumblingRadio.monitor" → target "MumblingRadio" + STREAM_CAPTURE_SINK
    // because monitor ports in native PipeWire live on the sink node itself.
    let (target, capture_sink) = source_name
        .strip_suffix(".monitor")
        .map(|s| (s.to_string(), true))
        .unwrap_or_else(|| (source_name.to_string(), false));

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE     => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Communication",
    };
    props.insert(*pw::keys::TARGET_OBJECT, target);
    if capture_sink {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }

    let stream = pw::stream::StreamBox::new(&core, "mumbled-flight-loopback", props)?;

    let _listener = stream
        .add_local_listener_with_user_data(tx)
        .process(|stream, tx| {
            let mut buf = match stream.dequeue_buffer() {
                Some(b) => b,
                None    => return,
            };
            let datas = buf.datas_mut();
            if datas.is_empty() { return; }
            let n = datas[0].chunk().size() as usize;
            if let Some(raw) = datas[0].data() {
                let end = n.min(raw.len());
                let samples: Vec<f32> = raw[..end]
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect();
                let _ = tx.try_send(samples);
            }
        })
        .register()?;

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(48000);
    audio_info.set_channels(1);
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id:    ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values: Vec<u8> = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    ).unwrap().0.into_inner();
    let mut params = [Pod::from_bytes(&values).unwrap()];

    stream.connect(
        Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    info!("[Audio:Loopback] PipeWire capture active: '{source_name}'");
    mainloop.run();
    Ok(())
}

pub fn start_capture(
    tx: mpsc::Sender<Vec<f32>>,
    _denoise: bool,
    mic_gain: Arc<AtomicU32>,
    _gate_threshold: f32,
    device_name_filter: Option<String>,
) {
    std::thread::spawn(move || {
        // Hold the env lock across set_var → build_input_stream → play() so that
        // PIPEWIRE_NODE is not mutated by a concurrent stream-open in another thread.
        let _stream = {
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
                    let gain = f32::from_bits(mic_gain.load(Ordering::Relaxed));
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
