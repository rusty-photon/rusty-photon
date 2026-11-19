//! The shared steps behind every driver service's `restart.feature`: a
//! restart on request ends the service with the restart code
//! ([`docs/crates/rusty-photon-service-lifecycle.md`] § Restart on request).
//!
//! The World embeds a [`ServiceRestartState`], implements
//! [`ServiceRestartWorld`], and invokes
//! [`service_restart_steps!`](crate::service_restart_steps); its own Given
//! and When steps start the service. The request goes to the service's
//! announced address over plain HTTP, and the exit is read from the
//! service's own process: no service manager runs under test to start it
//! again. Three more steps go in the suite's `auth.feature`, after the
//! suite's own steps start the service with TLS and auth: both routes
//! refuse a request without the credentials, and the refused restart
//! request started no restart.
//!
//! [`docs/crates/rusty-photon-service-lifecycle.md`]: ../../../docs/crates/rusty-photon-service-lifecycle.md

use std::time::Duration;

use crate::tls_auth::PkiFixture;
use crate::ServiceHandle;

/// The path a driver service takes a restart request on.
pub const RESTART_PATH: &str = "/service/v1/restart";

/// The path that names the process serving.
pub const INSTANCE_PATH: &str = "/service/v1/instance";

/// How long a step waits for a service asked to restart to exit, on the
/// slowest runner the suites meet: the service's close deadline is 10 s.
const EXIT_BUDGET: Duration = Duration::from_secs(30);

/// Per-scenario state; embed one (via `#[derive(Default)]`) in the suite's
/// `World`.
#[derive(Debug, Default)]
pub struct ServiceRestartState {
    /// The process the service named before the request.
    pub instance_before: Option<String>,
    /// The restart request's status.
    pub status: Option<u16>,
    /// The process the restart request's answer named.
    pub instance_answered: Option<String>,
}

/// Contract the [`service_restart_steps!`](crate::service_restart_steps)
/// macro programs against.
pub trait ServiceRestartWorld {
    /// Mutable access to the embedded [`ServiceRestartState`].
    fn service_restart(&mut self) -> &mut ServiceRestartState;

    /// The running service.
    fn restarted_service(&mut self) -> &mut ServiceHandle;

    /// The port and PKI of the service as the suite's auth scenarios start
    /// it, with TLS and auth.
    fn service_behind_auth(&self) -> (u16, &PkiFixture);
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("a plain reqwest client builds")
}

fn instance_of(body: &serde_json::Value) -> String {
    body["instance"]
        .as_str()
        .unwrap_or_else(|| panic!("the answer names no instance: {body}"))
        .to_string()
}

