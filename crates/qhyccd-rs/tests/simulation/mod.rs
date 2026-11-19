//! Integration tests for simulated cameras
//!
//! These tests verify that simulated cameras work correctly without
//! requiring actual QHYCCD hardware.

mod camera_tests;
mod config_tests;
mod image_generator_tests;
// `SimulatedCameraState` is `pub(crate)`, so its tests are the `state_tests`
// module in src/simulation.rs.
