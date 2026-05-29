//! Persisted plugin configuration — serialized to TOML.

use log::warn;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct PluginConfig {
    pub server: String,
    /// Mumble server password. **Not** persisted here — stored in the OS secret store. The field
    /// is retained only to read (and migrate) legacy plain-text values; it is never written
    /// back (kept empty + skipped on serialize).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub server_password: String,
    /// Optional client certificate (PKCS#12 / .p12) path used instead of password auth.
    pub cert_path: String,
    /// Passphrase for `cert_path`. Stored in the OS secret store, not here (legacy-read only).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub cert_pass: String,
    /// Optional CA / pinned-cert path (PEM/DER) used to verify the server certificate.
    pub server_ca: String,
    pub flight_id: String,
    pub user_name: String,
    /// Microphone input gain multiplier (applies to captured audio only, not playback).
    pub gain: f32,
    pub ambient_vol: f32,
    pub ic_vol: f32,
    pub denoise: bool,
    pub ambient_device: String,
    pub ic_device: String,
    pub mic_device: String,
    pub log_level: String,
    /// "" = disabled, "__auto__" = MumblingRadio auto-sink, anything else = device name.
    pub radio_source: String,
    /// Stereo width for spatialized playback: 0.0 = mono, 1.0 = full spatial.
    pub spatial_width: f32,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            server: "127.0.0.1:64738".to_string(),
            server_password: String::new(),
            cert_path: String::new(),
            cert_pass: String::new(),
            server_ca: String::new(),
            flight_id: String::new(),
            user_name: String::new(),
            gain: 1.0,
            ambient_vol: 1.0,
            ic_vol: 1.0,
            denoise: false,
            ambient_device: String::new(),
            ic_device: String::new(),
            mic_device: String::new(),
            log_level: "info".to_string(),
            radio_source: String::new(),
            spatial_width: 1.0,
        }
    }
}

impl PluginConfig {
    pub fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &PathBuf) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match toml::to_string_pretty(self) {
            Ok(s)  => { let _ = std::fs::write(path, s); }
            Err(e) => warn!("config save failed: {e}"),
        }
    }
}
