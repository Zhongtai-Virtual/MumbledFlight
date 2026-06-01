// Copyright (C) 2026 Zhongtai Virtual
//
// This file is part of MumbledFlight.
//
// MumbledFlight is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// MumbledFlight is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with MumbledFlight.  If not, see <https://www.gnu.org/licenses/>.


//! High-Performance Audio Engine for MumbledFlight.

use super::OPUS_FRAME_SAMPLES;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, debug, error, warn};
use nnnoiseless::DenoiseState;
use tokio::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::collections::VecDeque;
use std::io::Read;

// RNNoise VAD probability below this value is treated as non-speech.
// Exposed at module scope so hold/fade extensions can reference it without
// navigating into the capture closure.
const VAD_THRESHOLD: f32 = 0.5;

// Serialises concurrent CPAL device-open calls. On Linux also gates the
// PIPEWIRE_NODE env-var mutation, which is unsound under concurrent writes.
fn device_open_lock() -> &'static Mutex<()> {
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
#[derive(Clone)]
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
pub fn enumerate_pw_devices() -> (Vec<PwSinkInfo>, Vec<PwSinkInfo>) {
    use pipewire as pw;
    use std::cell::RefCell;
    use std::rc::Rc;

    pw::init();
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => { error!("[DeviceEnum] mainloop: {e}"); return (vec![], vec![]) }
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => { error!("[DeviceEnum] context: {e}"); return (vec![], vec![]) }
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => { error!("[DeviceEnum] connect: {e}"); return (vec![], vec![]) }
    };
    let registry = match core.get_registry() {
        Ok(r) => r,
        Err(e) => { error!("[DeviceEnum] registry: {e}"); return (vec![], vec![]) }
    };

    let sinks:      Rc<RefCell<Vec<PwSinkInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let sources:    Rc<RefCell<Vec<PwSinkInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let total_seen: Rc<RefCell<u32>>             = Rc::new(RefCell::new(0));
    let sinks_cl   = sinks.clone();
    let sources_cl = sources.clone();
    let total_cl   = total_seen.clone();

    let _reg = registry
        .add_listener_local()
        .global(move |global| {
            *total_cl.borrow_mut() += 1;
            let Some(props) = global.props else { return };
            let class = props.get(*pw::keys::MEDIA_CLASS).unwrap_or("(no class)");
            match class {
                "Audio/Sink" => {
                    if let Some(name) = props.get(*pw::keys::NODE_NAME) {
                        let desc = props.get(*pw::keys::NODE_DESCRIPTION).unwrap_or(name).to_string();
                        debug!("[DeviceEnum] sink: {name} ({desc})");
                        sinks_cl.borrow_mut().push(PwSinkInfo { name: name.to_string(), description: desc });
                    }
                }
                "Audio/Source" => {
                    if let Some(name) = props.get(*pw::keys::NODE_NAME) {
                        let desc = props.get(*pw::keys::NODE_DESCRIPTION).unwrap_or(name).to_string();
                        debug!("[DeviceEnum] source: {name} ({desc})");
                        sources_cl.borrow_mut().push(PwSinkInfo { name: name.to_string(), description: desc });
                    }
                }
                other => {
                    debug!("[DeviceEnum] global id={} class={other}", global.id);
                }
            }
        })
        .register();

    let pending = match core.sync(0) {
        Ok(p) => p,
        Err(e) => { error!("[DeviceEnum] sync: {e}"); return (vec![], vec![]) }
    };
    let ml = mainloop.clone();
    let _done = core
        .add_listener_local()
        .done(move |id, seq| {
            debug!("[DeviceEnum] done id={id} seq={seq:?} pending={pending:?}");
            // id 0 = core proxy in native PW; some versions report a different id.
            // Match on sequence number alone as a fallback.
            if seq == pending { ml.quit(); }
        })
        .register();

    mainloop.run();

    let total = *total_seen.borrow();
    debug!("[DeviceEnum] sync complete: {total} globals seen");

    // _reg's closure still holds sinks_cl/sources_cl — Rc strong count > 1 so
    // try_unwrap would fail. borrow() is safe here since the mainloop has quit
    // and the closure is no longer executing.
    let out_sinks   = sinks.borrow().clone();
    let out_sources = sources.borrow().clone();
    (out_sinks, out_sources)
}

