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


//! log::Log backend that forwards all records to XPLMDebugString → Log.txt.

use std::ffi::CString;
use xplane_sys::XPLMDebugString;
pub static LOGGER: XPlaneLogger = XPlaneLogger;

pub struct XPlaneLogger;

impl log::Log for XPlaneLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Debug
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            xp_log(&format!(
                "[MumbledFlight:{}] {}\n",
                record.level(),
                record.args()
            ));
        }
    }
    fn flush(&self) {}
}

pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}

// ── Raw XPLM log sink (used only by logger::XPlaneLogger) ─────────────────────

pub fn xp_log(s: &str) {
    if let Ok(cs) = CString::new(s) {
        unsafe { XPLMDebugString(cs.as_ptr()) };
    }
}
