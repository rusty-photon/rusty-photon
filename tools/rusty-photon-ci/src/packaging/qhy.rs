//! The QHY SDK's pins.
//!
//! The same release is linked on every OS, and qhy-camera reports it as the
//! ABI it was built against (`PINNED_SDK_VERSION` in
//! `services/qhy-camera/src/preflight.rs`), which the packaging guard
//! checks. The packaging jobs' SDK caches are keyed on this file, so it
//! holds the pins and nothing else that changes.

/// QHYCCD's release, `YY.MM.DD`.
pub const VERSION: &str = "26.06.04";

/// The SHA-256 of the macOS arm64 archive.
pub const SHA256_MAC_ARM64: &str =
    "3a03cef52ac95a513e4e7700058352d95c2f0556b02ab15e13efe313168f40a5";

/// The SHA-256 of the Linux `x86_64` archive. The firmware helper the
/// qhy-camera package ships downloads the same archive on the target.
pub const SHA256_LINUX_X86_64: &str =
    "cbfcec159809e6984c5013a587fed88c892afae9d834019e820213f64616a308";

/// The SHA-256 of the Linux `aarch64` archive, which the firmware helper
/// downloads too.
pub const SHA256_LINUX_AARCH64: &str =
    "d28795977311fba1cb7a4fdc48bbd6d5f994716674b2154d9a860d1fdf2f5e0e";

/// The macOS arm64 archive, as QHYCCD names it; it unpacks to a directory
/// of the same name, as the Linux ones do.
#[must_use]
pub fn mac_archive() -> String {
    format!("sdk_mac_arm_{VERSION}.tar.gz")
}

/// The Linux `x86_64` archive, as QHYCCD names it.
#[must_use]
pub fn linux_x86_64_archive() -> String {
    format!("sdk_linux64_{VERSION}.tar.gz")
}

/// The Linux `aarch64` archive, as QHYCCD names it.
#[must_use]
pub fn linux_aarch64_archive() -> String {
    format!("sdk_linux_arm64_{VERSION}.tar.gz")
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
