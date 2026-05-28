//! Audio device enumeration — CPAL fallback for non-Linux platforms.
//! On Linux, device enumeration is handled by enumerate_pw_devices() in the background thread.

#[cfg(not(target_os = "linux"))]
use cpal::traits::{DeviceTrait, HostTrait};

/// Returns `(names, labels)` where index 0 is the system-default sentinel (empty name).
#[cfg(not(target_os = "linux"))]
pub fn enumerate_output_devices() -> (Vec<String>, Vec<String>) {
    let mut names  = vec![String::new()];
    let mut labels = vec!["(system default)".to_string()];
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

/// Returns input device names for use as radio relay sources.
#[cfg(not(target_os = "linux"))]
pub fn enumerate_input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}
