use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{HeaderValue, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use tracing::debug;
use zeroize::Zeroizing;

use crate::config::AuthConfig;
use crate::verifier::{Verdict, Verifier};

/// Apply HTTP Basic Auth middleware to a router.
///
/// Builds one `Verifier` for this layer instance: the verification memo
/// and the KDF gate live as long as the router does, so a config reload
/// that rebuilds the router starts from an empty memo.
pub fn apply(router: Router, config: &AuthConfig) -> Router {
    apply_with_verifier(router, Arc::new(Verifier::new(config)))
}

/// Apply the middleware with a caller-supplied verifier (the in-crate tests
/// inject a counting or blocking KDF through it).
pub(crate) fn apply_with_verifier(router: Router, verifier: Arc<Verifier>) -> Router {
    router.layer(middleware::from_fn_with_state(verifier, auth_middleware))
}

async fn auth_middleware(
    State(verifier): State<Arc<Verifier>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());

    let Some(header_value) = auth_header else {
        debug!("missing Authorization header");
        return unauthorized_response();
    };

    let Some(encoded) = header_value.strip_prefix("Basic ") else {
        debug!("Authorization header is not Basic scheme");
        return unauthorized_response();
    };

    // The decoded credential is wiped when this handler returns; the
    // password itself travels to the verifier in its own zeroizing buffer.
    let Ok(decoded) = BASE64.decode(encoded).map(Zeroizing::new) else {
        debug!("Authorization header contains invalid base64");
        return unauthorized_response();
    };
    let Ok(decoded) = std::str::from_utf8(&decoded) else {
        debug!("Authorization header contains invalid UTF-8");
        return unauthorized_response();
    };

    let Some((username, password)) = decoded.split_once(':') else {
        debug!("Authorization header missing ':' separator");
        return unauthorized_response();
    };
    let password = Zeroizing::new(password.to_owned());

    match verifier.check(username, password).await {
        Verdict::Allow => next.run(request).await,
        Verdict::Deny => {
            debug!("invalid credentials for user '{}'", username);
            unauthorized_response()
        }
        Verdict::Busy => service_unavailable_response(),
    }
}

fn unauthorized_response() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"Rusty Photon\""),
    );
    response
}

/// The gate stayed busy with other credentials for the whole wait. Not a
/// 401 — the credential was never judged — and not a 429, which sentinel's
/// probe would read as down; 503 is "alive but degraded" to it.
fn service_unavailable_response() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use super::*;
    use crate::credentials;
    use crate::verifier::Kdf;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt as _;

    /// One real Argon2id hash for the whole test binary: hashing exists to
    /// be slow, and an unoptimised test build makes it slower still.
    fn test_config() -> AuthConfig {
        static HASH: OnceLock<String> = OnceLock::new();
        let hash = HASH.get_or_init(|| credentials::hash_password("test-password").unwrap());
        AuthConfig {
            username: "testuser".to_string(),
            password_hash: hash.clone(),
        }
    }

    fn test_router() -> Router {
        let config = test_config();
        let app = Router::new().route("/test", get(|| async { "ok" }));
        apply(app, &config)
    }

    /// A router over an injected KDF (no argon2), with a short gate wait.
    fn router_with_kdf(kdf: Box<Kdf>) -> (Router, Arc<Verifier>) {
        let config = AuthConfig {
            username: "testuser".to_string(),
            password_hash:
                "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$RdescudvJCsgt3ub+b+dWRWJTmaaJObG"
                    .to_string(),
        };
        let key = Some(Zeroizing::new([3u8; 64]));
        let verifier = Arc::new(Verifier::build(
            &config,
            kdf,
            key,
            Duration::from_millis(100),
        ));
        let app = Router::new().route("/test", get(|| async { "ok" }));
        (apply_with_verifier(app, Arc::clone(&verifier)), verifier)
    }

    fn basic_auth_header(username: &str, password: &str) -> String {
        let encoded = BASE64.encode(format!("{username}:{password}"));
        format!("Basic {encoded}")
    }

    fn authed_request(username: &str, password: &str) -> Request<Body> {
        Request::builder()
            .uri("/test")
            .header("authorization", basic_auth_header(username, password))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_repeated_credential_is_served_without_a_second_kdf() {
        let (app, verifier) = router_with_kdf(Box::new(|password, _| password == "pw"));
        for _ in 0..3 {
            let response = app
                .clone()
                .oneshot(authed_request("testuser", "pw"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(verifier.kdf_runs(), 1);
    }

    #[tokio::test]
    async fn a_busy_gate_answers_503_with_retry_after_and_no_challenge() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let kdf: Box<Kdf> = Box::new(move |password, _| {
            let _ = release_rx.lock().unwrap().recv();
            password == "pw"
        });
        let (app, _verifier) = router_with_kdf(kdf);
        let first = tokio::spawn({
            let app = app.clone();
            async move { app.oneshot(authed_request("testuser", "pw")).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let response = app
            .clone()
            .oneshot(authed_request("testuser", "another-guess"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(RETRY_AFTER).unwrap(), "1");
        assert!(
            response.headers().get(WWW_AUTHENTICATE).is_none(),
            "a 503 is not a credential verdict and must not challenge"
        );

        release_tx.send(()).unwrap();
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing in test_config() is too slow under Miri
    async fn valid_credentials_returns_200() {
        let app = test_router();
        let request = Request::builder()
            .uri("/test")
            .header(
                "authorization",
                basic_auth_header("testuser", "test-password"),
            )
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing in test_config() is too slow under Miri
    async fn wrong_password_returns_401() {
        let app = test_router();
        let request = Request::builder()
            .uri("/test")
            .header("authorization", basic_auth_header("testuser", "wrong"))
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing in test_config() is too slow under Miri
    async fn wrong_username_returns_401() {
        let app = test_router();
        let request = Request::builder()
            .uri("/test")
            .header(
                "authorization",
                basic_auth_header("baduser", "test-password"),
            )
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing in test_config() is too slow under Miri
    async fn missing_auth_header_returns_401() {
        let app = test_router();
        let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing in test_config() is too slow under Miri
    async fn malformed_auth_header_returns_401() {
        let app = test_router();
        let request = Request::builder()
            .uri("/test")
            .header("authorization", "Bearer some-token")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing in test_config() is too slow under Miri
    async fn response_401_includes_www_authenticate_header() {
        let app = test_router();
        let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let www_auth = response
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(www_auth, "Basic realm=\"Rusty Photon\"");
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing in test_config() is too slow under Miri
    async fn invalid_base64_returns_401() {
        let app = test_router();
        let request = Request::builder()
            .uri("/test")
            .header("authorization", "Basic !!!not-base64!!!")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // argon2 hashing in test_config() is too slow under Miri
    async fn missing_colon_separator_returns_401() {
        let encoded = BASE64.encode("no-colon-here");
        let app = test_router();
        let request = Request::builder()
            .uri("/test")
            .header("authorization", format!("Basic {encoded}"))
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
