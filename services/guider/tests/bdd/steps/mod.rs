//! Step definition modules, one for each `.feature` file under
//! `tests/features/` that has steps of its own. Cucumber's `#[given]` /
//! `#[when]` / `#[then]` macros register globally, so the configuration
//! and start steps in `config_steps.rs` and the device steps in
//! `devices_steps.rs` serve every feature.

pub mod auth_steps;
pub mod config_steps;
pub mod devices_steps;
pub mod doctor_steps;
pub mod health_steps;
