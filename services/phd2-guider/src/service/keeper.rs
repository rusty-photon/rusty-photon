//! What keeps PHD2 running on a host switched over to PHD2 under the
//! service manager.
//!
//! A restart of PHD2 (`docs/services/phd2-guider.md` § "PHD2 under the
//! service manager") asks the keeper whether it starts PHD2 again, which
//! process it knows as PHD2, and how long that process has run, and has
//! it end that process.
//!
//! On Linux the keeper is systemd's `rusty-photon-phd2` unit
//! ([`systemd::Systemd`]). This build has no keeper elsewhere, so a restart
//! is refused there.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

#[cfg(target_os = "linux")]
pub mod systemd;

/// One process, told apart from a later one the OS gives the same id: its
/// pid and the moment it started, in the keeper's own units (clock ticks
/// since boot on Linux).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessId {
    pub pid: u32,
    pub started: u64,
}

/// The keeper's state, as a restart reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// The keeper's own word for its state (systemd's `ActiveState`), for
    /// messages.
    pub state: String,
    /// The keeper runs PHD2, or is about to start it again: a PHD2 it runs
    /// that exits is started again.
    pub restarts: bool,
    /// The keeper has stopped keeping PHD2: nothing starts PHD2 again.
    pub stopped: bool,
    /// The process the keeper knows as PHD2, while one runs.
    pub main: Option<ProcessId>,
}

/// The thing that keeps PHD2 running.
#[async_trait]
pub trait Keeper: Send + Sync {
    /// The keeper's name in messages (`rusty-photon-phd2`).
    fn name(&self) -> &'static str;

    /// Read the keeper's state.
    async fn state(&self) -> Result<Kept, String>;

    /// How long `process` has run; `None` once it no longer runs (gone, a
    /// zombie, or its pid now another process's).
    async fn age(&self, process: ProcessId) -> Result<Option<Duration>, String>;

    /// End `process`, and return once it no longer runs. On Linux: `SIGTERM`,
    /// and `SIGKILL` 5 s later. A signal only ever reaches `process` itself,
    /// never a later process given its pid.
    async fn end(&self, process: ProcessId) -> Result<(), String>;
}

/// This host's keeper of PHD2: the `rusty-photon-phd2` unit on Linux, and
/// none elsewhere.
#[must_use]
pub fn system_keeper() -> Option<Arc<dyn Keeper>> {
    #[cfg(target_os = "linux")]
    {
        Some(Arc::new(systemd::Systemd))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Why this host has no keeper, for the refusal's message.
#[must_use]
pub const fn why_no_keeper() -> &'static str {
    if cfg!(target_os = "macos") {
        "nothing keeps PHD2 running on macOS, so nothing would start it again"
    } else {
        "this build keeps PHD2 running only on Linux, so nothing would start it again here"
    }
}
