#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! `rusty-photon-driver` — the shared ASCOM-driver runtime layer.
//!
//! Where [`rusty_photon_config`] is the transport- and consumer-agnostic config
//! *model* (also used by the plain-REST `rp` / `sentinel` services), this crate is
//! the ASCOM-driver *adapter* shared by the six Alpaca driver services. It owns:
//!
//! - [`driver_error!`] — a macro generating each driver's common error enum (the
//!   shared transport-driver variants + their ASCOM classification +
//!   `From<TransportError>`), with device-specific variants spliced in. Kept a
//!   macro (rather than a shared enum) so each driver's error stays a local type —
//!   its own `From<SessionError<XxxCodecError>>` would otherwise be orphan-illegal.
//! - [`apply_error_to_ascom`] — the one mapping that can't be a `From` impl
//!   (`ApplyError` and `ASCOMError` are both foreign here).
//! - [`ConfigActionCtx`] + [`dispatch`] + [`supported_actions`] — the generic
//!   `config.get` / `config.apply` / `config.schema` action dispatch, including the
//!   fire-after-response in-process reload.
//!
//! A driver invokes [`driver_error!`] for its error type and delegates
//! `Device::action` / `Device::supported_actions` to the functions here. See
//! [`docs/decisions/007-rusty-photon-driver-shared-crate.md`].
//!
//! [`docs/decisions/007-rusty-photon-driver-shared-crate.md`]: ../../../docs/decisions/007-rusty-photon-driver-shared-crate.md

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

pub mod actions;
pub mod discovery;
pub mod error;
mod macros;

pub use actions::{dispatch, supported_actions, ConfigActionCtx, RELOAD_AFTER_RESPONSE_DELAY};
pub use error::apply_error_to_ascom;
