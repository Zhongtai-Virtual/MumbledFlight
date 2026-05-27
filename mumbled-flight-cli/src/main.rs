//! MumblingCockpit: A high-fidelity bridge between X-Plane 12 and Mumble.

mod xplane;

use anyhow::Result;
use clap::Parser;
use log::{info, error};
use std::sync::{Arc, Mutex};
use std::net::SocketAddr;
use std::path::PathBuf;
use mumbled_flight_core::state::CockpitState;
use mumbled_flight_core::mumble;
use mumbled_flight_core::config::Config;

#[derive(Parser, Debug)]
#[command(
    author = "MumblingCockpit Team",
    version,
    about = "High-fidelity shared cockpit audio bridge for X-Plane 12 and Mumble.",
    long_about = "A standalone utility that bridges X-Plane 12 cockpit states with Mumble VoIP."
)]
struct Args {
    /// The Flight ID or Session Name. Partners must use the same ID to meet.
    #[arg(index = 1)]
    flight_id: Option<String>,

    /// Path to X-Plane 12 root folder. Used to extract your pilot name automatically.
    #[arg(short, long, value_name = "PATH")]
    xplane_path: Option<PathBuf>,

    /// Microphone gain multiplier. Acts as a pre-amp before filtering. (e.g. 1.5)
    #[arg(short, long, default_value_t = 1.0, value_name = "FACTOR")]
    gain: f32,

    /// Custom username prefix. (Required if --xplane-path is missing).
    #[arg(short, long, value_name = "NAME")]
    user: Option<String>,

    /// Enable the WebRTC Audio Processing Suite (Noise Suppression, HPF, AGC).
    #[arg(short, long, default_value_t = false)]
    denoise: bool,

    /// Loopback device for X-Pilot radio audio.
    #[arg(short, long, value_name = "DEVICE")]
    radio_source: Option<String>,

    /// Linux only: Automatically create a virtual 'MumblingRadio' loopback device.
    #[arg(long, default_value_t = false)]
    auto_sink: bool,

    /// List all available audio input and output devices on your system.
    #[arg(long, default_value_t = false)]
    list_devices: bool,

    /// Debugging: Only spawn a single Ambient client (no Intercom or Radio).
    #[arg(long, default_value_t = false)]
    single_client: bool,

    /// Test Mode: transmit from a fixed static position in Mumble space (X,Y,Z meters).
    /// Use without a value for the origin (0,0,0), or supply coordinates: --test-mode 2,0,7
    #[arg(long, value_name = "X,Y,Z", num_args = 0..=1, default_missing_value = "0,0,0")]
    test_mode: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    // 1. Device Discovery Mode
    if args.list_devices {
        mumble::audio::list_audio_devices();
        return Ok(());
    }

    // Ensure we have a flight ID for normal operation
    let flight_id = args.flight_id.expect("Error: Flight ID is required. Use --help for usage details.");

    info!("MumblingCockpit Standalone Bridge starting...");

    // 2. Initialize shared state
    let state = Arc::new(Mutex::new(CockpitState::default()));
    let server_addr: SocketAddr = "127.0.0.1:64738".parse()?;
    
    // 3. Resolve Identity
    let user_prefix = if let Some(u) = args.user {
        u
    } else if let Some(ref path) = args.xplane_path {
        let cfg = Config::read_cl60_config(path)
            .expect("Critical: Found X-Plane path but could not read CL650 configuration.");
        cfg.user_name
    } else {
        error!("Username could not be determined.");
        error!("Please provide either --user <Name> or --xplane-path <Path>.");
        std::process::exit(1);
    };

    info!("Session ready — user: {}, flight: {}, denoise: {}",
        user_prefix, flight_id, args.denoise);

    // 4. Start Mumble Stack
    let test_pos: Option<[f32; 3]> = args.test_mode.map(|s| {
        let p: Vec<f32> = s.split(',').map(|c| c.trim().parse().unwrap_or(0.0)).collect();
        [p.first().copied().unwrap_or(0.0), p.get(1).copied().unwrap_or(0.0), p.get(2).copied().unwrap_or(0.0)]
    });
    let state_mumble = Arc::clone(&state);
    tokio::spawn(async move {
        mumble::run_mumble_stack(
            state_mumble,
            user_prefix,
            flight_id,
            args.gain,
            args.denoise,
            args.radio_source,
            args.auto_sink,
            args.single_client,
            test_pos,
            server_addr,
            None,
        ).await;
    });

    // 5. Run the X-Plane WebAPI bridge (Main Thread)
    xplane::bridge::run_bridge_forever(state).await
}
