use std::future::Future;
use tokio_util::sync::CancellationToken;

use crate::Restart;

/// Cooperative shutdown handle.
///
/// Backed by a [`CancellationToken`] so it composes with any code that
/// already accepts one (e.g. `sentinel`'s engine). Clone freely and pass
/// [`Shutdown::token`] to spawned subtasks; they all observe the same
/// cancellation.
///
/// Constructed by [`crate::ServiceRunner::run`] / `run_with_reload` and
/// handed to the user closure; not constructible from outside the crate.
#[derive(Clone, Debug)]
pub struct Shutdown {
    token: CancellationToken,
    restart: Restart,
}

impl Shutdown {
    /// `restart` must cancel `token` when requested.
    pub(crate) const fn new(token: CancellationToken, restart: Restart) -> Self {
        Self { token, restart }
    }

    #[cfg(test)]
    fn from_token(token: CancellationToken) -> Self {
        Self::new(token.clone(), Restart::new(token))
    }

    /// Clone of the underlying [`CancellationToken`] for handing to
    /// subtasks or APIs that take a token directly.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Future that resolves when shutdown has been requested.
    ///
    /// The returned future is owned (`'static + Send`) — it can be passed
    /// to APIs that move it (e.g. `axum::serve(...).with_graceful_shutdown(...)`
    /// or `BoundServer::start`) without borrowing `self`. Call multiple times
    /// to get multiple independent futures observing the same cancellation.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + 'static {
        let token = self.token.clone();
        async move {
            token.cancelled().await;
        }
    }

    /// Returns `true` if shutdown has already been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// The handle that asks the service to restart: a request cancels this
    /// shutdown, and the runner exits with
    /// [`RESTART_EXIT_CODE`](crate::RESTART_EXIT_CODE) once the run closure
    /// returns (docs/crates/rusty-photon-service-lifecycle.md § Restart on
    /// request).
    #[must_use]
    pub fn restart(&self) -> Restart {
        self.restart.clone()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn is_cancelled_flips_after_token_cancel() {
        let token = CancellationToken::new();
        let shutdown = Shutdown::from_token(token.clone());
        assert!(!shutdown.is_cancelled());
        token.cancel();
        assert!(shutdown.is_cancelled());
    }

    #[tokio::test]
    async fn cancelled_resolves_when_any_clone_cancels() {
        let token = CancellationToken::new();
        let shutdown = Shutdown::from_token(token.clone());
        let cloned_token = shutdown.token();

        let waiter = tokio::spawn(async move { shutdown.cancelled().await });
        cloned_token.cancel();
        waiter.await.unwrap();
    }

    #[test]
    fn a_restart_request_cancels_the_shutdown() {
        let shutdown = Shutdown::from_token(CancellationToken::new());
        shutdown.restart().request();
        assert!(shutdown.is_cancelled());
        assert!(shutdown.restart().is_requested());
    }
}