/// Read the serving process's id, then post the restart request, as `rp`'s
/// `restart_service` does.
///
/// # Panics
///
/// Panics when the service does not answer either request.
pub async fn request_restart(state: &mut ServiceRestartState, base_url: &str) {
    let client = client();
    let before: serde_json::Value = client
        .get(format!("{base_url}{INSTANCE_PATH}"))
        .send()
        .await
        .expect("the instance route answers")
        .json()
        .await
        .expect("the instance route answers JSON");
    state.instance_before = Some(instance_of(&before));
    let response = client
        .post(format!("{base_url}{RESTART_PATH}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("the restart route answers");
    state.status = Some(response.status().as_u16());
    let body: serde_json::Value = response
        .json()
        .await
        .expect("the restart route answers JSON");
    state.instance_answered = Some(instance_of(&body));
}

/// Assert the restart request was accepted with `status`, naming the
/// process that served before it.
///
/// # Panics
///
/// Panics when it was not.
pub fn assert_accepted(state: &ServiceRestartState, status: u16) {
    assert_eq!(state.status, Some(status), "the restart request's status");
    assert_eq!(
        state.instance_answered, state.instance_before,
        "the restart request's answer must name the process that is restarting"
    );
}

/// Assert the restart route refuses a request without valid credentials.
///
/// Wait until the service started with TLS and auth names its process to a
/// client with the credentials, then post a restart request with no
/// credentials and one with a wrong password, and assert both are refused
/// with 401.
///
/// The instance route answering 200 first shows that the service's routes
/// sit behind the credentials, so the 401 is theirs: the auth layer answers
/// a path no route serves with 401 too.
///
/// # Panics
///
/// Panics when the instance route never answers 200 with the credentials,
/// or the restart route answers anything but 401 without valid ones.
pub async fn assert_restart_needs_credentials(port: u16, pki: &PkiFixture) {
    let client = pki.https_client();
    let base_url = format!("https://127.0.0.1:{port}");
    crate::tls_auth::wait_until_ready(
        &client,
        &format!("{base_url}{INSTANCE_PATH}"),
        pki.username(),
        pki.password(),
    )
    .await;
    let wrong_password = format!("not-{}", pki.password());
    for (credentials, what) in [
        (None, "no credentials"),
        (Some(wrong_password.as_str()), "a wrong password"),
    ] {
        let mut request = client
            .post(format!("{base_url}{RESTART_PATH}"))
            .json(&serde_json::json!({}));
        if let Some(password) = credentials {
            request = request.basic_auth(pki.username(), Some(password));
        }
        let response = request.send().await.expect("the restart route answers");
        assert_eq!(
            response.status().as_u16(),
            401,
            "a restart request with {what}"
        );
    }
}

/// Ask the service started with TLS and auth which process serves,
/// without the credentials, and assert it is refused with 401.
///
/// # Panics
///
/// Panics when the instance route answers anything but 401.
pub async fn assert_instance_needs_credentials(port: u16, pki: &PkiFixture) {
    let response = pki
        .https_client()
        .get(format!("https://127.0.0.1:{port}{INSTANCE_PATH}"))
        .send()
        .await
        .expect("the instance route answers");
    assert_eq!(
        response.status().as_u16(),
        401,
        "an instance request without credentials"
    );
}

/// What the lifecycle crate's `Restart::request` logs the moment a restart
/// is requested, before the request's answer is written.
///
/// Each driver's restart scenario requires it, so a change to the line
/// fails there rather than leaving [`assert_no_restart_requested`] unable
/// to fail.
pub const RESTART_REQUESTED: &str = "restart requested:";

/// Stop the service, then assert its whole log carries no restart request.
///
/// A request logs [`RESTART_REQUESTED`] before its answer is written, well
/// before the stop, and [`ServiceHandle::stop`] returns once the service
/// has exited and the log's reader has reached the end of the log, or
/// after 1 s more.
///
/// # Panics
///
/// Panics when the log shows a restart request.
pub async fn assert_no_restart_requested(service: &mut ServiceHandle) {
    service.stop().await;
    let lines = service.stderr_lines();
    let requested: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains(RESTART_REQUESTED))
        .collect();
    assert!(
        requested.is_empty(),
        "a request without credentials started a restart: {requested:?}"
    );
}

/// What the runner logs when a service asked to restart has closed in
/// order and exits; an exit at the close deadline never logs it
/// (`rusty-photon-service-lifecycle`'s `finish_console`).
pub const CLOSED_IN_ORDER: &str = "closed after the restart request; exiting with code";

/// Wait for the service to exit by itself, and assert it logged the
/// restart request and closed in order before exiting with `code`, rather
/// than reaching the close deadline, whose exit has the same code.
///
/// # Panics
///
/// Panics when it is still running after the budget, exits with another
/// code, or exits without having logged the request or closed.
pub async fn assert_closes_and_exits_with(service: &mut ServiceHandle, code: i32) {
    let status = service.wait_exit(EXIT_BUDGET).await.unwrap_or_else(|| {
        panic!("the service was still running {EXIT_BUDGET:?} after the restart request")
    });
    assert_eq!(
        status.code(),
        Some(code),
        "the service's exit status: {status}"
    );
    let lines = service.stderr_lines();
    assert!(
        lines.iter().any(|line| line.contains(RESTART_REQUESTED)),
        "the service restarted without logging {RESTART_REQUESTED:?}; the line changed, and \
         the auth scenario's check for it can no longer fail. Its last lines:\n{}",
        last_lines(&lines)
    );
    assert!(
        lines.iter().any(|line| line.contains(CLOSED_IN_ORDER)),
        "the service exited without logging that it closed after the restart request; its \
         close reached the deadline, or the line changed. Its last lines:\n{}",
        last_lines(&lines)
    );
}

/// The last ten of `lines`, for a panic message.
fn last_lines(lines: &[String]) -> String {
    lines
        .iter()
        .rev()
        .take(10)
        .rev()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n")
}

/// Generate the steps of `restart.feature`, and the restart steps of
/// `auth.feature`, for `$world`, which implements [`ServiceRestartWorld`].
///
/// ```rust,ignore
/// use crate::world::MyWorld;
///
/// bdd_infra::service_restart_steps!(MyWorld);
/// ```
#[macro_export]
macro_rules! service_restart_steps {
    ($world:ident) => {
        #[::cucumber::when("a client asks the service to restart")]
        async fn service_restart_request(world: &mut $world) {
            use $crate::service_restart::ServiceRestartWorld as _;
            let base_url = world.restarted_service().base_url();
            $crate::service_restart::request_restart(world.service_restart(), &base_url).await;
        }

        #[::cucumber::then(
            expr = "the service accepts the restart request with status {int}, naming its own process"
        )]
        fn service_restart_accepted(world: &mut $world, status: u16) {
            use $crate::service_restart::ServiceRestartWorld as _;
            $crate::service_restart::assert_accepted(world.service_restart(), status);
        }

        #[::cucumber::then(expr = "the service closes, then exits with code {int}")]
        async fn service_restart_exits(world: &mut $world, code: i32) {
            use $crate::service_restart::ServiceRestartWorld as _;
            $crate::service_restart::assert_closes_and_exits_with(world.restarted_service(), code)
                .await;
        }

        #[::cucumber::then("the service refuses a restart request without valid credentials with 401")]
        async fn service_restart_needs_credentials(world: &mut $world) {
            use $crate::service_restart::ServiceRestartWorld as _;
            let (port, pki) = world.service_behind_auth();
            $crate::service_restart::assert_restart_needs_credentials(port, pki).await;
        }

        #[::cucumber::then("the service refuses an instance request without credentials with 401")]
        async fn service_restart_instance_needs_credentials(world: &mut $world) {
            use $crate::service_restart::ServiceRestartWorld as _;
            let (port, pki) = world.service_behind_auth();
            $crate::service_restart::assert_instance_needs_credentials(port, pki).await;
        }

        #[::cucumber::then("the service's log shows no restart request once it is stopped")]
        async fn service_restart_none_requested(world: &mut $world) {
            use $crate::service_restart::ServiceRestartWorld as _;
            $crate::service_restart::assert_no_restart_requested(world.restarted_service()).await;
        }
    };
}
