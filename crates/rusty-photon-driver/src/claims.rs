//! Device claims: the runtime half of a camera driver's `usb_devices` list
//! ([device-claims plan](../../../docs/plans/device-claims-and-phd2-camera.md),
//! Part A), behind this crate's `claims` feature.
//!
//! The pure half — the list's validation, the join that places each SDK
//! camera on a port, the reason a listed number is held, the placeholder's
//! error code and `UniqueID` form, the `doctor --devices` listing — is
//! `rusty-photon-doctor-checks`' `claims` module, beside the USB inventory it
//! reads. What lives here is what that crate cannot hold, being neither an
//! ASCOM nor an async one: the placeholder Camera device a driver registers at
//! a listed number whose camera it cannot serve, and the background re-scan a
//! failed USB scan starts. `rusty-photon-camera-core` is by its own rule no
//! home for a `Camera` implementation.

mod placeholder;
mod rescan;

pub use placeholder::{Placeholder, PlaceholderCamera};
pub use rescan::{rescan_waits, stop_or_rescanned, Blocker, Ended, Rescan, StopCause};
