//! OS secret-store integration for sensitive fields (the Mumble server password and the
//! client-certificate passphrase). Keeps them out of the plain-text `config.toml`.
//!
//! Backed by the `keyring` crate: on Linux this is the freedesktop **Secret Service** D-Bus API
//! (GNOME Keyring, KWallet, KeePassXC, …) via the pure-Rust zbus backend; macOS uses the
//! Keychain and Windows the Credential Manager. All calls are best-effort — if the platform
//! secret store is unavailable we log and continue, never falling back to plain-text storage.

use keyring::Entry;
use log::warn;

const SERVICE: &str = "MumbledFlight";

/// Stable keys for the two stored secrets.
pub const SERVER_PASSWORD: &str = "mumble-server-password";
pub const CERT_PASSPHRASE: &str = "client-cert-passphrase";

/// Reads a secret. Returns an empty string when absent or the store is unavailable.
pub fn load(key: &str) -> String {
    match Entry::new(SERVICE, key).and_then(|e| e.get_password()) {
        Ok(secret) => secret,
        Err(keyring::Error::NoEntry) => String::new(),
        Err(e) => {
            warn!("[secrets] could not read '{key}' from the OS secret store: {e}");
            String::new()
        }
    }
}

/// Stores a secret, or deletes it when `value` is empty.
pub fn store(key: &str, value: &str) {
    let entry = match Entry::new(SERVICE, key) {
        Ok(e) => e,
        Err(e) => {
            warn!("[secrets] OS secret store unavailable for '{key}': {e}");
            return;
        }
    };
    let result = if value.is_empty() {
        match entry.delete_credential() {
            Err(keyring::Error::NoEntry) => Ok(()),
            other => other,
        }
    } else {
        entry.set_password(value)
    };
    if let Err(e) = result {
        warn!("[secrets] could not store '{key}' in the OS secret store: {e}");
    }
}
