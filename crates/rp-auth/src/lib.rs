#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! HTTP Basic Auth utilities for Rusty Photon services.
//!
//! Provides Argon2id credential hashing/verification, axum tower middleware,
//! and shared configuration types for opt-in authentication across all services.
//!
//! # Verification cost
//!
//! The Argon2id verify exists to be expensive (~41 ms on a Raspberry Pi 5)
//! and HTTP Basic is stateless, so the middleware does not run it per
//! request. Each [`layer`] call owns a verifier (the private `verifier`
//! module) that
//!
//! - memoises verdicts under a per-instance random key (a keyed BLAKE2b-256
//!   tag of the username, password and stored hash — never the credential
//!   itself): one positive slot with a 15-minute sliding idle TTL and a ring
//!   of eight negatives for 5 s. A hit answers in microseconds.
//! - runs every miss on tokio's blocking pool behind a one-permit gate, with
//!   the permit and the memo store owned by the blocking closure, so a
//!   disconnecting client can neither free the gate early nor lose the
//!   warm-up, and at most one KDF is in flight per service.
//! - never refuses a request for the credential that is currently being
//!   verified: it waits for that verdict, whether it found the verification
//!   in flight on arrival or only after queueing for the gate. Only a request
//!   for a *different* credential can be answered `503 Service Unavailable`
//!   + `Retry-After: 1`, after waiting 1 s for the gate.
//! - compares the username in constant time and only after the KDF has run,
//!   so a wrong username costs the same as a wrong password.
//!
//! The stored hash, the wire format and the 401 challenge are unchanged; see
//! ADR-003 § Verification memo and KDF admission control.

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
mod memo;
pub mod middleware;
mod verifier;

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
