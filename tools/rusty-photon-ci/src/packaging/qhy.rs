//! The QHY SDK's pins.
//!
//! The same release is linked on every OS, and qhy-camera reports it as the
//! ABI it was built against (`PINNED_SDK_VERSION` in
//! `services/qhy-camera/src/preflight.rs`), which the packaging guard
//! checks. The macOS jobs' SDK cache is keyed on this file, so it holds the
//! pins and nothing else that changes.

/// QHYCCD's release, `YY.MM.DD`.
pub const VERSION: &str = "26.06.04";

/// The SHA-256 of the macOS arm64 archive.
pub const SHA256_MAC_ARM64: &str =
    "3a03cef52ac95a513e4e7700058352d95c2f0556b02ab15e13efe313168f40a5";

/// The macOS arm64 archive, as QHYCCD names it; it unpacks to a directory
/// of the same name.
#[must_use]
pub fn mac_archive() -> String {
    format!("sdk_mac_arm_{VERSION}.tar.gz")
}

/// Where QHYCCD publishes `file`: from 26.06.04 on, in a directory named
/// after the version without its dots.
#[must_use]
pub fn url(file: &str) -> String {
    format!(
        "https://www.qhyccd.com/file/repository/publish/SDK/{}/{file}",
        VERSION.replace('.', "")
    )
}
