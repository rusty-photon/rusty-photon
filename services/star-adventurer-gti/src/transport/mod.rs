//! Transport factories — `rusty-photon-shared-transport`-shaped.
//!
//! Each per-transport module ([`serial`], [`udp`], [`mock`] under the
//! `mock` feature) provides a [`TransportFactory`] that the shared
//! crate's [`SharedTransport`] calls for every conduit it opens — the
//! cold `start()` and each reconnect attempt. Framing — terminator-delimited for
//! serial (`\r`), datagram-bounded for UDP — lives inside the
//! [`FrameTransport`] each factory hands back.
//!
//! [`FrameTransport`]: rusty_photon_shared_transport::FrameTransport
//! [`SharedTransport`]: rusty_photon_shared_transport::SharedTransport
//! [`TransportFactory`]: rusty_photon_shared_transport::TransportFactory

pub mod serial;
pub mod udp;

#[cfg(feature = "mock")]
pub mod mock;
