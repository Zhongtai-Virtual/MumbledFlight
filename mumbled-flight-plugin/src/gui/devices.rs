//! Output device enumeration — ALSA via CPAL plus PipeWire/PulseAudio via pactl.

use cpal::traits::{DeviceTrait, HostTrait};
use std::collections::HashMap;

/// Returns `(names, labels)` where `names` are used for routing and `labels` for display.
/// On Linux, PipeWire/PulseAudio sinks invisible to ALSA (e.g. Bluetooth) are appended.
pub fn enumerate_output_devices() -> (Vec<String>, Vec<String>) {
    let mut names: Vec<String> = cpal::default_host()
        .output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();

    let descriptions = pactl_sink_descriptions();

    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("pactl").args(["list", "short", "sinks"]).output() {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            for sink in s.lines().filter_map(|l| l.split_whitespace().nth(1)) {
                if !names.iter().any(|d| d == sink) {
                    names.push(sink.to_string());
                }
            }
        }
    }

    let labels = names.iter()
        .map(|name| descriptions.get(name).cloned().unwrap_or_else(|| name.clone()))
        .collect();

    (names, labels)
}

fn pactl_sink_descriptions() -> HashMap<String, String> {
    #[cfg(not(target_os = "linux"))]
    return HashMap::new();

    #[cfg(target_os = "linux")]
    {
        let mut map = HashMap::new();
        let Ok(out) = std::process::Command::new("pactl").args(["list", "sinks"]).output() else {
            return map;
        };
        let Ok(s) = std::str::from_utf8(&out.stdout) else { return map };
        let mut current_name: Option<String> = None;
        for line in s.lines() {
            let line = line.trim();
            if let Some(name) = line.strip_prefix("Name: ") {
                current_name = Some(name.to_string());
            } else if let Some(desc) = line.strip_prefix("Description: ") {
                if let Some(name) = current_name.take() {
                    map.insert(name, desc.to_string());
                }
            }
        }
        map
    }
}