#[cfg(not(target_os = "linux"))]
pub fn enumerate_pw_devices() -> (Vec<PwSinkInfo>, Vec<PwSinkInfo>) { (vec![], vec![]) }

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
            debug!("[PwEnum] done id={id} seq={seq:?} pending={pending:?}");
            if seq == pending { ml.quit(); }
        })
        .register();

    mainloop.run();

    let out: Vec<T> = results.borrow_mut().drain(..).collect();
    out
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
        .done(move |_id, seq| {
            if seq == pending {
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

/// Select a CPAL device by optional name, falling back to the system default.
///
/// On Linux: routes PipeWire to the requested node via `PIPEWIRE_NODE` and
/// returns the generic "pipewire" CPAL device. Must be called with
/// `device_open_lock` held so the env-var write is not concurrent.
///
/// On macOS/Windows: iterates CPAL devices and matches by exact name.
#[cfg(target_os = "linux")]
fn select_device(node: Option<&str>, input: bool) -> Option<cpal::Device> {
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
    } else {
        host.output_devices().ok()
            .and_then(|mut d| d.find(is_pw))
            .or_else(|| {
                warn!("[Audio] 'pipewire' output device not found, falling back to system default");
                host.default_output_device()
            })
    }
}

#[cfg(not(target_os = "linux"))]
fn select_device(node: Option<&str>, input: bool) -> Option<cpal::Device> {
    let host = cpal::default_host();
    if let Some(name) = node {
        let devices = if input { host.input_devices() } else { host.output_devices() };
        if let Some(d) = devices.ok().and_then(|mut it| it.find(|d| d.name().unwrap_or_default() == name)) {
            return Some(d);
        }
        warn!("[Audio] device '{name}' not found, falling back to system default");
    }
    if input { host.default_input_device() } else { host.default_output_device() }
}

/// Generates a 500 Hz sine tone via ffmpeg and feeds it into the mic pipeline.
/// Requires ffmpeg on PATH. Intended for CLI test/debug mode only.
pub fn start_sine_capture(tx: mpsc::Sender<Vec<f32>>, mic_gain: Arc<AtomicU32>) {
    run_ffmpeg_capture(
        vec![
            "-f".into(), "lavfi".into(),
            "-i".into(), "sine=f=500:r=48000".into(),
        ],
        "[Audio:Sine]",
        true,
        tx,
        mic_gain,
    );
}

/// Decodes an audio file via ffmpeg and feeds it into the mic pipeline, looping on EOF.
/// Requires ffmpeg on PATH. Intended for CLI test/debug mode only.
pub fn start_file_capture(path: std::path::PathBuf, tx: mpsc::Sender<Vec<f32>>, mic_gain: Arc<AtomicU32>) {
    run_ffmpeg_capture(
        vec!["-i".into(), path.to_string_lossy().into_owned()],
        "[Audio:File]",
        true,
        tx,
        mic_gain,
    );
}

/// Spawns ffmpeg with the given input args, piping decoded f32le mono 48 kHz audio into `tx`.
/// Loops from the beginning on EOF when `loop_on_eof` is true.
fn run_ffmpeg_capture(
    input_args: Vec<String>,
    log_tag: &'static str,
    loop_on_eof: bool,
    tx: mpsc::Sender<Vec<f32>>,
    mic_gain: Arc<AtomicU32>,
) {
    std::thread::spawn(move || {
        const SAMPLES: usize = OPUS_FRAME_SAMPLES;
        const FRAME_DUR: std::time::Duration = std::time::Duration::from_millis(20);
        let mut next = std::time::Instant::now();
        loop {
            let mut cmd = std::process::Command::new("ffmpeg");
            for arg in &input_args { cmd.arg(arg); }
            cmd.args(["-f", "f32le", "-ar", "48000", "-ac", "1", "-"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => { error!("{log_tag} ffmpeg spawn failed: {e}"); return; }
            };

            info!("{log_tag} ffmpeg active");

            let mut stdout = child.stdout.take().unwrap();
            let mut buf = [0u8; SAMPLES * 4];
            loop {
                if stdout.read_exact(&mut buf).is_err() { break; }
                let gain = f32::from_bits(mic_gain.load(Ordering::Relaxed));
                let frame: Vec<f32> = buf.chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * gain)
                    .collect();
                let _ = tx.try_send(frame);
                next += FRAME_DUR;
                if let Some(rem) = next.checked_duration_since(std::time::Instant::now()) {
                    std::thread::sleep(rem);
                }
            }
            let _ = child.wait();
            if !loop_on_eof { break; }
        }
    });
}

/// Captures audio from a named PipeWire source using the native PipeWire API.
/// For monitor sources (e.g. "MumblingRadio.monitor") the ".monitor" suffix is stripped
/// and STREAM_CAPTURE_SINK is set, because in native PipeWire the monitor ports live on
/// the sink node itself — there is no separate monitor node.
/// `shutdown` is watched so the capture stream is torn down when the connection ends, instead of
/// leaking across reconnects — this is what lets a changed radio source take effect on reconnect.
pub fn start_loopback_capture(source_name: String, tx: mpsc::Sender<Vec<f32>>, shutdown: Arc<AtomicBool>) {
    #[cfg(target_os = "linux")]
    std::thread::spawn(move || {
        // std::sync channel for the RT callback (no async runtime in RT thread).
        let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(256);

        {
            let src = source_name.clone();
            std::thread::spawn(move || {
                if let Err(e) = pw_capture_loop(&src, raw_tx, shutdown) {
                    error!("[Audio:Loopback] PipeWire error: {e}");
                }
            });
        }

        // Accumulate variable-sized PipeWire buffers into 960-sample Opus frames.
        // Ends when pw_capture_loop quits (on shutdown) and drops raw_tx.
        let mut accum: Vec<f32> = Vec::new();
        for chunk in raw_rx {
            accum.extend_from_slice(&chunk);
            while accum.len() >= OPUS_FRAME_SAMPLES {
                let frame: Vec<f32> = accum.drain(..OPUS_FRAME_SAMPLES).collect();
                let _ = tx.try_send(frame);
            }
        }
        info!("[Audio:Loopback] capture stopped");
    });

    #[cfg(not(target_os = "linux"))]
    std::thread::spawn(move || {
        let _stream = {
            let _guard = device_open_lock().lock().unwrap();
            let device = match select_device(Some(&source_name), true) {
                Some(d) => d,
                None => { error!("[Audio:Loopback] no input device available for '{source_name}'"); return; }
            };
            let device_name = device.name().unwrap_or_else(|_| "(unknown)".into());
            let config = match device.supported_input_configs() {
                Ok(mut cfgs) => cfgs.find(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000),
                Err(e) => { error!("[Audio:Loopback] failed to query configs for '{device_name}': {e}"); return; }
            };
            let config = match config {
                Some(c) => c.with_sample_rate(cpal::SampleRate(48000)),
                None => { error!("[Audio:Loopback] '{device_name}' does not support 48 kHz"); return; }
            };
            let num_channels = config.channels() as usize;
            info!("[Audio:Loopback] Active: {device_name} (48kHz, {num_channels} ch)");

            let mut accum: Vec<f32> = Vec::new();
            let s = match device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| {
                    // Downmix to mono then accumulate into 960-sample Opus frames.
                    if num_channels > 1 {
                        for chunk in data.chunks_exact(num_channels) {
                            let sum: f32 = chunk.iter().sum();
                            accum.push(sum / num_channels as f32);
                        }
                    } else {
                        accum.extend_from_slice(data);
                    }
                    while accum.len() >= OPUS_FRAME_SAMPLES {
                        let frame: Vec<f32> = accum.drain(..OPUS_FRAME_SAMPLES).collect();
                        let _ = tx.try_send(frame);
                    }
                },
                |err| error!("[Audio:Loopback] stream error: {err}"),
                None,
            ) {
                Ok(s) => s,
                Err(e) => { error!("[Audio:Loopback] failed to build stream for '{device_name}': {e}"); return; }
            };
            if let Err(e) = s.play() {
                error!("[Audio:Loopback] failed to start stream for '{device_name}': {e}");
                return;
            }
            s
        };
        // Keep the stream alive until the connection is torn down, then drop it so the source is
        // released and a reconnect can rebind to a different device.
        while !shutdown.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        drop(_stream);
        info!("[Audio:Loopback] capture stopped");
    });
}

#[cfg(target_os = "linux")]
fn pw_capture_loop(
    source_name: &str,
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    shutdown: Arc<AtomicBool>,
) -> Result<(), pipewire::Error> {
    use pipewire as pw;
    use pw::spa::{
        param::{audio::{AudioFormat, AudioInfoRaw}, ParamType},
        pod::{Object, Pod, Value, serialize::PodSerializer},
        utils::{Direction, SpaTypes},
    };
    use pw::types::ObjectType;

    use std::cell::RefCell;
    use std::rc::Rc;

    // node.name we give our own capture stream, so the registry listener can recognise it.
    const STREAM_NODE_NAME: &str = "mumbled-flight-loopback";

    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context  = pw::context::ContextRc::new(&mainloop, None)?;
    let core     = context.connect_rc(None)?;

    // A "MumblingRadio.monitor" source name targets the "MumblingRadio" sink node: in native
    // PipeWire a sink's monitor ports are simply its output ports, so capturing a sink monitor and
    // capturing a real source are the same operation — link the target node's *output* ports to
    // our stream's input port. The ".monitor" suffix is only a naming convention; we strip it to
    // recover the node name.
    let target = source_name.strip_suffix(".monitor").unwrap_or(source_name).to_string();

    // We do NOT use AUTOCONNECT. WirePlumber's standard linking policy overrides the connect
    // target for capture streams (and especially capture-sink streams), routing them to the
    // *default* device regardless of the target id or `target.object` hint — capturing the
    // headphone-output monitor instead of the selected radio source. (This is the loopback "wrong
    // input stream" bug.) Instead we resolve the graph from the registry and create the links
    // ourselves via the link-factory, exactly like `pw-link`, which no policy can override.

    /// Graph state assembled from the registry, then used to wire the links.
    #[derive(Default)]
    struct Graph {
        target_node: Option<u32>,           // sink/source node we capture from
        our_node: Option<u32>,              // our capture stream's node
        // (owning node id, port global id, port channel index, is_output). The channel index
        // (`port.id`) gives the per-node channel order (0,1,…) used for positional matching, since
        // a null-sink's monitor ports carry no channel map (audio.channel = "UNK").
        ports: Vec<(u32, u32, u32, bool)>,
        // Global ids of our input ports already linked, so each is wired exactly once.
        linked_inputs: Vec<u32>,
    }
    let graph = Rc::new(RefCell::new(Graph::default()));
    // Created Link proxies must be kept alive — dropping a proxy tears the link down.
    let links: Rc<RefCell<Vec<pw::link::Link>>> = Rc::new(RefCell::new(Vec::new()));

    // Link the target node's output ports → our stream's input ports as they appear in the
    // registry. Each of our input ports is wired to the source output port with the same channel
    // index (a mono source clamps to its single port, feeding every input channel). Per-input-port
    // and idempotent — NOT a one-shot latch: our two DSP input ports (input_FL/_FR) are registered
    // in separate events, so a single-shot link would wire only the first channel. The two
    // channels are averaged to mono in the process callback below — matching the CPAL downmix
    // convention (sum / n), so a centred mono radio signal stays at unity gain instead of +6 dB.
    let attempt_link: Rc<dyn Fn()> = {
        let graph = graph.clone();
        let links = links.clone();
        let core = core.clone();
        Rc::new(move || {
            let mut g = graph.borrow_mut();
            let (Some(tnode), Some(onode)) = (g.target_node, g.our_node) else { return };
            // Source output ports sorted by channel index → outs[i] is channel i.
            let mut outs: Vec<(u32, u32)> = g.ports.iter()
                .filter(|&&(n, _, _, out)| n == tnode && out)
                .map(|&(_, gid, idx, _)| (idx, gid))
                .collect();
            outs.sort_unstable();
            if outs.is_empty() { return; }

            // Our input ports, with their channel index, that aren't linked yet.
            let pending: Vec<(u32, u32)> = g.ports.iter()
                .filter(|&&(n, gid, _, out)| n == onode && !out && !g.linked_inputs.contains(&gid))
                .map(|&(_, gid, idx, _)| (idx, gid))
                .collect();

            for (in_idx, dst) in pending {
                let (_, src) = outs[(in_idx as usize).min(outs.len() - 1)];
                let link_props = pw::properties::properties! {
                    *pw::keys::LINK_OUTPUT_NODE => tnode.to_string(),
                    *pw::keys::LINK_OUTPUT_PORT => src.to_string(),
                    *pw::keys::LINK_INPUT_NODE  => onode.to_string(),
                    *pw::keys::LINK_INPUT_PORT  => dst.to_string(),
                    *pw::keys::OBJECT_LINGER    => "false",
                };
                match core.create_object::<pw::link::Link>("link-factory", &link_props) {
                    Ok(l) => {
                        links.borrow_mut().push(l);
                        g.linked_inputs.push(dst);
                        info!("[Audio:Loopback] linked {tnode}:{src} → {onode}:{dst} (ch {in_idx})");
                    }
                    Err(e) => warn!("[Audio:Loopback] link {tnode}:{src} → {onode}:{dst} failed: {e}"),
                }
            }
        })
    };

    // Registry listener (alive for the whole loop): discovers the target node, our own stream
    // node, and every port, retrying the link on each change so ordering of globals doesn't matter.
    let registry = core.get_registry()?;
    let _reg = {
        let graph = graph.clone();
        let attempt = attempt_link.clone();
        let want = target.clone();
        registry
            .add_listener_local()
            .global(move |global| {
                let Some(props) = global.props else { return };
                match global.type_ {
                    ObjectType::Node => {
                        match props.get(*pw::keys::NODE_NAME) {
                            Some(n) if n == want => {
                                let mut g = graph.borrow_mut();
                                g.target_node.get_or_insert(global.id);
                            }
                            Some(STREAM_NODE_NAME) => {
                                let mut g = graph.borrow_mut();
                                g.our_node.get_or_insert(global.id);
                            }
                            _ => return,
                        }
                    }
                    ObjectType::Port => {
                        let Some(node) = props.get(*pw::keys::NODE_ID)
                            .and_then(|s| s.parse::<u32>().ok()) else { return };
                        // port.id is the per-node channel index (0,1,…); default to 0 if absent.
                        let index = props.get(*pw::keys::PORT_ID)
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(0);
                        let is_output = props.get(*pw::keys::PORT_DIRECTION) == Some("out");
                        graph.borrow_mut().ports.push((node, global.id, index, is_output));
                    }
                    _ => return,
                }
                attempt();
            })
            .register()
    };

    // Build the capture stream. NODE_NAME lets the registry listener above recognise our own node.
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE     => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE     => "Production",
        *pw::keys::NODE_NAME      => STREAM_NODE_NAME,
    };

    let stream = pw::stream::StreamBox::new(&core, STREAM_NODE_NAME, props)?;

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
                // Interleaved stereo F32LE → mono by averaging L/R (sum / 2), matching the
                // CPAL downmix convention so a centred mono signal stays at unity gain.
                let samples: Vec<f32> = raw[..end]
                    .chunks_exact(8)
                    .map(|b| {
                        let l = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                        let r = f32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                        (l + r) * 0.5
                    })
                    .collect();
                let _ = tx.try_send(samples);
            }
        })
        .register()?;

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(48000);
    audio_info.set_channels(2);
    let obj = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id:    ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    // Serialising a fixed AudioInfoRaw into a Cursor<Vec> is infallible.
    let values: Vec<u8> = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(obj),
    ).expect("SPA pod serialize").0.into_inner();
    let mut params = [Pod::from_bytes(&values).expect("SPA pod deserialize")];

    // No AUTOCONNECT: the stream node + its input port are still created and exported, but the
    // session manager never links them. `attempt_link` creates the links from the registry data.
    stream.connect(
        Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    info!("[Audio:Loopback] PipeWire capture starting: target '{target}' (manual link)");

    // Shutdown timer: poll the connection's flag and quit the mainloop when it is set, so the
    // stream + links are dropped on disconnect rather than leaking across reconnects (this is what
    // lets a changed radio source rebind on the next connect).
    let timer = mainloop.loop_().add_timer({
        let ml = mainloop.clone();
        move |_| {
            if shutdown.load(Ordering::Acquire) {
                ml.quit();
            }
        }
    });
    let tick = std::time::Duration::from_millis(200);
    if let Err(e) = timer.update_timer(Some(tick), Some(tick)).into_result() {
        error!("[Audio:Loopback] failed to arm shutdown timer: {e:?}");
    }

    // One-shot sanity check: after the initial registry dump has been delivered, warn if the
    // target node was never found, so a vanished/misnamed source is not a silent no-audio failure.
    {
        let graph = graph.clone();
        let target = target.clone();
        let warned = std::cell::Cell::new(false);
        let pending = core.sync(0)?;
        let _done = core
            .add_listener_local()
            .done(move |_id, seq| {
                if seq == pending && !warned.replace(true) && graph.borrow().target_node.is_none() {
                    warn!(
                        "[Audio:Loopback] target node '{target}' not present in the PipeWire \
                         registry — radio capture will be silent until it appears"
                    );
                }
            })
            .register();
        mainloop.run();
    }
    Ok(())
}

