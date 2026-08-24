#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! Refcounted multi-client lifecycle scaffolding for duplex transports.
//!
//! This crate factors out the connect-handshake-share-teardown pattern that
//! every ASCOM service in this workspace had grown independently
//! (`qhy-focuser`, `ppba-driver`, `pa-falcon-rotator`,
//! `star-adventurer-gti`). It dissolves three bug classes structurally:
//! the `set_connected` lock-holding race, the refcount leak on
//! partial-connect failure, and the polling-task teardown leak.
//!
//! # The shape
//!
//! ```text
//! Service Manager  ───►  Arc<SharedTransport<C>>
//!                              │  acquire()
//!                              ▼
//!                          Session<C>  ──► request(cmd) → C::Response
//!                              │  close().await   ◄── primary teardown
//!                              │  Drop            ◄── detached fallback
//! ```
//!
//! [`SharedTransport`] holds the refcount, the slot, and the open-state
//! lock. [`Session`] is the handle a service hands to its ASCOM device
//! types; one device = one session. The first `acquire` runs the
//! handshake; the last drop runs teardown. A `while_open` task (e.g. a
//! poll loop) can be configured via [`Hooks`] — its lifetime tracks the
//! transport's, not any individual session's.
//!
//! Codec authors implement [`Codec`] to translate between protocol
//! commands and on-wire frames. Splitting the byte stream into frames —
//! reading until a terminator on serial, taking one datagram on UDP — is
//! the [`FrameTransport`] implementation's job; *emitting* any in-frame
//! terminator the protocol carries on the wire (e.g. `\r` for
//! Sky-Watcher, `}` for qhy-focuser's JSON) is the codec's
//! responsibility in [`Codec::encode`].

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

pub mod codec;
pub mod connection;
pub mod error;
pub mod session;
pub mod shared;
pub mod transport;

pub use codec::Codec;
pub use connection::Connection;
pub use error::{SessionError, TransportError};
pub use session::{Hooks, Session, WhileOpen};
pub use shared::SharedTransport;
pub use transport::{FrameTransport, SerialFrameTransport, TransportFactory, UdpFrameTransport};

/// Pinned, heap-allocated, Send-able future used by [`Hooks`] closures.
///
/// Equivalent to `futures::future::BoxFuture`, redefined here so the crate
/// has no `futures` dependency.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
