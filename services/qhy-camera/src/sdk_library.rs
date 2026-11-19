//! Whether the QHYCCD SDK's library loads (`docs/services/qhy-camera.md`
//! PF4–PF6).
//!
//! A server checks it before it opens the SDK. Without it the server serves
//! without the SDK, and a background re-check rebuilds it once it loads.

#[cfg(feature = "simulation")]
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(feature = "simulation")]
use tracing::error;

/// Loads the library, saying why it does not load, and logging PF4's
/// `error!` when it does not and the flag is set. Blocking.
type Load = Arc<dyn Fn(bool) -> Result<(), String> + Send + Sync>;

/// Where this build's QHYCCD SDK library comes from, and how it loads.
#[derive(Clone)]
pub struct SdkLibrary {
    load: Load,
}

impl Default for SdkLibrary {
    fn default() -> Self {
        Self::platform()
    }
}

impl SdkLibrary {
    /// This platform's library. On a Windows real-SDK build it is the
    /// delay-loaded `qhyccd.dll`, which the preflight resolves (PF1–PF3).
    /// Every other build links the SDK in, or never calls it (PF5), so it
    /// always loads.
    #[must_use]
    pub fn platform() -> Self {
        #[cfg(all(windows, not(feature = "simulation")))]
        let load = load_dll;
        #[cfg(not(all(windows, not(feature = "simulation"))))]
        let load = |_announce: bool| -> Result<(), String> { Ok(()) };
        Self {
            load: Arc::new(load),
        }
    }

    /// Test-only: a simulated library that loads once a file exists at
    /// `path` (PF5).
    #[cfg(feature = "simulation")]
    #[must_use]
    pub fn staged(path: PathBuf) -> Self {
        Self {
            load: Arc::new(move |announce| {
                let loaded = load_staged(&path);
                if let (true, Err(reason)) = (announce, &loaded) {
                    error!("{reason}");
                }
                loaded
            }),
        }
    }

    /// Load the library for a server's build, logging the one actionable
    /// `error!` when it does not load (PF4). Blocking.
    ///
    /// # Errors
    ///
    /// Why the library does not load: the reason a placeholder's connect is
    /// refused with.
    pub fn load(&self) -> Result<(), String> {
        (self.load)(true)
    }

    /// Load the library for a try of the background re-check (PF6), logging
    /// nothing: the build's PF4 line has said it does not load. Blocking.
    ///
    /// # Errors
    ///
    /// Why the library does not load.
    pub fn try_load(&self) -> Result<(), String> {
        (self.load)(false)
    }
}

/// Resolve `qhyccd.dll` and keep it resident (PF1–PF3), logging the PF4
/// `error!` when it is nowhere and `announce` is set.
#[cfg(all(windows, not(feature = "simulation")))]
fn load_dll(announce: bool) -> Result<(), String> {
    use crate::preflight::{
        dll_missing_reason, ensure_qhyccd_dll, resolve_and_load, DllResolution, PreflightError,
    };

    let failures = if announce {
        match ensure_qhyccd_dll() {
            Ok(_) => return Ok(()),
            Err(PreflightError::DllNotFound { failures, .. }) => failures,
        }
    } else {
        match resolve_and_load() {
            DllResolution::FoundAt(_) | DllResolution::FoundByName => return Ok(()),
            DllResolution::NotFound { failures, .. } => failures,
        }
    };
    Err(dll_missing_reason(&failures))
}

/// The simulated library loads once a file exists at `path`.
#[cfg(feature = "simulation")]
fn load_staged(path: &Path) -> Result<(), String> {
    if path.exists() {
        Ok(())
    } else {
        Err(format!(
            "the simulated QHYCCD SDK library is not installed: {} does not exist",
            path.display()
        ))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn the_platform_library_always_loads_off_windows() {
        SdkLibrary::platform().load().unwrap();
        SdkLibrary::platform().try_load().unwrap();
    }

    #[cfg(feature = "simulation")]
    #[test]
    fn a_staged_library_loads_only_once_its_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qhyccd-sdk-library");
        let library = SdkLibrary::staged(path.clone());

        let reason = library.load().unwrap_err();
        assert_eq!(
            reason,
            format!(
                "the simulated QHYCCD SDK library is not installed: {} does not exist",
                path.display()
            )
        );
        library.try_load().unwrap_err();

        std::fs::write(&path, b"").unwrap();
        library.load().unwrap();
        library.try_load().unwrap();
    }
}
