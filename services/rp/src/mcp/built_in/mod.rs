//! Per-tool-category submodules.
//!
//! Each module declares its own
//! `#[tool_router(router = tool_router_<name>, vis = "pub")]` impl
//! block on `super::handler::McpHandler`; the merge happens in
//! `McpHandler::new`. Adding a new tool category = one new file
//! here, one `pub mod <name>;` line below, and one
//! `+ Self::tool_router_<name>()` in `handler::McpHandler::new`.

pub mod auto_focus;
pub mod camera;
pub mod center_on_target;
pub mod cooling;
pub mod cover_calibrator;
pub mod filter_wheel;
pub mod focuser;
pub mod guider;
pub mod imaging;
pub mod mount;
pub mod plan_schema;
pub mod plan_validation;
pub mod planner;
pub mod plate_solve;
pub mod rotator;
pub mod safety;
pub mod targets;
