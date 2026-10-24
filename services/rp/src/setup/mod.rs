//! rp's setup page, `/setup` (`docs/services/rp.md` § "Setup Page").
//!
//! The shared crate's server page over rp's configuration, with rp's own
//! part ([`roster`]): the equipment block and its roster, and the
//! registered plugins. rp is not an Alpaca device, so it has no device
//! pages, and it applies its configuration at the next start
//! (`ApplyDisposition::Restart`).

pub mod roster;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use rusty_photon_setup::{PageRequest, PageResponse, PageScope, SetupPages};

use crate::config::Config;
use crate::config_actions::RpConfigDriver;
use crate::equipment::EquipmentRegistry;

/// The page's handler, shared by its routes.
pub type Pages = Arc<SetupPages<RpConfigDriver>>;

/// The page over `config`, saved to the file at `path`, its roster's live
/// state read from `equipment`.
#[must_use]
pub fn pages(config: &Config, path: PathBuf, equipment: Arc<EquipmentRegistry>) -> Pages {
    Arc::new(
        SetupPages::new("rp", config.clone(), path, ())
            .with_https(config.server.tls.is_some())
            .with_driver_pages(Arc::new(roster::RosterPages::new(equipment))),
    )
}

/// `/setup`, for `GET` and `POST`.
pub fn router(pages: Pages) -> Router {
    Router::new()
        .route("/setup", get(show).post(save))
        .with_state(pages)
}

async fn show(
    State(pages): State<Pages>,
    headers: HeaderMap,
    Query(query): Query<Vec<(String, String)>>,
) -> Response {
    let request = PageRequest {
        method: Method::GET,
        headers,
        query,
        form: Vec::new(),
    };
    answer(&pages, request).await
}

async fn save(
    State(pages): State<Pages>,
    headers: HeaderMap,
    Query(query): Query<Vec<(String, String)>>,
    Form(form): Form<Vec<(String, String)>>,
) -> Response {
    let request = PageRequest {
        method: Method::POST,
        headers,
        query,
        form,
    };
    answer(&pages, request).await
}

async fn answer(pages: &Pages, request: PageRequest) -> Response {
    let PageResponse {
        status,
        headers,
        body,
    } = pages.handle(PageScope::Server, request).await;
    (status, headers, body).into_response()
}
