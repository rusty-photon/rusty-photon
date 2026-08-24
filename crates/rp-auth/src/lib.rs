#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! HTTP Basic Auth utilities for Rusty Photon services.
//!
//! Provides Argon2id credential hashing/verification, axum tower middleware,
//! and shared configuration types for opt-in authentication across all services.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

pub mod config;
pub mod credentials;
pub mod error;
pub mod middleware;

use axum::Router;
use config::AuthConfig;

/// Wrap a router with HTTP Basic Auth middleware.
///
/// All requests must include a valid `Authorization: Basic` header.
/// Requests with missing or invalid credentials receive `401 Unauthorized`
/// with a `WWW-Authenticate: Basic realm="Rusty Photon"` header.
pub fn layer(router: Router, config: &AuthConfig) -> Router {
    middleware::apply(router, config)
}
