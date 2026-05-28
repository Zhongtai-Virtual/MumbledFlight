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