pub fn start_capture(
    tx: mpsc::Sender<Vec<f32>>,
    denoise: bool,
    mic_gain: Arc<AtomicU32>,
    _gate_threshold: f32,
    device_name_filter: Option<String>,
    shutdown: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        // Hold the lock across device selection → build_input_stream → play() to
        // serialise concurrent opens (on Linux also guards the PIPEWIRE_NODE write).
        let _stream = {
            let _guard = device_open_lock().lock().unwrap();
            let device = match select_device(device_name_filter.as_deref(), true) {
                Some(d) => d,
                None => { error!("[Audio:Capture] no input device available"); return; }
            };
            let device_name = device.name().unwrap_or_else(|_| "(unknown)".into());
            let config = match device.supported_input_configs() {
                Ok(mut cfgs) => cfgs
                    .find(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000),
                Err(e) => { error!("[Audio:Capture] failed to query configs for '{device_name}': {e}"); return; }
            };
            let config = match config {
                Some(c) => c.with_sample_rate(cpal::SampleRate(48000)),
                None => { error!("[Audio:Capture] '{device_name}' does not support 48 kHz — microphone unavailable"); return; }
            };
            let num_channels = config.channels() as usize;
            info!("[Audio:Capture] Active: {device_name} (48kHz, {num_channels} ch, denoise={denoise})");
            // Downmixed + gained mono samples awaiting processing.
            let mut capture_buffer: Vec<f32> = Vec::new();
            // Post-denoise samples awaiting batching into 960-sample (20 ms) frames.
            let mut output_buffer: Vec<f32> = Vec::new();
            // RNNoise keeps internal state across frames, so the denoiser is created once
            // and reused. None when denoise is disabled. Operates on 480-sample (10 ms) frames.
            let mut denoiser: Option<Box<DenoiseState>> = if denoise {
                Some(DenoiseState::new())
            } else {
                None
            };
            let s = match device.build_input_stream(
                &config.into(),
                move |data: &[f32], _| {
                    let gain = f32::from_bits(mic_gain.load(Ordering::Relaxed));
                    // Downmix to mono without gain so RNNoise sees a normalised signal.
                    if num_channels > 1 {
                        for chunk in data.chunks_exact(num_channels) {
                            let sum: f32 = chunk.iter().sum();
                            capture_buffer.push(sum / num_channels as f32);
                        }
                    } else {
                        capture_buffer.extend_from_slice(data);
                    }

                    if let Some(d) = denoiser.as_mut() {
                        // RNNoise processes fixed 480-sample frames scaled to the i16 range.
                        // Stack scratch buffers — no heap allocation on the audio callback thread.
                        // Gain is applied after denoising so the denoiser sees normalised levels.
                        // Below VAD_THRESHOLD, scale by vad/threshold rather than hard-zeroing:
                        // a binary step to zero creates an audible click at speech boundaries.
                        const FRAME: usize = DenoiseState::FRAME_SIZE;
                        let mut scaled = [0.0f32; FRAME];
                        let mut out = [0.0f32; FRAME];
                        while capture_buffer.len() >= FRAME {
                            for (dst, src) in scaled.iter_mut().zip(capture_buffer.drain(..FRAME)) {
                                *dst = src * 32768.0;
                            }
                            let vad = d.process_frame(&mut out, &scaled);
                            let scale = if vad >= VAD_THRESHOLD { gain } else { gain * (vad / VAD_THRESHOLD) };
                            output_buffer.extend(out.iter().map(|s| s / 32768.0 * scale));
                        }
                    } else {
                        // Apply gain in-place before the O(1) append so the hot path
                        // (gain == 1.0, the default) avoids per-sample work entirely.
                        if gain != 1.0 {
                            for s in &mut capture_buffer { *s *= gain; }
                        }
                        output_buffer.append(&mut capture_buffer);
                    }

                    while output_buffer.len() >= OPUS_FRAME_SAMPLES {
                        let frame: Vec<f32> = output_buffer.drain(..OPUS_FRAME_SAMPLES).collect();
                        let _ = tx.try_send(frame);
                    }
                },
                |err| error!("[Audio:Capture] stream error: {err}"),
                None,
            ) {
                Ok(s) => s,
                Err(e) => { error!("[Audio:Capture] failed to build input stream for '{device_name}': {e}"); return; }
            };
            if let Err(e) = s.play() {
                error!("[Audio:Capture] failed to start stream for '{device_name}': {e}");
                return;
            }
            s
        }; // device_open_lock released — stream is already connected
        // Keep the stream alive until the connection is torn down, then drop it so the
        // CPAL/PipeWire input stream is released instead of leaking across reconnects.
        while !shutdown.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        drop(_stream);
        info!("[Audio:Capture] stream stopped");
    });
}

