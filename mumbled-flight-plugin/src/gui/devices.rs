//! Audio device enumeration — output (pactl on Linux, CPAL elsewhere) and input (CPAL).

use cpal::traits::{DeviceTrait, HostTrait};
use std::collections::HashMap;

/// Returns `(names, labels)` where `names` are used for routing and `labels` for display.
/// On Linux uses `pactl list short sinks` exclusively — avoids ALSA's process-level device cache.
pub fn enumerate_output_devices() -> (Vec<String>, Vec<String>) {
    let descriptions = pactl_sink_descriptions();

    // Index 0 is always the "system default" sentinel — empty name means pass None to playback.
    let mut names  = vec![String::new()];
    let mut labels = vec!["(system default)".to_string()];

    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("pactl").args(["list", "short", "sinks"]).output() {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            for sink in s.lines().filter_map(|l| l.split_whitespace().nth(1))
                .filter(|name| *name != "MumblingRadio")
            {
                let label = descriptions.get(sink).cloned().unwrap_or_else(|| sink.to_string());
                names.push(sink.to_string());
                labels.push(label);
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    for name in cpal::default_host()
        .output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect::<Vec<_>>())
        .unwrap_or_default()
    {
        let label = descriptions.get(&name).cloned().unwrap_or_else(|| name.clone());
        names.push(name);
        labels.push(label);
    }

    (names, labels)
}

/// Returns PipeWire source names for use as radio relay sources.
/// Monitor sources (loopback captures) are excluded — only real inputs are listed.
pub fn enumerate_input_devices() -> Vec<String> {
    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("pactl").args(["list", "short", "sources"]).output() {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            return s.lines()
                .filter_map(|l| l.split_whitespace().nth(1))
                .filter(|name| !name.ends_with(".monitor"))
                .map(|s| s.to_string())
                .collect();
        }
    }
    // Non-Linux fallback.
    cpal::default_host()
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
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
