#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! `rusty-photon-setup` — the HTML setup pages a service serves for its own
//! configuration.
//!
//! The Alpaca Management API defines two pages, `/setup` for the whole
//! server and `/setup/v1/{device_type}/{device_number}/setup` for one device.
//! This crate renders both from a service's config schema, saves a `POST` to
//! either, refuses a cross-site `POST`, edits the server's credential, and
//! fires the service's reload after the response. It depends on neither
//! `ascom-alpaca` nor any service: an Alpaca driver reaches [`SetupPages`]
//! through `rusty-photon-driver`'s glue, a plain HTTP service from its own
//! router. See [`docs/crates/rusty-photon-setup.md`].
//!
//! [`docs/crates/rusty-photon-setup.md`]: ../../../docs/crates/rusty-photon-setup.md

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

pub mod custom;
pub mod form;
pub mod handler;
pub mod origin;
pub mod render;
pub mod schema;
pub mod security;

pub use custom::{BoxFuture, Confirmation, CustomSection, DriverPages, SectionView, CONFIRM};
pub use form::FormValues;
pub use handler::{PageRequest, PageResponse, PageScope, SetupPages, RELOAD_AFTER_RESPONSE_DELAY};
pub use schema::{FieldKind, FieldModel, FieldSpec};
