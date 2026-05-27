//! MumblingCockpit: A high-fidelity bridge between X-Plane 12 and Mumble.

mod state;
mod config;
mod xplane;
mod mumble;

use anyhow::Result;
use clap::Parser;
use std::sync::{Arc, Mutex};
use std::net::SocketAddr;
use std::path::PathBuf;
use crate::state::CockpitState;
use crate::config::Config;

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 1. Device Discovery Mode
    if args.list_devices {
        mumble::audio::list_audio_devices();
        return Ok(());
    }

    // Ensure we have a flight ID for normal operation
    let flight_id = args.flight_id.expect("Error: Flight ID is required. Use --help for usage details.");

    println!("MumblingCockpit Standalone Bridge starting...");

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
        eprintln!("Error: Username could not be determined.");
        eprintln!("Please provide either --user <Name> or --xplane-path <Path>.");
        std::process::exit(1);
    };

    println!("Session Ready. User: {}, Flight ID: {}, Denoise: {}", 
        user_prefix, flight_id, args.denoise);

    // 4. Start Mumble Stack
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
            server_addr
        ).await;
    });

    // 5. Run the X-Plane WebAPI bridge (Main Thread)
    xplane::bridge::run_bridge_forever(state).await
}
