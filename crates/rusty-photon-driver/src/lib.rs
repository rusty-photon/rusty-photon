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
//! - `setup` — the glue from the crate's setup hooks (`Device::setup_request`,
//!   `Server::setup`) to `rusty-photon-setup`'s pages.
//! - [`connected_transition`] — what an ASCOM `Connected` write does on a device
//!   whose session may have been lost to a device that left the bus.
//! - [`overrides`] — the own part of the setup pages of a driver whose hardware
//!   names its devices and whose configuration only renames them, through a
//!   `devices` map of overrides: a page per device, editing its override.
//! - `claims` (the `claims` feature) — a camera driver's device claims at
//!   runtime: the placeholder Camera that holds a listed device number whose
//!   camera cannot be served, and the failed USB scan's background re-scan.
//!   Only the camera drivers with a `usb_devices` list turn the feature on.
//!
//! A driver invokes [`driver_error!`] for its error type and serves its setup
//! pages through `setup`. See
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

#[cfg(feature = "claims")]
pub mod claims;
pub mod connection;
pub mod discovery;
mod macros;
pub mod overrides;
pub mod setup;

pub use connection::{connected_transition, ConnectedTransition};
pub use setup::{device_setup_request, ServerPages};
