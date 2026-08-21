# Plan: unified service lifecycle (issue #294)

**Status: COMPLETE (archived 2026-05-24; PR #295).** ALL PHASES LANDED. The crate at
`crates/rusty-photon-service-lifecycle/` ships the full runner (console
signal path + SCM dispatch behind the `scm` feature). All twelve
long-running services have been migrated, including the five shared-
transport ASCOM services that additionally closed the #287 outer-race
bug, and `phd2-guider` which gained SIGTERM handling as a side fix.
The `docs/skills/service-lifecycle.md` skill doc is published and
referenced from the workspace skill index. Issue #294 closes with
PR #295; issue #287 closes as a side fix in the same PR.

**Date:** 2026-05-22
**Branch:** `worktree-issue-294`
**Issue:** [#294 — look over all services and simplify/unify the signal handler installation](https://github.com/rusty-photon/rusty-photon/issues/294)
**Also fixes:** [#287 — outer `shutdown_signal` race cancels inner graceful shutdown](https://github.com/rusty-photon/rusty-photon/issues/287)
**Closest precedent:** [`docs/plans/shared-transport-extraction.md`](shared-transport-extraction.md) (multi-phase workspace-wide infra crate adopted by N services)
**Crate design doc:** [`docs/crates/rusty-photon-service-lifecycle.md`](../../crates/rusty-photon-service-lifecycle.md)
**Skill doc:** [`docs/skills/service-lifecycle.md`](../../skills/service-lifecycle.md)

## Outcomes

- **Phase 0** (design + scaffolding): landed. Crate skeleton + public
  API + design docs on PR #295. Copilot review fed back as a
  follow-up commit (`Shutdown::cancelled()` now returns `'static`,
  `Fut: Send` dropped, `todo!()` swapped for structured `Err`, etc.).
- **Phase 1** (crate implementation + filemonitor migration): landed.
  Runner builds its own multi-thread tokio runtime; no-panic
  signal-install path for SIGINT/SIGTERM/SIGHUP; SCM dispatch path
  gated by `#[cfg(all(windows, feature = "scm"))]` with type-erased
  closure stash; filemonitor migrated and its `service.rs` deleted.
  Unit tests cover runner contract (invokes once, propagates err,
  requires `.with_reload()`); Unix integration tests use
  `libc::raise(SIGTERM)` / `SIGHUP` to drive end-to-end.
- **Phase 2** (11 service migrations + phd2-guider SIGTERM fix):
  landed. Eleven commits on PR #295, one per service, in the order
  enumerated below. The five shared-transport services (`dsd-fp2`,
  `pa-falcon-rotator`, `ppba-driver`, `qhy-focuser`,
  `star-adventurer-gti`) additionally close issue #287 by threading
  the runner's `Shutdown` through `BoundServer::start(shutdown)`
  instead of installing a second signal handler internally. The
  `phd2-guider` migration adds a Unix-only regression test
  (`test_monitor_shuts_down_on_sigterm`) that spawns the binary in
  monitor mode, sends `libc::SIGTERM`, and asserts the process exits
  within 2s — the bounded-time clean-shutdown assertion called out
  in the plan. Per-service builds + tests + clippy + fmt clean at
  each commit; full `cargo rail run --profile commit -q` clean at
  PR head.
- **Phase 3** (skill doc + workspace docs polish): landed. Added
  [`docs/skills/service-lifecycle.md`](../../skills/service-lifecycle.md)
  covering the standard pattern, runner API, reload, SCM mode, and
  common pitfalls. Cross-references from
  [`docs/skills/development-workflow.md`](../../skills/development-workflow.md)
  (read-this-when-scaffolding-a-service list) and `CLAUDE.md` rule 1.b.

## Background

Every long-running binary in the workspace installs OS signal handlers
for graceful shutdown. Twelve services have drifted into roughly four
shapes for the same task:

1. **Pre-PR-289 `.expect()` panic-on-install pattern.** Eight call
   sites — `dsd-fp2` (twice, in both `main.rs` and `lib.rs`),
   `pa-falcon-rotator`, `ppba-driver`, `qhy-focuser`,
   `star-adventurer-gti`, `sky-survey-camera`, `rp`. The function is
   named `shutdown_signal()` in all of them and is structurally
   identical down to the bracing.
2. **Post-PR-289 no-panic pattern.** Three call sites — `sentinel`,
   `calibrator-flats`, `plate-solver`. Each adopts a longer
   `if let Err(e)` / `match` / `std::future::pending` shape that
   logs install failures via `tracing::warn!` instead of panicking.
   The three implementations differ among themselves (`plate-solver`
   races SIGTERM+SIGINT as a pair; `sentinel` drives a
   `CancellationToken` from a spawned task; `calibrator-flats` is
   closest to "standard").
3. **`phd2-guider`** — bug: inline `tokio::signal::ctrl_c()` only,
   no SIGTERM handler. `systemctl stop` / `kill -TERM` will not
   shut it down gracefully.
4. **`filemonitor`** — the most elaborate: SIGHUP-for-reload on Unix
   plus a full Windows Service Control Manager integration
   (`services/filemonitor/src/service.rs`, ~100 lines, the only
   `windows-service` crate adopter). Its `run_server_loop` accepts
   stop and reload signals as
   `FnMut() -> Pin<Box<dyn Future<Output = ()>>>` closures, which is
   awkward to test and reason about.

Net of PR #289: the divergence is actively *growing*. Three services
adopt a longer no-panic shape; eight remain on the panic shape; one
is buggy; one is special. The diversity is an artifact of building
these services over time, not a principled difference in what they
need.

### Coupled bug: issue #287

The five shared-transport ASCOM services (`dsd-fp2`, `qhy-focuser`,
`ppba-driver`, `pa-falcon-rotator`, `star-adventurer-gti`) compound
the duplication problem with an actual bug: each installs the signal
handler **twice** — an outer `shutdown_signal()` in `main.rs` racing
against `bound.start()`, plus an inner `shutdown_signal()`
constructed inside `BoundServer::start()` and passed to
`rp_tls::server::serve_plain` / `serve_tls`'s
`with_graceful_shutdown(...)`. When a signal fires, the outer
`select!` drops `bound.start()` mid-flight, which means axum's
inner graceful-shutdown drain never gets to flush in-flight requests
or run `Drop` impls promptly. Observable symptoms: spurious
connection-aborted log lines on the client side; possible loss of
`llvm-cov` profraw data.

This bug is structurally caused by the duplication this plan removes,
so it gets fixed as part of the same migration. Verified that the
other six services do *not* have the outer race — they only have the
single inner installation and just `bound.start().await?` in `main`.
The fix is scoped to the five shared-transport services.

The crate design itself —
[`docs/crates/rusty-photon-service-lifecycle.md`](../../crates/rusty-photon-service-lifecycle.md)
— covers scope, public API, behavior, and usage examples. This plan
covers only the *migration* from the four current shapes to the
unified one.

## Goals

1. **One propagation primitive** workspace-wide:
   `tokio_util::sync::CancellationToken`. Today, raw one-shot futures
   coexist with sentinel's `CancellationToken` and filemonitor's
   `Notify`-and-closures.
2. **One no-panic signal-install path** (the PR #289 pattern) in one
   place, used by every service.
3. **One signal-handler installation per service** — the
   `ServiceRunner` installs handlers once and produces a `Shutdown`
   handle that flows everywhere that needs it; nothing inside a
   service constructs its own. Closes #287.
4. **Filemonitor's reload-on-SIGHUP and Windows Service mode work
   under the same abstraction** as everyone else — no special-case
   shape inside filemonitor.
5. **Adding SCM mode to a second service in the future** is a ~3-line
   change to that service's `main.rs` plus a `Cargo.toml` feature
   flip, not a copy of `service.rs`.
6. **Migration is reviewable**: one commit per service, behavior-
   preserving except for the phd2-guider SIGTERM bug fix and the
   #287 fix in the five shared-transport services.

## Non-goals (of the migration)

* **Installer packaging.** No msi, winget, `sc create` scripts,
  systemd unit files. Today only `filemonitor` ships an SCM story;
  this plan extracts the *runtime code* so adding SCM to another
  service is cheap, but produces no installation artifacts. Tracked
  separately whenever the second SCM adopter materializes.
* **Forcing SCM adoption.** The crate *supports* SCM via an opt-in
  cargo feature; only `filemonitor` enables it in Phase 1. Other
  services may opt in later on their own timelines.
* **Wholesale rewriting of `BoundServer` / `ServerBuilder` across
  services.** The unified shutdown handle is passed *into* the
  existing server shape rather than reshaping it. The only API
  change is a narrow one — `BoundServer::start(self)` →
  `BoundServer::start(self, shutdown: impl Future<Output = ()> + Send)`
  in the five shared-transport services, so axum's
  `with_graceful_shutdown` consumes the unified source instead of
  installing its own. Required to fix #287.

## Migration decisions resolved

These resolve open questions raised during design discussion. Crate
design decisions (one crate not two, builder not macro, runner owns
runtime, naming, no `tracing` init in runner) are documented in the
[crate design doc](../../crates/rusty-photon-service-lifecycle.md);
migration-specific decisions live here.

* **Defer installer packaging.** Architectural payoff up front
  (unified `Shutdown` abstraction, SCM-as-source pattern), per-service
  SCM adoption + packaging deferred until needs arise. Avoids the
  "ocean-boil" of rebuilding the entire Windows lifecycle story
  across the workspace in one PR. Reversible: if SCM never spreads
  beyond filemonitor, no work is wasted.
* **One commit per service migration.** Each migration is small,
  behavior-preserving, and individually reviewable. Bundling them
  would obscure the per-service before/after.
* **phd2-guider SIGTERM bug fix lands as part of its migration
  commit**, not as a separate prior PR. The fix falls out of adopting
  the runner; isolating it would be paperwork without value.

## Phase 0 — design + scaffolding *(landed)*

* Crate design doc at
  [`docs/crates/rusty-photon-service-lifecycle.md`](../../crates/rusty-photon-service-lifecycle.md).
* This plan doc.
* Crate scaffold at `crates/rusty-photon-service-lifecycle/` with full
  public API surface, `//!` doc comments, and `todo!()` bodies on the
  runner methods. Compiles clean under both `--all-features` and
  default-features.
* Workspace `Cargo.toml` updated: added the crate to `members` and to
  `[workspace.dependencies]`.

## Phase 1 — crate implementation + filemonitor migration *(landed)*

Proves the design end-to-end (both signal-driven console mode and SCM
mode) before any other service touches the crate. Landed on PR #295.

### Crate implementation

* Implement `ServiceRunner::run` and `run_with_reload`:
  * Build a multi-thread tokio runtime.
  * Spawn the signal-watcher task; have it cancel a
    `CancellationToken` on first signal.
  * Spawn the reload-watcher task (SIGHUP / SCM ParamChange) if
    `with_reload`.
  * `block_on(run_fn(Shutdown::from_token(token)))`.
* Implement the SCM dispatch path behind `#[cfg(feature = "scm")]`.
  Stash config in a `OnceLock`; `ffi_service_main` re-enters the
  runtime build with SCM-driven token + reload signal.
* Crate unit tests — contract for `Shutdown`/`ReloadSignal`, the
  signal-install path (SIGTERM kicks the token), runner-invokes-
  closure-exactly-once. SCM dispatch is manually validated via
  filemonitor's existing Windows smoke test, not in CI. Detail in
  the crate design doc's [Testing
  section](../../crates/rusty-photon-service-lifecycle.md#testing).

### Filemonitor migration

* Delete `services/filemonitor/src/service.rs`.
* Rewrite `main.rs` `run_with_reload` body to use the runner.
* Change `run_server_loop` signature from two pinned-future-
  returning closures to `(CancellationToken, ReloadSignal)`.
* Update `docs/services/filemonitor.md` to reference the shared
  crate for the SCM/reload lifecycle, removing the duplicated
  explanation.

### Risks

* **Runtime ownership flip.** Most services use `#[tokio::main]`; the
  runner builds its own runtime. Confirmed during inventory that
  filemonitor already uses `Runtime::new()?`, so the SCM-side flip is
  zero-risk. For other services in Phase 2, the migration moves their
  `main` from `#[tokio::main] async fn` to plain `fn` wrapping the
  runner — same effective shape, no behavioral change.
* **Closure `Send + 'static` bound.** Services must `move` `args`
  into the closure. Idiomatic; no new lifetime gymnastics. Documented
  in the crate's [Public API
  section](../../crates/rusty-photon-service-lifecycle.md#public-api).
* **SCM regression in filemonitor.** The highest-risk migration
  because filemonitor is currently the only SCM adopter. Mitigated by
  preserving exact behavior (Stop→cancel, ParamChange→notify,
  Interrogate→NoError) and running filemonitor's existing Windows
  smoke test before merging.

## Phase 2 — remaining service migrations

One commit per service, ordered by complexity / risk (lowest first).
The five shared-transport services additionally fix #287; see
"Shared-transport variant" below for the extra steps they need.

1. `dsd-fp2` (shared-transport; fixes #287) — has the duplicate-
   function quirk; collapses to one call site, easiest validation.
2. `pa-falcon-rotator` (shared-transport; fixes #287)
3. `ppba-driver` (shared-transport; fixes #287)
4. `qhy-focuser` (shared-transport; fixes #287)
5. `star-adventurer-gti` (shared-transport; fixes #287)
6. `sky-survey-camera`
7. `rp`
8. `calibrator-flats` (axum-style)
9. `plate-solver` (axum-style; also drops the redundant SIGINT
   handler)
10. `sentinel` (drops the hand-rolled `tokio::spawn` + token plumbing;
    sources `shutdown.token()` directly into the engine)
11. `phd2-guider` (gains SIGTERM as a side fix — a small behavioral
    change; add a regression test asserting clean shutdown within
    bounded time on SIGTERM)

### Standard migration (all services)

Each commit:

* Replaces `shutdown_signal()` (or equivalent) with
  `ServiceRunner::run`.
* Deletes the now-unused `use tokio::signal` / helper imports.
* Updates the service's `docs/services/<name>.md` if shutdown is
  documented there (most aren't).

### Shared-transport variant (services 1–5 — fixes #287)

In addition to the standard migration, each shared-transport service
performs three extra mechanical changes to eliminate the
double-installation race:

* **`lib.rs`** — `BoundServer::start(self)` →
  `BoundServer::start(self, shutdown: impl Future<Output = ()> + Send)`.
  The inner `serve_plain` / `serve_tls` call passes `shutdown`
  through to `with_graceful_shutdown(...)` instead of constructing
  a fresh `shutdown_signal()`. Delete the per-service
  `async fn shutdown_signal()` in `lib.rs` entirely.
* **`main.rs`** — drop the outer `tokio::select!`. The call becomes
  `bound.start(shutdown.cancelled()).await?` — one signal source,
  no race. The `info!("Shutting down")` log line moves into the
  `ServiceRunner` (already logs at `info!` level on shutdown), so
  it still fires once per shutdown event.
* **Existing SIGTERM tests** in `services/*/tests/test_lib.rs`
  continue to pass; add at least one regression assertion (per
  service or shared) that an in-flight request started just before
  SIGTERM is allowed to complete before the process exits.

`rp_tls::server::serve_plain` and `serve_tls` already accept a
shutdown future as their third parameter — no `rp-tls` API change
needed.

## Phase 3 — workspace docs polish

* Add `docs/skills/service-lifecycle.md` capturing the standard
  pattern, the runner API, and a "how to enable SCM mode" section
  pointing at filemonitor as the worked example.
* Update `docs/skills/development-workflow.md` to reference the new
  skill doc when scaffolding a new service.
* Update `CLAUDE.md` if its service-creation guidance lists signal
  handling as boilerplate.

## Future work (explicitly deferred)

* **Per-service SCM adoption.** `sentinel` is the most plausible next
  adopter (unattended, long-running, no client coupling). When that
  happens, the change is `+ features = ["scm"]` in its `Cargo.toml`
  plus `.scm_mode(args.service)` in main. No new SCM code.
* **Installer packaging.** msi/winget/sc-create scripts; systemd unit
  files. Tracked separately once the second SCM adopter materializes
  (so the install story isn't one-off-shaped).
* **Shutdown timeout** — `ServiceRunner::with_shutdown_timeout(d)`.
  Adds "if user closure hasn't returned within `d` after cancellation,
  log error and exit anyway." Useful for catching hangs; not needed
  today.
* **Structured shutdown reason.** Today `Shutdown::cancelled()` is
  `() -> ()`. Could become
  `() -> ShutdownReason::{CtrlC, Sigterm, Sighup, ScmStop, ...}` for
  diagnostic logging. Additive, postpone.
* **systemd readiness notifications** (`sd_notify(READY=1)`). For
  services started under systemd `Type=notify`. Additive on the
  runner; postpone until there's an asker.

## Pre-Phase 1 baseline inventory (reference)

**Frozen snapshot.** This is the codebase at the PR #289 baseline
that motivated the unification — *not* a live status board. Track
per-phase progress in the [Outcomes](#outcomes) section above;
filemonitor in particular has moved off this row (Phase 1 deleted
`service.rs` and migrated `run_server_loop` to the runner). The
table stays unchanged to preserve the divergence picture that
justified the design.

One row per shutdown call site:

| Service | Path | Pattern | Reload | SCM |
|---|---|---|---|---|
| `dsd-fp2` | `services/dsd-fp2/src/main.rs:44` + `lib.rs:168` | OLD (`.expect`) — both copies; **also #287 outer race** | — | — |
| `pa-falcon-rotator` | `services/pa-falcon-rotator/src/main.rs:47` + `lib.rs` | OLD; **also #287** | — | — |
| `ppba-driver` | `services/ppba-driver/src/main.rs:168` + `lib.rs` | OLD; **also #287** | — | — |
| `qhy-focuser` | `services/qhy-focuser/src/main.rs:46` + `lib.rs` | OLD; **also #287** | — | — |
| `star-adventurer-gti` | `services/star-adventurer-gti/src/main.rs:52` + `lib.rs` | OLD; **also #287** | — | — |
| `sky-survey-camera` | `services/sky-survey-camera/src/lib.rs:141` | OLD (single install) | — | — |
| `rp` | `services/rp/src/lib.rs:243` | OLD (single install) | — | — |
| `phd2-guider` | `services/phd2-guider/src/main.rs:269` | BUGGY (Ctrl+C only) | — | — |
| `sentinel` | `services/sentinel/src/lib.rs:234` | NEW + `CancellationToken` | — | — |
| `calibrator-flats` | `services/calibrator-flats/src/lib.rs:104` | NEW + axum graceful | — | — |
| `plate-solver` | `services/plate-solver/src/lib.rs:126` | NEW + SIGTERM/SIGINT pair | — | — |
| `filemonitor` | `services/filemonitor/src/main.rs:51` (console) + `service.rs` (SCM) | OLD (Phase 1 deletes) | **SIGHUP** | **windows-service** |
