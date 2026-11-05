//! Compositional tools, one MCP tool per file. Two flavors:
//!
//! - **Pure compositional analyzers** (`measure_basic`,
//!   `measure_stars`): bind multiple [`crate::imaging::analysis`]
//!   kernels into one MCP-tool-shaped result. Pure functions over
//!   `ArrayView2`, no I/O, no async — unit-testable without a runtime.
//!
//! - **Compound equipment-driving tools**: `center_on_target` drives a
//!   capture/solve/sync/slew loop using primitive built-in tools,
//!   behind async traits for the equipment surface so the driving
//!   logic is testable against synthetic adapters; the MCP wrapper in
//!   `mcp/built_in/<tool>.rs` provides live adapters that bind to the
//!   real Alpaca clients. `auto_focus` holds the pure helpers of the
//!   guiding train's metric sweep (`build_grid`,
//!   `confirmation_accepted`), whose loop reads the guider in
//!   `mcp/built_in/auto_focus.rs`. The math these tools bundle
//!   (`build_grid`, `haversine_arcsec`) stays pure.
//!
//! The `CaptureOps` trait lives in [`ops`], its single canonical
//! home.

pub mod auto_focus;
pub mod center_on_target;
pub mod measure_basic;
pub mod measure_stars;
pub mod ops;
