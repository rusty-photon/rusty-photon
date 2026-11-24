#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
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

//! `guide-simulator`'s mount and sky behind an Alpaca Camera and an
//! Alpaca Telescope, on the wall clock.
//!
//! [`SimDevices::start`] serves the camera and the mount on two loopback
//! servers over one [`Simulator`](guide_simulator::Simulator): a pulse
//! on the mount moves the stars in the camera's next frame. Its methods
//! script the devices while they run and read back every pulse and
//! exposure they were asked for. See `docs/crates/guide-simulator-alpaca.md`.

mod camera;
mod config;
mod rig;
mod server;
mod telescope;

pub use config::{
    CameraDeviceConfig, Device, DevicesConfig, ExposureRecord, FrameFault, MountDeviceConfig,
    PulseRecord, Region,
};
pub use server::{SimDevices, StartError};
