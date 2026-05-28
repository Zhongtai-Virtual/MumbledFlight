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
            if line.starts_with("netlink/user_name = ") {
                user_name = Some(line.replace("netlink/user_name = ", "").trim().to_string());
            }
        }

        user_name.map(|u| Config { user_name: u })
    }
}
