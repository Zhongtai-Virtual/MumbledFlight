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


//! Configuration handling for the Hotstart CL60 aircraft.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Holds the session-specific configuration extracted from the aircraft.
pub struct Config {
    /// The user name configured in the aircraft, used as the Mumble Identity.
    pub user_name: String,
}

impl Config {
    /// Reads the `user.cfg` file for the Hotstart CL650 from the provided X-Plane base path.
    pub fn read_cl60_config(base_path: &Path) -> Option<Self> {
        let config_path = base_path.join("Output/CL650/user.cfg");
        
        let file = File::open(config_path).ok()?;
        let reader = BufReader::new(file);
        let mut user_name = None;

        for line in reader.lines().map_while(Result::ok) {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("netlink/user_name = ") {
                user_name = Some(value.trim().to_string());
            }
        }

        user_name.map(|u| Config { user_name: u })
    }
}
