//! The doctor report schema — re-exported from its canonical home,
//! `rusty_photon_doctor_checks::report` (docs/services/doctor.md §Report).
//!
//! Since D5 the schema crosses a binary boundary (per-service `doctor`
//! subcommands emit it, central doctor aggregates it), so central doctor
//! consumes the one shared definition instead of carrying its own copy.

pub use rusty_photon_doctor_checks::report::*;
