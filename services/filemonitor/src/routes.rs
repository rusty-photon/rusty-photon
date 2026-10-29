//! The router filemonitor's connections answer with, which a reload
//! replaces under them.

use std::sync::{Arc, Mutex, PoisonError};

use axum::extract::Request;
use axum::response::Response;
use tokio::sync::watch;
use tower::ServiceExt;

/// The router each request is answered with, looked up when the request
/// arrives: replacing it changes what every open connection answers next,
/// and closes none of them.
#[derive(Debug, Clone)]
pub struct Routes {
    current: Arc<Mutex<Current>>,
}

/// The router requests are answered with now.
#[derive(Debug)]
struct Current {
    router: axum::Router,
    /// Cloned into every request the router takes, and dropped once that
    /// request is answered.
    answering: watch::Receiver<()>,
    /// Its `closed()` resolves once every `answering` clone is dropped.
    answered: watch::Sender<()>,
}

impl Current {
    fn new(router: axum::Router) -> Self {
        let (answered, answering) = watch::channel(());
        Self {
            router,
            answering,
            answered,
        }
    }
}

impl Routes {
    pub fn new(router: axum::Router) -> Self {
        Self {
            current: Arc::new(Mutex::new(Current::new(router))),
        }
    }

    /// The router to serve: it answers each request with the router
    /// current when the request arrives.
    pub fn service(&self) -> axum::Router {
        let routes = self.clone();
        axum::Router::new().fallback(move |request: Request| {
            let routes = routes.clone();
            async move { routes.answer(request).await }
        })
    }

    async fn answer(&self, request: Request) -> Response {
        let (router, answering) = {
            let current = self.lock();
            (current.router.clone(), current.answering.clone())
        };
        let response = router.oneshot(request).await;
        drop(answering);
        match response {
            Ok(response) => response,
            Err(never) => match never {},
        }
    }

    /// Put `router` in front of every connection, and return the router
    /// it replaces, which still answers the requests it has taken.
    pub fn replace(&self, router: axum::Router) -> Replaced {
        let replaced = std::mem::replace(&mut *self.lock(), Current::new(router));
        Replaced {
            answered: replaced.answered,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Current> {
        self.current.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A router another has replaced, while it answers the requests it took
/// before then.
#[derive(Debug)]
#[must_use = "dropping a Replaced does not wait for its requests"]
pub struct Replaced {
    answered: watch::Sender<()>,
}

impl Replaced {
    /// Resolve once every request the router took has been answered.
    pub async fn answered(self) {
        self.answered.closed().await;
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use tokio::sync::oneshot;

    fn named(name: &'static str) -> axum::Router {
        axum::Router::new().route("/", get(move || async move { name }))
    }

    async fn ask(service: &axum::Router) -> String {
        let response = service
            .clone()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn a_request_after_a_replace_is_answered_by_the_new_router() {
        let routes = Routes::new(named("first"));
        let service = routes.service();
        assert_eq!(ask(&service).await, "first");

        let _replaced = routes.replace(named("second"));

        assert_eq!(ask(&service).await, "second");
    }

    #[tokio::test]
    async fn a_replaced_router_with_no_request_is_answered_at_once() {
        let routes = Routes::new(named("first"));

        routes.replace(named("second")).answered().await;
    }

    #[tokio::test]
    async fn a_replaced_router_waits_for_the_request_it_is_answering() {
        let (release, released) = oneshot::channel::<()>();
        let released = Arc::new(tokio::sync::Mutex::new(Some(released)));
        let (arrived, has_arrived) = oneshot::channel::<()>();
        let arrived = Arc::new(Mutex::new(Some(arrived)));
        let held = axum::Router::new().route(
            "/",
            get(move || async move {
                if let Some(arrived) = arrived.lock().unwrap().take() {
                    arrived.send(()).unwrap();
                }
                if let Some(released) = released.lock().await.take() {
                    released.await.unwrap();
                }
                "first"
            }),
        );
        let routes = Routes::new(held);
        let service = routes.service();
        let in_flight = tokio::spawn({
            let service = service.clone();
            async move { ask(&service).await }
        });
        has_arrived.await.unwrap();

        let answered = routes.replace(named("second")).answered();
        tokio::pin!(answered);
        assert_eq!(ask(&service).await, "second");
        tokio::select! {
            biased;
            () = &mut answered => panic!("the replaced router should wait for the request it is answering"),
            () = std::future::ready(()) => {}
        }

        release.send(()).unwrap();
        assert_eq!(in_flight.await.unwrap(), "first");
        answered.await;
    }
}
