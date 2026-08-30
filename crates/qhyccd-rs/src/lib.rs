//! # QHYCCD SDK bindings for Rust
//!
//! This crate provides a safe interface to the QHYCCD SDK for controlling QHYCCD cameras and filter wheels.
//! (Focusers are supported by the QHYCCD SDK but are not yet exposed by this crate.)
//! The `libqhyccd-sys` crate provides the raw FFI bindings; this crate wraps them in a safe API, using `tracing` for logging and returning typed [`QHYError`] values (via `thiserror`) for error handling.
//!
//! # Example
//! ```no_run
//! use qhyccd_rs::Sdk;
//! let sdk = Sdk::new().expect("SDK::new failed");
//! let sdk_version = sdk.version().expect("get_sdk_version failed");
//! println!("SDK version: {:?}", sdk_version);
//! ```
//!
//! # Simulation Feature
//!
//! The `simulation` feature enables development and testing without physical hardware. When enabled,
//! [`Sdk::new()`] automatically provides a simulated camera environment that behaves like real hardware.
//!
//! ## Enabling Simulation
//!
//! ```toml
//! [dependencies]
//! qhyccd-rs = { version = "0.1.9", features = ["simulation"] }
//! ```
//!
//! ## Transparent Usage
//!
//! With simulation enabled, your code works identically for both real and simulated cameras:
//!
//! ```no_run
//! use qhyccd_rs::Sdk;
//!
//! // Same code works with or without the simulation feature
//! let sdk = Sdk::new().expect("Failed to initialize SDK");
//! let cameras = sdk.cameras();
//! println!("Found {} camera(s)", cameras.count());
//! ```
//!
//! ## Default Simulated Camera
//!
//! When compiled with the `simulation` feature, [`Sdk::new()`] automatically provides:
//!
//! - **Camera**: QHY178M-Simulated (`SIM-QHY178M`)
//!   - 3072x2048 resolution, 16-bit depth
//!   - Cooler support for temperature control
//!   - Full control API (gain, offset, exposure, etc.)
//!
//! - **Filter Wheel**: 7-position CFW
//!   - Accessible via [`Sdk::filter_wheels()`]
//!   - Complete control API support
//!
//! ## Custom Simulated Cameras
//!
//! For advanced use cases, use [`Sdk::new_simulated()`] and [`Sdk::add_simulated_camera()`]:
//!
//! ```
//! # #[cfg(feature = "simulation")]
//! # {
//! use qhyccd_rs::{Sdk, simulation::SimulatedCameraConfig};
//!
//! let mut sdk = Sdk::new_simulated();
//! let config = SimulatedCameraConfig::default()
//!     .with_id("CUSTOM-CAM")
//!     .with_filter_wheel(5);
//! sdk.add_simulated_camera(config);
//! # }
//! ```
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![warn(missing_debug_implementations, rust_2018_idioms, missing_docs)]
// Interior `#[cfg(test)]` modules: the simulated-pixel assertions cast and do
// arithmetic on the tests' own fixture values.
#![cfg_attr(test, allow(clippy::as_conversions, clippy::arithmetic_side_effects))]
// Curated test-scope allow list — documented in the root Cargo.toml
// [workspace.lints] block.
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

/// Raw, unsafe FFI bindings (`libqhyccd-sys`). Prefer the safe API in this crate.
///
/// Re-exported at the crate root to match the sibling `zwo-rs` / `svbony-rs`
/// convention (`pub use libzwo_sys as sys;` / `pub use libsvbony_sys as sys;`).
pub use libqhyccd_sys as sys;

// Module declarations. `camera` is the single device-file-major module holding
// the `Camera` device, its `ControlType` address space, and (only without the
// `simulation` feature) the real-hardware handle machinery — the compile-time
// real/sim split used by the sibling `zwo-rs` / `svbony-rs` crates (a per-method
// `#[cfg]` fork), replacing the former runtime `CameraBackend` enum + `#[automock]`
// FFI-mock layer. It merges the former `camera/` submodule split, `backend.rs`,
// and `control.rs`.
mod camera;
mod error;
mod filter_wheel;
mod quantize;
mod sdk;
mod types;

#[cfg(feature = "simulation")]
pub mod simulation;

// Public re-exports
pub use camera::{Camera, ControlType};
pub use error::{check, QHYError, Result};
pub use filter_wheel::FilterWheel;
pub use sdk::Sdk;
pub use types::{
    BayerPattern, CCDChipArea, CCDChipInfo, FrameInfo, ReadoutMode, SDKVersion, StreamMode,
};