pub fn start_playback(mut rx: mpsc::Receiver<Vec<f32>>, preferred_device: Option<String>, vol: Arc<AtomicU32>) {
    // Build and run entirely inside one thread — CPAL Stream is !Send on ALSA,
    // so creating and dropping it in the same thread avoids any Send requirement.
    std::thread::spawn(move || {
        let pending_samples    = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let pending_samples_cb = Arc::clone(&pending_samples);

        let (stream, device_channels) = {
            let _guard = device_open_lock().lock().unwrap();
            let device = match select_device(preferred_device.as_deref(), false) {
                Some(d) => d,
                None => { error!("[Audio:Out] no output device available"); return; }
            };
            let device_name = device.name().unwrap_or_else(|_| "(unknown)".into());

            let config = match device.supported_output_configs() {
                Ok(cfgs) => {
                    let preferred = cfgs
                        .filter(|c| c.min_sample_rate().0 <= 48000 && c.max_sample_rate().0 >= 48000)
                        .find(|c| c.channels() == 2);
                    match preferred {
                        Some(c) => c,
                        None => match device.supported_output_configs().ok().and_then(|mut it| it.next()) {
                            Some(c) => c,
                            None => { error!("[Audio:Out] no usable output config for '{device_name}'"); return; }
                        }
                    }
                }
                Err(e) => { error!("[Audio:Out] failed to query configs for '{device_name}': {e}"); return; }
            }.with_sample_rate(cpal::SampleRate(48000));

            let device_channels = config.channels() as usize;
            info!("[Audio:Out] Sink: {device_name} (48kHz, {device_channels} ch)");

            let s = match device.build_output_stream(
                &config.into(),
                move |data: &mut [f32], _| {
                    let mut lock = pending_samples_cb.lock().unwrap();
                    let n = lock.len().min(data.len());
                    for (dst, src) in data[..n].iter_mut().zip(lock.drain(..n)) {
                        *dst = src;
                    }
                    for x in data[n..].iter_mut() { *x = 0.0; }
                },
                |err| error!("[Audio:Out] stream error: {err}"),
                None,
            ) {
                Ok(s) => s,
                Err(e) => { error!("[Audio:Out] failed to build output stream for '{device_name}': {e}"); return; }
            };
            if let Err(e) = s.play() {
                error!("[Audio:Out] failed to start stream for '{device_name}': {e}");
                return;
            }
            (s, device_channels)
        }; // device_open_lock released — stream is already connected

        let mut last_log = std::time::Instant::now();
        while let Some(mut stereo_frame_48k) = rx.blocking_recv() {
            let v = f32::from_bits(vol.load(std::sync::atomic::Ordering::Relaxed));
            if (v - 1.0).abs() > 1e-4 {
                for s in &mut stereo_frame_48k { *s *= v; }
            }
            let mut lock = pending_samples.lock().unwrap();

            if device_channels == 2 {
                lock.extend(stereo_frame_48k);
            } else {
                for chunk in stereo_frame_48k.chunks_exact(2) {
                    let mono = (chunk[0] + chunk[1]) * 0.5;
                    for _ in 0..device_channels { lock.push_back(mono); }
                }
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
