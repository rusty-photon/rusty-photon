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

//! A guide camera's view through a mount, in simulated time.
//!
//! [`Mount`] drifts, wobbles and answers guide pulses as a real mount
//! does, short pulses included. [`Simulator::expose`] renders the stars
//! the camera sees through it over an exposure, with seeing, clouds and
//! sensor noise, and every [`Frame`] carries its truth. One seed sets
//! every random draw. See `docs/crates/guide-simulator.md`.

mod camera;
mod config;
mod geometry;
mod mount;
mod random;
mod simulator;
mod sky;
mod time;

pub use camera::{BayerResponse, CameraConfig, Frame, FrameTruth, SensorType, StarTruth};
pub use config::ConfigError;
pub use geometry::{SensorPoint, SkyOffset};
pub use mount::{
    DecBacklash, DecDelivery, DecSense, Drift, GuideDirection, LateStep, Mount, MountConfig,
    MountError, PeriodicTerm, PierSide, PulseDelivery, RaDelivery, SIDEREAL_ARCSEC_PER_S,
};
pub use simulator::{SimConfig, Simulator};
pub use sky::{Cloud, RandomStars, Seeing, SkyConfig, Star, StarField};
pub use time::SimTime;
