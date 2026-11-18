//! The release version every component reports. The `clouduler` binary sets
//! it at startup from its own `CARGO_PKG_VERSION`, so only that crate's
//! version has to change per release.

use std::sync::OnceLock;

static VERSION: OnceLock<&'static str> = OnceLock::new();

/// Records the version to report. The first call wins.
pub fn set_version(v: &'static str) {
    let _ = VERSION.set(v);
}

/// The reported version; falls back to this crate's own version when
/// `set_version` was never called (tests).
pub fn version() -> &'static str {
    VERSION.get().copied().unwrap_or(env!("CARGO_PKG_VERSION"))
}
