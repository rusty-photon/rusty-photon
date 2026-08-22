//! Planner sub-tree: MCP tool wrappers over `rp-catalog` and
//! `rp-ephemeris`, plus the decision logic composing them.
//!
//! The wrappers cover catalog lookup and the ephemeris primitives
//! (positions, transit, twilight, etc.); the decision logic composes
//! those primitives into the convenience tools `get_target_status` /
//! `get_next_target` / `get_meridian_status`.
//!
//! The math and data live in their respective crates; this module is
//! purely the MCP-tool wrapping plus the small amount of decision
//! logic that doesn't belong in either dependency. See
//! `docs/services/rp.md` §"Planning and Ephemeris".

pub mod catalog;
pub mod convenience;
pub mod decision;
pub mod goal_wire;
pub mod primitives;
pub mod progress;
pub mod progress_scan;
