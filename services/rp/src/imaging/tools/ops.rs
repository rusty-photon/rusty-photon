//! Shared trait surface for compound equipment-driving tools.
//!
//! Both [`super::auto_focus`] and [`super::center_on_target`] need a
//! `capture(duration) → document_id` operation against the live camera
//! adapter — extracted here so the trait has one canonical home rather
//! than being declared in each compound tool's module.
//!
//! `String` errors keep the trait surface deliberately simple: every
//! adapter ultimately wraps an Alpaca call whose error already carries
//! a human-readable message; structured-error variants would add
//! ceremony without giving the test-side synthetic adapters anything
//! to do.

use async_trait::async_trait;
use std::time::Duration;

/// Asynchronously capture an exposure of `duration` and return its
/// `document_id`.
///
/// The capture path's other side effects (FITS write, cache insert,
/// `exposure_complete` event) are the implementer's responsibility.
#[async_trait]
pub trait CaptureOps {
    async fn capture(&self, duration: Duration) -> Result<String, String>;
}
