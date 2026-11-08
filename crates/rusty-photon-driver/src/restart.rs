//! A driver service's restart on request, over HTTP
//! (docs/crates/rusty-photon-service-lifecycle.md § Restart on request).
//!
//! [`routes`] serves `POST /service/v1/restart`, which asks the service's
//! runner to restart, and `GET /service/v1/instance`, which names the
//! process that answers, so a client can tell the restarted process from
//! the one it asked. A driver merges them into its router beside the Alpaca
//! API, before the auth layer wraps it.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusty_photon_service_lifecycle::Restart;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// The path a restart request is posted to.
pub const RESTART_PATH: &str = "/service/v1/restart";

/// The path that names the process serving.
pub const INSTANCE_PATH: &str = "/service/v1/instance";

/// What both routes answer with: the id of the process that answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instance {
    pub instance: String,
}

/// The body a restart request carries: an empty JSON object, so the
/// request has to be JSON and a page on another site cannot submit it as a
/// form.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartBody {}

/// The two routes, answering for `restart`. Building them fixes the
/// process's [`instance_id`], so it is fixed once the process serves.
pub fn routes(restart: Restart) -> Router {
    let _ = instance_id();
    Router::new()
        .route(RESTART_PATH, post(request_restart))
        .route(INSTANCE_PATH, get(current_instance))
        .with_state(restart)
}

/// The router a driver puts its Alpaca service behind, as the fallback:
/// the two routes when the runner handed the server a restart, none when a
/// test built the server without one.
pub fn router(restart: Option<Restart>) -> Router {
    restart.map_or_else(Router::new, routes)
}

/// This process's id: its process id and the time it was first asked for,
/// fixed for the process's life.
///
/// A reload keeps it. The next process has another: a process id the OS
/// hands out again comes with a later time.
#[must_use]
pub fn instance_id() -> &'static str {
    static INSTANCE: OnceLock<String> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        format!("{}-{started}", std::process::id())
    })
}

async fn request_restart(
    State(restart): State<Restart>,
    Json(RestartBody {}): Json<RestartBody>,
) -> (StatusCode, Json<Instance>) {
    debug!("restart requested over HTTP");
    restart.request();
    (
        StatusCode::ACCEPTED,
        Json(Instance {
            instance: instance_id().to_string(),
        }),
    )
}

async fn current_instance() -> Json<Instance> {
    Json(Instance {
        instance: instance_id().to_string(),
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request};
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use super::*;

    fn post(body: &'static str, content_type: &str) -> Request<Body> {
        Request::post(RESTART_PATH)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .unwrap()
    }

    async fn instance_of(response: axum::response::Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice::<Instance>(&bytes).unwrap().instance
    }

    #[tokio::test]
    async fn a_json_post_requests_the_restart_and_names_this_process() {
        let token = CancellationToken::new();
        let restart = Restart::new(token.clone());
        let response = routes(restart.clone())
            .oneshot(post("{}", "application/json"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(instance_of(response).await, instance_id());
        assert!(restart.is_requested());
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn a_form_post_is_refused_and_requests_nothing() {
        let restart = Restart::new(CancellationToken::new());
        let response = routes(restart.clone())
            .oneshot(post("restart=1", "application/x-www-form-urlencoded"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(!restart.is_requested());
    }

    #[tokio::test]
    async fn a_body_with_fields_is_refused_and_requests_nothing() {
        let restart = Restart::new(CancellationToken::new());
        let response = routes(restart.clone())
            .oneshot(post(r#"{"now": true}"#, "application/json"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!restart.is_requested());
    }

    #[tokio::test]
    async fn the_instance_route_names_this_process_without_restarting() {
        let restart = Restart::new(CancellationToken::new());
        let response = routes(restart.clone())
            .oneshot(Request::get(INSTANCE_PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(instance_of(response).await, instance_id());
        assert!(!restart.is_requested());
    }

    #[tokio::test]
    async fn a_server_built_without_a_restart_serves_neither_route() {
        let response = router(None)
            .oneshot(post("{}", "application/json"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_instance_id_is_fixed_for_the_process() {
        assert_eq!(instance_id(), instance_id());
        assert!(instance_id().starts_with(&format!("{}-", std::process::id())));
    }
}
