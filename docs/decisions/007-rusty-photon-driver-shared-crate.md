# ADR-007: Extract `rusty-photon-driver` — the shared ASCOM-driver adapter

## Status

Accepted (2026-06-01). Implemented on the `feature/config-actions-phase3`
branch (PR #344), bundled with the Phase 3 config-actions work that surfaced the
duplication.

Amended 2026-10-25: items 1 and 2 of the decision (the config-action dispatch
and `apply_error_to_ascom`) retired with the config actions, which the setup
pages replaced ([driver-setup-pages.md](../plans/driver-setup-pages.md), P5).
The crate keeps `driver_error!`, and gained the glue from the pinned crate's
setup hooks to `rusty-photon-setup`'s pages, `connected_transition`, the
`overrides` pages and the `claims` feature.

## Context

Phase 3 of the config-actions work (see
[`docs/plans/archive/config-actions-protocol.md`](../plans/archive/config-actions-protocol.md)) generalised
`config.get` / `config.apply` / `config.schema` across all six Alpaca driver
services. The driver-agnostic *protocol* logic (the `ConfigurableDriver` trait and
the `config_get` / `config_apply` / `config_schema` functions) was correctly
factored into [`rusty-photon-config`](../../crates/rusty-photon-config) — which
stays free of `ascom-alpaca` because the non-Alpaca `rp` and `sentinel` services
consume it too. But the **ASCOM-facing glue** ended up copy-pasted into every
driver:

- the `config.apply` `ApplyError → ASCOMError` match (**6×** — four in a
  per-driver `dispatch`, two inlined in `device.rs`);
- a per-driver `ConfigActionCtx` + `supported_actions` + `dispatch` (**6×**,
  near-identical, with one inconsistency: sky-survey routed `ApplyError::Serialize`
  through a local `ser_err`);
- a per-driver error enum sharing a ten-variant common core
  (`NotConnected`, `Io`, `Timeout`, `Communication`, …) plus `to_ascom_error` /
  `From<DriverError> for ASCOMError` / `From<TransportError>` (**5×**), with
  **109** near-duplicate unit tests.

This duplication was also the bulk of the PR's uncovered (`codecov/patch`) lines,
because hand-rolled glue is awkward to unit-test per driver.

## Decision

Introduce a new workspace crate **`rusty-photon-driver`** — the ASCOM-driver
*adapter / runtime layer* — depending one-way on `rusty-photon-config`,
`ascom-alpaca`, `rusty-photon-shared-transport`, and
`rusty-photon-service-lifecycle`. It is consumed only by the six driver services.

It provides:

1. **`dispatch::<D>` / `supported_actions` / `ConfigActionCtx<D>`** — the generic
   ASCOM config-action dispatch (Get/Schema/Apply, the fire-after-response reload),
   generic over the driver's `ConfigurableDriver` marker. Each driver's
   `Device::action` / `Device::supported_actions` delegate here.
2. **`apply_error_to_ascom`** — the `ApplyError → ASCOMError` mapping, as a free
   function (see *orphan rule* below).
3. **`driver_error!`** — a macro that *generates* each driver's error enum: the
   ten common transport-driver variants + their ASCOM classification +
   `From<TransportError>` + a `Result<T>` alias, with device-specific variants and
   ASCOM-code overrides spliced in.

### Why a separate crate, not folded into `rusty-photon-config`

`rusty-photon-config` is the transport-/consumer-agnostic config *model* and is
on a path to a standalone, vendor-neutral crate. The shared `DriverError` is a
*device/transport* error, not config; and giving the config crate an (even
optional) `ascom-alpaca` / `tokio` / transport dependency would leak Alpaca into
the `rp` / `sentinel` config-consumer graph. Keeping the adapter in its own crate
preserves a clean one-directional `driver → config` boundary. (`ascom-alpaca`
implements the open ASCOM-Alpaca standard, so depending on it is not a vendor
lock-in — the concern is dependency *reach*, not neutrality.)

### Why a macro for the error model (not a shared `DriverError` enum)

Each driver also has `impl From<SessionError<XxxCodecError>> for XxxError`. The
orphan rule requires `XxxError` to be a **local** type for that impl, so the
drivers cannot simply alias or wrap a foreign `rusty_photon_driver::DriverError`
without forcing hundreds of call-site changes (every `XxxError::NotConnected`
becoming `…::Common(DriverError::NotConnected)`). The `driver_error!` macro
instead generates a **flat, local** enum per driver: the common core is defined
once (in the macro), call sites are unchanged, the `From<SessionError<…>>` impl
stays orphan-legal, and macro-expanded coverage is attributed to the macro's
definition site (so the per-driver `error.rs` files shrink to a single
invocation). The common-variant logic is unit-tested **once** in the crate.

### Why `apply_error_to_ascom` is a free function

`impl From<ApplyError> for ASCOMError` is orphan-illegal in `rusty-photon-driver`
(both `ApplyError` and `ASCOMError` are foreign to it). The only legal homes are
`rusty-photon-config` (kept ascom-free) or `ascom-alpaca` (not ours). With a
single caller (the dispatch's apply arm), a free function is the pragmatic choice.

### `ascom-alpaca` feature selection

The crate uses only the unfeatured core ASCOM error types, but `ascom-alpaca`
`compile_error!`s without one device and one network feature. We enable
`["__anydevice", "server"]`: `__anydevice` is the empty marker that satisfies the
device guard **without** pulling a real device trait (e.g. `cover_calibrator`)
into every driver's graph via feature unification; `server` is the lightest real
network feature (required because `api::device_state`'s serde derives are gated on
`client`/`server`, not the marker) and is already enabled by every driver.

## Consequences

- **Net deletion** across the six drivers: the 6 `ApplyError` blocks, 6
  dispatch/ctx copies, 5 error enums, and 109 duplicate tests collapse to one
  tested crate. A new driver gets the error model + ASCOM dispatch for free.
- **Standardised error messages.** The macro's common-variant `Display` strings
  are generic (`"not connected"` rather than `"Not connected to <device>"`). The
  ASCOM error *codes* are unchanged; the prose differs and a handful of test
  assertions were updated. This is a deliberate, accepted trade for the dedup.
- The `config.get` / `config.apply` / `config.schema` **wire contract is
  unchanged** — verified by every driver's `config_actions.feature` BDD suite.
- Bazel: a new `crates/rusty-photon-driver/BUILD.bazel`, the dep added to each
  driver's `_INTRA_WORKSPACE_DEPS`, and a `crate_universe` repin
  (`MODULE.bazel.lock`). No `MODULE.bazel` change (workspace members auto-discovered).
