//! Audio device enumeration — output (PipeWire registry on Linux, CPAL elsewhere) and input.

#[cfg(not(target_os = "linux"))]
use cpal::traits::{DeviceTrait, HostTrait};
#[cfg(target_os = "linux")]
use mumbled_flight_core::mumble::audio::{enumerate_pw_sinks, enumerate_pw_sources, VIRTUAL_SINK_NAME};

/// Returns `(names, labels)` where `names` are used for routing and `labels` for display.
/// On Linux uses the PipeWire registry directly — avoids spawning external processes.
pub fn enumerate_output_devices() -> (Vec<String>, Vec<String>) {
    // Index 0 is always the "system default" sentinel — empty name means pass None to playback.
    let mut names  = vec![String::new()];
    let mut labels = vec!["(system default)".to_string()];

    #[cfg(target_os = "linux")]
    for sink in enumerate_pw_sinks()
        .into_iter()
        .filter(|s| s.name != VIRTUAL_SINK_NAME)
    {
        names.push(sink.name);
        labels.push(sink.description);
    }

    #[cfg(not(target_os = "linux"))]
    for name in cpal::default_host()
        .output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let label = name.clone();
        names.push(name);
        labels.push(label);
    }

    (names, labels)
}

/// Returns PipeWire source names for use as radio relay sources.
/// Monitor sources are excluded — only real inputs are listed.
pub fn enumerate_input_devices() -> Vec<String> {
    #[cfg(target_os = "linux")]
    return enumerate_pw_sources();

    #[cfg(not(target_os = "linux"))]
    cpal::default_host()
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}
