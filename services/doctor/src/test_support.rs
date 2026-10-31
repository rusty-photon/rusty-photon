//! Scaffolding shared by this crate's unit-test modules.

use tokio::sync::{Mutex, MutexGuard};

/// Held for the whole body of every unit test in this crate that starts a
/// child process.
///
/// Some of those tests write a script and then run it. A child that another
/// test forks while the script is still open for writing inherits the write
/// descriptor and keeps it until its own exec closes it (`O_CLOEXEC`). An
/// exec of the script inside that window fails with `ETXTBSY` ("Text file
/// busy"). Libtest runs a binary's tests as threads of one process, so the
/// forking test can be any test that starts a child, not only one that
/// writes a script. Every such test takes this lock, so no fork overlaps a
/// write.
///
/// It is tokio's mutex: an async test waits for it without blocking its
/// runtime, and a test that panics releases it as the guard drops, with no
/// poisoning for the next test to clear.
static FORK_LOCK: Mutex<()> = Mutex::const_new(());

/// Take [`FORK_LOCK`] in an async test, as its first line.
pub async fn fork_lock() -> MutexGuard<'static, ()> {
    FORK_LOCK.lock().await
}

/// Take [`FORK_LOCK`] in a synchronous test, as its first line.
///
/// # Panics
///
/// Panics if called from within an asynchronous execution context, as
/// tokio's `blocking_lock` does. Async tests take [`fork_lock`].
pub fn blocking_fork_lock() -> MutexGuard<'static, ()> {
    FORK_LOCK.blocking_lock()
}
