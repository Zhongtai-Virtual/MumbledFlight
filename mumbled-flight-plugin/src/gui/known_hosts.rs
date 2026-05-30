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

impl KnownHosts {
    /// Canonical store key for a server.
    pub fn key(host: &str, port: u16) -> String {
        format!("{}:{}", host.trim(), port)
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
