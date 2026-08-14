---
applyTo: "**/*.rs"
---

# Reviewing Rust in this workspace

## Do not reason about whether it compiles

`cargo clippy -- -D warnings` (two passes: `--all-features
--all-targets`, and default-features `--lib --bins`) and a
Linux + Windows Bazel build have already passed on this diff; macOS
follows on merge. Never
raise a borrow, move, lifetime, trait-resolution, import or lint
concern — you cannot add information the compiler has already settled,
and no such comment here has ever produced an improvement.

In particular, these are all valid and have each been wrongly reported:

- `format!` and `assert!` bind their arguments by reference through
  `format_args!`, and `serde_json`'s `json!` serializes interpolated
  values via `to_value(&…)`; interpolating a `String` field of a
  borrowed struct moves nothing.
- `matches!(x, Variant)` and `if let Variant = x` with no bindings do
  not move the scrutinee.
- Disjoint field access on a non-`Drop` struct is legal after a
  partial move of a different field.
- `macro_rules!` uses mixed-site hygiene: a macro body may name items
  the invoking module has not imported.

## Where to look instead

**Async and locking.** Guards held across `.await`; read-then-write
sequences on a shared lock that another task can interleave; state
flags set after the action they are meant to guard rather than before;
spawned tasks with no abort on timeout, error or shutdown; work that
must be cancelled when a newer operation supersedes it.

**Error paths.** Refcounts, connection state or cached values not
rolled back when a later step fails, leaving the object wedged until
process restart. Errors flattened to `String`, dropping the variant
that callers switch on. `unwrap_or`, `unwrap_or_default` and `let _ =`
that convert a failure into a plausible-looking value — say what wrong
behavior the masked error produces.

**Numeric and unit correctness.** Silent truncation into a narrower
wire encoding, casts that wrap, non-finite floats reaching arithmetic
that assumes finiteness, mixed units (arcseconds vs degrees, hours vs
degrees, µm vs mm, steps vs counts) crossing an API boundary.

**Deserialization and config.** Structs accepting external input
without `deny_unknown_fields` so a typo silently disables a feature;
`#[serde(default)]` producing a valid-looking but wrong default;
validation applied at config load but skipped on the equivalent
runtime or API path.

**Hardware safety.** No code reachable from service startup, driver
connect or reconnect, config apply, or a passive/supervisory
transition may actuate hardware — no motion, homing, park slews,
cover or lamp changes, cooler setpoints, power or dew toggles, filter
moves or guide pulses. Stop-class commands and cleanup inside an
operator-started session are allowed. Flag any new call on a connect
or handshake path that moves a device.

## Conventions worth flagging

- `debug!` for routine logging; `info!` only for events an operator
  benefits from, such as a service becoming ready.
- Comments describe current behavior only. Flag comments that narrate
  history, name a PR or issue number, or explain what changed.
- A dependency used by more than one service belongs in the workspace
  `Cargo.toml`, not a per-crate one.

## Do not

Do not suggest adding retry or readiness loops to make a test or
client tolerate a race — this repo treats those as masking real bugs
and rejects them. Report the race itself instead.

Do not propose a fix that only narrows the failure window; if the
correct fix needs a structural change, say so plainly.
