#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! Unified service lifecycle for Rusty Photon binaries.
//!
//! Every long-running service in this workspace needs the same three things:
//! a tokio runtime, OS signal handlers for graceful shutdown, and a way to
//! propagate "time to stop" to its workers. This crate is the single
//! workspace-wide replacement for the per-service boilerplate that grew
//! up around those needs.
//!
//! Full design and usage: `docs/crates/rusty-photon-service-lifecycle.md`.
//! Migration plan from the existing per-service shutdown handlers:
//! `docs/plans/archive/service-lifecycle-unification.md` (issue #294).
//!
//! The public surface is small:
//!
//! * [`ServiceRunner`] — builder that owns the tokio runtime, installs signal
//!   handlers (or dispatches to the Windows Service Control Manager when the
//!   `scm` feature is on and `scm_mode(true)` is set), and invokes a user
//!   closure with a [`Shutdown`] handle.
//! * [`Shutdown`] — thin wrapper around [`tokio_util::sync::CancellationToken`].
//!   Hand `shutdown.token()` to spawned subtasks; race `shutdown.cancelled()`
//!   in `tokio::select!`; pass `shutdown.cancelled()` to
//!   `axum::serve(...).with_graceful_shutdown(...)`.
//! * [`ReloadSignal`] — opt-in (via [`ServiceRunner::with_reload`]) reload
//!   notifier, driven by `SIGHUP` on Unix or `ServiceControl::ParamChange` in
//!   SCM mode. On Windows console mode it never fires.
//! * [`init_tracing`] — installs the shared tracing subscriber: logs to stderr
//!   (stdout is reserved for the `bound_addr=` handshake), filtered by `RUST_LOG`
//!   or a fallback level.
//! * [`init_service_tracing`] — what every service binary calls at startup:
//!   [`init_tracing`] in console mode, but in Windows SCM service mode
//!   (`--service`, `scm` feature) it writes to a rolling log file under
//!   `%PROGRAMDATA%\rusty-photon\logs\` instead of the dead stderr handle.
//!   Returns a [`TracingGuard`] that `main` must hold until process exit so
//!   the final lines flush on SCM Stop.
//!
//! * [`ServiceResult`] — the result type service `main`s return. Its error
//!   side is [`Report`], so startup failures and fatal exits print a readable
//!   multi-line `source()` chain instead of a one-line `Debug` dump. Run
//!   closures return [`RunResult`] (a boxed [`RunError`]) instead — the runner
//!   converts that into the `Report` at its own boundary, so closures keep
//!   plain `?` ergonomics on typed and boxed errors alike. The runner also
//!   installs the `color-eyre` panic hook (once per process), so panics print
//!   formatted reports with span context (via the `ErrorLayer` that
//!   [`init_tracing`] composes in). Per ADR-011 this crate is the **only**
//!   owner of `color-eyre`: errors stay `thiserror`-typed everywhere below the
//!   binary boundary, and only the *types* are re-exported here — never the
//!   `eyre!`/`bail!` macros.
//!
//! ## Minimal example
//!
//! ```no_run
//! use rusty_photon_service_lifecycle::{ServiceResult, ServiceRunner};
//!
//! fn main() -> ServiceResult {
//!     ServiceRunner::new("example-service").run(|shutdown| async move {
//!         // Race the server against shutdown.
//!         tokio::select! {
//!             _ = std::future::pending::<()>() => {}
//!             _ = shutdown.cancelled() => tracing::debug!("shutdown requested"),
//!         }
//!         Ok(())
//!     })
//! }
//! ```
//!
//! ## Behavioral guarantees
//!
//! * **No panic on signal install failure.** Both `tokio::signal::ctrl_c()`
//!   and `tokio::signal::unix::signal(...)` errors are logged via
//!   `tracing::warn!` and replaced with a never-resolving future. This
//!   matches the pattern established by PR #289.
//! * **Cross-platform.** On Unix: `SIGINT` (Ctrl+C) + `SIGTERM`, plus `SIGHUP`
//!   if `with_reload()` is set. On Windows console: Ctrl+C only. On Windows
//!   SCM mode: `ServiceControl::Stop` + `ServiceControl::ParamChange`.
//! * **Single propagation primitive.** Both the signal-handler task and SCM
//!   control-handler callback feed the same [`CancellationToken`]. Consumers
//!   never need to know which source triggered shutdown.

#![deny(unsafe_code)]
// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
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

// Every binary in this workspace depends on this crate, so this is the one
// place a workspace-wide target guard reaches all of them. Cargo.toml cannot
// express it — `compile_error!` in the root of the common dependency can.
//
// The workspace assumes a little-endian target throughout: the catalog format
// (`rp-catalog`), the mount wire protocols (`skywatcher-motor-protocol`,
// `pa-falcon-rotator`), and the camera SDK buffers all read and write native
// integers without byte-swapping. None of it has ever been built or validated
// big-endian. Failing at compile time is honest; silently decoding a star
// catalog or an encoder position with the bytes reversed is not.
#[cfg(not(target_endian = "little"))]
compile_error!(
    "Rusty Photon supports little-endian targets only. Wire protocols, the star \
     catalog, and camera buffers are all decoded natively without byte-swapping, \
     so a big-endian build would misread them silently. If you need big-endian \
     support, open an issue — the decoders need auditing first."
);

mod logging;
mod reload;
mod runner;
mod shutdown;

pub use logging::{init_service_tracing, init_tracing, TracingGuard};
pub use reload::ReloadSignal;
pub use runner::{
    is_scm_service, report_from_boxed, RunError, RunResult, ServiceResult, ServiceRunner,
};
pub use shutdown::Shutdown;

/// Re-exported so services can *name* the error type in signatures (e.g.
/// `plate-solver`'s `ExitCode` path) without depending on `color-eyre`
/// themselves. Deliberately a type-only re-export: the `eyre!`/`bail!`
/// macros are NOT re-exported. Constructing a `Report` directly (e.g.
/// `Report::msg(...)`) remains *possible* through this re-export, but it is
/// against workspace policy outside the binary boundary — errors below it
/// stay `thiserror`-typed, and the crate-local dependency on `color-eyre`
/// is the structural guardrail (ADR-011).
pub use color_eyre::Report;
