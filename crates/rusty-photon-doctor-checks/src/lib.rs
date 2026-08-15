//! rusty-photon-doctor-checks: what central doctor and the per-service
//! `doctor` subcommands share (docs/services/doctor.md) — ADR-016
//! decision 6: the similarity between doctors lives in a shared library,
//! not a shared binary. That is the no-SDK hardware facts and predicates
//! behind the `hardware.*` check family, and, since D5, the canonical
//! report schema, the text renderer, and the per-service runner every
//! service binary's `doctor` subcommand calls.
//!
//! Everything here is read-only: `stat`, directory listings, and inventory
//! queries. Nothing ever opens a device — a running service holds its
//! hardware, and doctor must never contend for it (ADR-016 decision 5).

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
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

pub mod access;
pub mod facts;
pub mod render;
pub mod report;
pub mod service;
pub mod udev;

pub use access::Identity;
pub use facts::{gather, HardwareFacts, PathFacts, PathKind, ProbeRequest, UsbDevice, UserFacts};
