//! Persisted plugin configuration — serialized to TOML.

use log::warn;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct PluginConfig {
    pub server: String,
    pub flight_id: String,
    pub user_name: String,
    /// Microphone input gain multiplier (applies to captured audio only, not playback).
    pub gain: f32,
    pub denoise: bool,
    pub output_device: String,
    pub log_level: String,
    /// "" = disabled, "__auto__" = MumblingRadio auto-sink, anything else = device name.
    pub radio_source: String,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            server: "127.0.0.1:64738".to_string(),
            flight_id: String::new(),
            user_name: String::new(),
            gain: 1.0,
            denoise: false,
            output_device: String::new(),
            log_level: "info".to_string(),
            radio_source: String::new(),
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
