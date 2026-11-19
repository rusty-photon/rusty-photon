//! Step definition modules. Each maps to one `.feature` file under
//! `tests/features/`. Cucumber's `#[given]` / `#[when]` / `#[then]`
//! macros register globally, so a step phrase declared here is
//! resolved no matter which feature file references it — shared
//! phrases (e.g., "the response status is N") live in
//! `solve_steps.rs` and are reused across other feature files'
//! scenarios.

pub mod auth_steps;
pub mod config_steps;
pub mod doctor_steps;
pub mod health_steps;
pub mod real_astap_steps;
pub mod solve_steps;
pub mod supervision_steps;
