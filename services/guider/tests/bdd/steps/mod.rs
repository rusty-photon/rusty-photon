//! Step definition modules, one for each `.feature` file under
//! `tests/features/` that has steps of its own. Cucumber's `#[given]` /
//! `#[when]` / `#[then]` macros register globally, so the configuration
//! and start steps in `config_steps.rs`, the device steps in
//! `devices_steps.rs` and the request and answer steps in `loop_steps.rs`
//! serve every feature.

pub mod auth_steps;
pub mod calibrate_steps;
pub mod calibration_store_steps;
pub mod config_steps;
pub mod devices_steps;
pub mod doctor_steps;
pub mod frame_latest_steps;
pub mod frames_steps;
pub mod guiding_steps;
pub mod health_steps;
pub mod loop_steps;
pub mod star_reselect_steps;
pub mod stats_steps;
pub mod stop_steps;
