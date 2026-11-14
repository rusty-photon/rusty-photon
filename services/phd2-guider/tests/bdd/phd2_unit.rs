//! A stand-in for systemd's `rusty-photon-phd2` unit, for the restart
//! scenarios (`tests/features/phd2_restart.feature`).
//!
//! The service reads the unit with `systemctl show`, found on its `PATH`.
//! The stand-in is a `systemctl` script in the scenario's directory that
//! prints the unit's `ActiveState` and `MainPID` from a state file this
//! module keeps. Keeping PHD2 running, it holds the mock PHD2 as the unit's
//! main process and, like `Restart=always` with `RestartSec=2`, starts the
//! mock again on the same port 2 s after it exits, recording how each one
//! ended.

use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The unit's restart delay (`RestartSec=2`).
const RESTART_DELAY: Duration = Duration::from_secs(2);

/// How often the main process is checked for its exit.
const EXIT_POLL: Duration = Duration::from_millis(20);

/// Starts the mock PHD2 again: its port and its process.
pub type Respawn = Box<dyn Fn() -> (u16, Child) + Send>;

#[derive(Debug)]
pub struct Phd2Unit {
    /// Holds the `systemctl` stand-in and the unit's state file.
    dir: PathBuf,
    /// The unit keeps PHD2 running; otherwise it is inactive.
    keeps: bool,
    /// The unit's main process, while one runs.
    main: Arc<Mutex<Option<Child>>>,
    /// How each main process ended, in order.
    exits: Arc<Mutex<Vec<ExitStatus>>>,
    /// Set when the scenario ends: start nothing again.
    stop: Arc<AtomicBool>,
}

impl Phd2Unit {
    /// A unit in `dir` that keeps PHD2 running (`keeps`), or is inactive.
    /// Until a mock is [adopted](Self::adopt), the unit keeping PHD2 is
    /// waiting to start it (`activating`).
    pub async fn create(dir: PathBuf, keeps: bool) -> Self {
        let state_file = dir.join("unit-state");
        let initial = if keeps { "activating" } else { "inactive" };
        write_state(&state_file, initial, 0);
        let script = dir.join("systemctl");
        let body = systemctl_script(&state_file);
        // Written under the spawn lock, so no child forked meanwhile holds
        // the script open for writing when the service runs it
        // (docs/skills/testing.md § 6.6).
        bdd_infra::off_loop(move || {
            bdd_infra::with_spawn_lock(|| write_executable(&script, &body));
        })
        .await;
        Self {
            dir,
            keeps,
            main: Arc::new(Mutex::new(None)),
            exits: Arc::new(Mutex::new(Vec::new())),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    pub const fn keeps(&self) -> bool {
        self.keeps
    }

    /// The service's `PATH`: the stand-in first.
    pub fn path_env(&self) -> String {
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let dirs = std::iter::once(self.dir.clone()).chain(std::env::split_paths(&rest));
        std::env::join_paths(dirs)
            .expect("the scenario's directory fits in PATH")
            .into_string()
            .expect("PATH is UTF-8")
    }

    /// Run `child` as the unit's main process, and start PHD2 again with
    /// `respawn` 2 s after each exit.
    pub fn adopt(&self, child: Child, respawn: Respawn) {
        let state_file = self.dir.join("unit-state");
        write_state(&state_file, "active", child.id());
        *lock(&self.main) = Some(child);
        let main = Arc::clone(&self.main);
        let exits = Arc::clone(&self.exits);
        let stop = Arc::clone(&self.stop);
        std::thread::spawn(move || keep_running(&state_file, &main, &exits, &stop, &respawn));
    }

    /// How each main process has ended so far, in order.
    pub fn exits(&self) -> Vec<ExitStatus> {
        lock(&self.exits).clone()
    }
}

impl Drop for Phd2Unit {
    /// Start nothing again, and kill the main process, reaping it on a
    /// thread of its own (the world is dropped on a runtime worker).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(child) = lock(&self.main).take() {
            kill_and_reap(child);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn kill_and_reap(mut child: Child) {
    let _ = child.kill();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// The unit's restart loop, on a thread of its own: wait for the main
/// process to exit, record how, wait the restart delay, start it again.
fn keep_running(
    state_file: &Path,
    main: &Mutex<Option<Child>>,
    exits: &Mutex<Vec<ExitStatus>>,
    stop: &AtomicBool,
    respawn: &Respawn,
) {
    loop {
        let exited = match lock(main).as_mut() {
            Some(child) => child.try_wait(),
            // The scenario ended and took the process.
            None => return,
        };
        match exited {
            Ok(None) => {
                std::thread::sleep(EXIT_POLL);
                continue;
            }
            Ok(Some(status)) => {
                lock(main).take();
                lock(exits).push(status);
            }
            Err(e) => panic!("cannot wait for the mock PHD2: {e}"),
        }
        write_state(state_file, "activating", 0);
        let mut waited = Duration::ZERO;
        while waited < RESTART_DELAY {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(EXIT_POLL);
            waited += EXIT_POLL;
        }
        let (_port, child) = respawn();
        let pid = child.id();
        let mut slot = lock(main);
        if stop.load(Ordering::SeqCst) {
            kill_and_reap(child);
            return;
        }
        *slot = Some(child);
        drop(slot);
        write_state(state_file, "active", pid);
    }
}

/// Replace the unit's state file in one step, so the stand-in reads either
/// the old state or the new one.
fn write_state(state_file: &Path, active_state: &str, main_pid: u32) {
    let staged = state_file.with_extension("new");
    std::fs::write(
        &staged,
        format!("ActiveState={active_state}\nMainPID={main_pid}\n"),
    )
    .expect("write the unit's state");
    std::fs::rename(&staged, state_file).expect("replace the unit's state");
}

/// A `systemctl` that answers the one query the service makes about its
/// PHD2 unit from `state_file`, with shell builtins only, and fails any
/// other call.
fn systemctl_script(state_file: &Path) -> String {
    let state_file = state_file.display().to_string();
    assert!(
        !state_file.contains('\''),
        "the scenario's directory {state_file} holds a quote"
    );
    format!(
        "#!/bin/sh\n\
         # The scenario's stand-in for systemctl (tests/bdd/phd2_unit.rs).\n\
         if [ \"$*\" != \"show --property=ActiveState --property=MainPID rusty-photon-phd2\" ]; then\n\
         \x20 echo \"unexpected systemctl call: $*\" >&2\n\
         \x20 exit 1\n\
         fi\n\
         while IFS= read -r line; do printf '%s\\n' \"$line\"; done < '{state_file}'\n"
    )
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).expect("write the systemctl stand-in");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("make the systemctl stand-in executable");
    }
}
