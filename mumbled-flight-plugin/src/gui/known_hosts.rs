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

//! Trust-On-First-Use store for Mumble server certificates (SSH `known_hosts` analogue).
//!
//! Maps a `host:port` key to the server's pinned certificate (PEM). When no explicit Server CA
//! is configured, the connect flow probes the server, asks the user to trust the fingerprint,
//! and records the cert here; subsequent connections pin against it and a changed certificate is
//! detected. Persisted next to `config.toml` as `known_hosts.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use log::warn;

pub struct KnownHosts {
    path: PathBuf,
    /// `host:port` → server certificate in PEM form.
    entries: HashMap<String, String>,
}

/// Lightweight snapshot used to look up entries inside an imgui build closure where `self` is
/// mutably borrowed by the renderer.
pub(super) type KnownHostsSnapshot = HashMap<String, String>;

impl KnownHosts {
    /// Canonical store key for a server.
    ///
    /// IPv6 addresses are normalised to `[addr]:port` regardless of whether the caller
    /// already included brackets, so lookups are consistent with how addresses are typed.
    pub fn key(host: &str, port: u16) -> String {
        let h = host.trim().trim_matches(|c| c == '[' || c == ']');
        if h.contains(':') {
            format!("[{h}]:{port}")
        } else {
            format!("{h}:{port}")
        }
    }

    /// Loads the store sitting next to `config_path`. A missing or malformed file yields an
    /// empty store (TOFU simply treats every server as new).
    pub fn load(config_path: &Path) -> Self {
        let path = config_path.with_file_name("known_hosts.toml");
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str::<HashMap<String, String>>(&s).ok())
            .unwrap_or_default();
        Self { path, entries }
    }

    /// The pinned PEM for `key`, if this server has been trusted before.
    pub fn get(&self, key: &str) -> Option<&String> {
        self.entries.get(key)
    }

    /// Returns a shallow clone of the entries for use inside closures that cannot borrow `self`.
    pub(super) fn snapshot(&self) -> KnownHostsSnapshot {
        self.entries.clone()
    }

    /// Records `pem` as the trusted certificate for `key` and persists the store.
    pub fn insert_and_save(&mut self, key: String, pem: String) {
        self.entries.insert(key, pem);
        match toml::to_string(&self.entries) {
            Ok(s) => {
                if let Err(e) = std::fs::write(&self.path, s) {
                    warn!("failed to write {}: {e}", self.path.display());
                }
            }
            Err(e) => warn!("failed to serialize known_hosts: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ipv4_and_hostname() {
        assert_eq!(KnownHosts::key("192.168.1.1", 64738), "192.168.1.1:64738");
        assert_eq!(KnownHosts::key("mumble.example.com", 64738), "mumble.example.com:64738");
        assert_eq!(KnownHosts::key("  mumble.example.com  ", 64738), "mumble.example.com:64738");
    }

    #[test]
    fn key_ipv6_brackets_normalised() {
        assert_eq!(KnownHosts::key("::1", 64738), "[::1]:64738");
        assert_eq!(KnownHosts::key("2001:db8::1", 64738), "[2001:db8::1]:64738");
        assert_eq!(KnownHosts::key("[::1]", 64738), "[::1]:64738");
        assert_eq!(KnownHosts::key("  [::1]  ", 64738), "[::1]:64738");
    }

    #[test]
    fn round_trip_load_insert_save_get() {
        let dir = std::env::temp_dir();
        let config_path = dir.join(format!("mf_test_{}.toml", std::process::id()));
        let _ = std::fs::remove_file(config_path.with_file_name("known_hosts.toml"));

        let mut kh = KnownHosts::load(&config_path);
        assert!(kh.get("mumble.example.com:64738").is_none());

        kh.insert_and_save("mumble.example.com:64738".to_string(), "FAKEPEM".to_string());

        let kh2 = KnownHosts::load(&config_path);
        assert_eq!(kh2.get("mumble.example.com:64738").map(|s| s.as_str()), Some("FAKEPEM"));

        let _ = std::fs::remove_file(config_path.with_file_name("known_hosts.toml"));
    }

    #[test]
    fn missing_or_corrupt_file_yields_empty_store() {
        let dir = std::env::temp_dir();
        let config_path = dir.join(format!("mf_nofile_{}.toml", std::process::id()));
        let kh = KnownHosts::load(&config_path);
        assert!(kh.get("anything").is_none());

        let hosts_path = config_path.with_file_name(
            format!("mf_corrupt_{}_known_hosts.toml", std::process::id()),
        );
        std::fs::write(&hosts_path, b"not valid toml [[[").unwrap();
        let corrupt_config =
            hosts_path.with_file_name(format!("mf_corrupt_{}.toml", std::process::id()));
        let kh2 = KnownHosts::load(&corrupt_config);
        assert!(kh2.get("anything").is_none());
        let _ = std::fs::remove_file(&hosts_path);
    }
}
