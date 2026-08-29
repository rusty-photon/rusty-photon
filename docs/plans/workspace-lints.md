# Workspace Lints Plan — deny the panic classes, on a measured ladder

## Goal

The workspace denies nine ways to panic as of L4 — `unwrap_used`,
`expect_used`, `unreachable`, `panic`, `todo`, `unimplemented`,
`panic_in_result_fn`, `unchecked_time_subtraction`, `string_slice`. What is
left of the target set is `indexing_slicing`, `arithmetic_side_effects`,
`as_conversions` and the `pedantic` / `nursery` groups (`exit` was measured
and deliberately dropped from the target — see L4):

```toml
[lints.clippy]
pedantic = { level = "deny", priority = -1 }
nursery  = { level = "deny", priority = -1 }
unwrap_used = "deny"
expect_used = "deny"
indexing_slicing = "deny"
arithmetic_side_effects = "deny"
unreachable = "deny"
unimplemented = "deny"
unchecked_time_subtraction = "deny"
todo = "deny"
string_slice = "deny"
panic_in_result_fn = "deny"
panic = "deny"
exit = "deny"                # measured, then deliberately dropped — see L4
as_conversions = "deny"
```

This matters for [tenet 2 (robustness)](../workspace.md#project-tenets): a panic
in a driver at 2am ends the night's imaging. The lints that close panic routes
are the point; the `pedantic` / `nursery` groups are a separate, much larger
style question that this plan deliberately sequences last.

## Measured baseline

Numbers below are the **pre-L1** census; each phase re-measures before it runs,
because the earlier estimate is reliably wrong once the knobs and the previous
phase's fixes are in (L3 in particular came in far cheaper than sized here).

Census taken with clippy 0.1.96 on `--workspace --all-targets --all-features`,
driving the full proposed set as `-W` flags so every crate still completes its
check pass. **The tree is warning-clean today (0 diagnostics)**, so every number
below is new debt. Nothing fails to *build* — at `deny` these become errors, but
each is a lint, not a compile failure.

For the 42 crates that inherit `[workspace.lints]`:

| Bucket | Sites | `--fix` can do | Hand-fix |
|---|---:|---:|---:|
| Production (lib/bin) | 4,853 | 2,234 | 2,619 |
| Test-side | 6,703 | 1,287 | 5,416 |
| **Total** | **11,556** | **3,521** | **8,035** |

**The named lints are the cheap part.** Only 1,054 of the 4,853 production
sites come from the thirteen named lints; the other 3,799 are `pedantic` /
`nursery` fallout.

| Named lint | Prod | Named lint | Prod |
|---|---:|---|---:|
| `arithmetic_side_effects` | 387 | `string_slice` | 25 |
| `as_conversions` | 368 | `panic` | 19 |
| `indexing_slicing` | 214 | `unchecked_time_subtraction` | 2 |
| `exit` | 39 | `unwrap`/`expect`/`unreachable`/`todo` | **0** |

### `clippy.toml` changes the test-side picture

Clippy lints test code by default — 3,891 of the test-side sites are in
`tests/` directories and 2,812 are in `#[cfg(test)] mod` blocks inside `src/`.
A repo-root `clippy.toml` suppresses four of them at source, in test scope
only, leaving production untouched:

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
allow-panic-in-tests = true
allow-indexing-slicing-in-tests = true
```

| | Without | With knobs |
|---|---:|---:|
| Production | 4,853 | 4,853 |
| Test-side | 6,703 | **5,020** |
| **Total** | **11,556** | **9,873** |

Three measured limits:

1. **Only eight knobs exist** — `dbg`, `expect`, `indexing-slicing`,
   `large-stack-frames`, `panic`, `print`, `unwrap`, `useless-vec`. There is
   none for `as_conversions`, `arithmetic_side_effects`, `string_slice`,
   `unreachable`, `todo`, `unimplemented`, or `exit`.
2. **Nothing in `pedantic` / `nursery` is covered.** All 3,844 test-side group
   sites survive, including `needless_pass_by_ref_mut`'s 1,171.
3. **The knobs only recognise `#[cfg(test)]` mods and `#[test]` fns.** The 682
   surviving `panic` / `indexing_slicing` test sites are dominated by
   `tests/bdd/steps/*.rs` and `tests/bdd/world.rs` — cucumber's
   `#[given]`/`#[when]`/`#[then]` are not `#[test]` functions, so clippy does
   not classify them as test code. L3 found the tail is broader than that:
   also plain `tests/*.rs` targets other than `bdd.rs`, and panics inside
   closures and `tests/common/` helpers that a `#[test]` fn merely calls.

### The knobs make most existing `#[allow]`s dead

Measured with `--force-warn` (which overrides `#[allow]` but *cannot*
resurrect a lint the knob suppressed at source, so it isolates exactly the
load-bearing attributes). Of 461 clippy allow attributes, 408 touch the trio:

| Lint | Files with allow | Still fire | **Dead** |
|---|---:|---:|---:|
| `unwrap_used` | 365 | 18 | **347** |
| `expect_used` | 363 | 25 | **338** |
| `unreachable` | 348 | 11 | **337** |
| `indexing_slicing` | 1 | 0 | **1** |

Applied in L1: **329 attributes deleted outright, 67 trimmed** to only the lint
that still fires — 470 clippy allow attributes down to 144.
Separately, 335 of the 348 files declaring `allow(clippy::unreachable)` contain
no `unreachable!()` at all: the lint was carried along by copy-paste from the
canonical snippet.

**Scope, not file, decides whether an allow is dead.** A per-file model is
wrong and fails loudly — `crates/bdd-infra/src/lib.rs` carries a crate-root
`#![allow(...)]` that covers every module in the package, and `bdd-infra` is
ordinary lib code rather than `#[cfg(test)]`, so the knobs never applied to it.
Three scopes have to be resolved before a removal is safe:

| Attribute | Scope |
|---|---|
| inner `#![allow]` in a file's header region | the whole package |
| outer `#[allow]` on a `mod name;` declaration | that module's file subtree, honouring `#[path = "..."]` |
| anything else | the file it sits in |

### Blast radius

Cargo only. Bazel never runs clippy (`.bazelrc` mentions it once, in a
comment), and `[lints.clippy]` is a Cargo feature that rules_rust does not
read. Affected: the pre-commit hook, the required `stable / clippy` PR gate,
the nightly `beta / clippy` early-warning job — which reports rather than
gates, so widening the deny set cannot make it red (L6a) — and, since #984's
fix, the off-PR `windows / clippy` + `macos / clippy` legs, which enforce the
same set on OS-cfg'd code and CAN go red on main when the set widens: a
widening phase must census the OS-cfg surface too, not just linux-gnu.

### Crates this does not reach

`qhyccd-rs`, `zwo-rs`, `svbony-rs` and their three `-sys` shims have no
`[lints] workspace = true` — they are dual-homed to crates.io per
[ADR-009](../decisions/009-vendor-qhyccd-rs.md) /
[ADR-010](../decisions/010-vendor-zwo-rs.md). They carry 1,038 sites even with
the knobs, including **every** `unwrap_used` (664) and `expect_used` (57) site
in the workspace. Phase 7.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| L0 | This plan | Complete | #827 |
| L1 | `clippy.toml` + dead-allow sweep + the four free lints | Complete | #827 |
| L3 | Deny `panic` — test-crate-root allows | Complete | #831 |
| L4 | Deny `string_slice`; leave `exit` alone | Complete | #831 |
| L6a | Split the CI channels: beta reports, stable gates | Complete | #839 |
| L2 | Mechanical `cargo clippy --fix` sweep | Complete | #846, #850 |
| L5 | `as_conversions`, `arithmetic_side_effects`, `indexing_slicing` | Complete | #854 (sign flips), #863 (step params); L5a complete in #862/#864; L5b in #870/#871/#878, SDK frame buffers in #883, QHY index casts in #890; L5c in #895 (pixel loops), #904 (value math), #908 (star geometry, noise source, tail, CFW codec / buffer copies) — **`qhyccd-rs` production code now at zero**; L5d (the three camera services' gain/offset range) in #912; L5e (the rest of the camera services, to zero) in #921; L5f (`rp-catalog` to zero) in #931; L5g (`skywatcher-motor-protocol` to zero) in #932; L5h (`star-adventurer-gti` to zero) in #935/#936; L5i (`rp-fits` to zero) in #938; L5j (`session-runner` to zero) in #939; L5t (test-side allows, workspace-wide) in #945; L5k (`polar-align` to zero) in #947; L5l (`phd2-guider` to zero) in #948; L5m (`rp-ephemeris` to zero) in #950; L5n (`ppba-driver` to zero) in #957; L5o (`doctor` + `rusty-photon-doctor-checks` to zero) in #958; L5p (`pa-falcon-rotator` to zero) in #959; L5q (`sky-survey-camera` to zero) in #963; L5r (`rusty-photon-server-config` to zero) in #965; L5s (`sentinel` to zero) in #966, folded into #965; L5u (`rusty-photon-config` + `shared-transport` + `tls` to zero) in #969; L5v (`bdd-infra` to zero, all-features) in #971; L5w (six services' mock/feature code to zero) in #972; L5x (`rp` star detection to zero) in #973; L5y (rest of `rp` imaging to zero) in #975; L5z (`rp` MCP layer to zero) in #976; L5aa (`rp` + `rp-targets` + example to zero) in #979 (the L5w–L5aa train); L5ab (the five residual services to zero — every `[lints]`-inheriting crate's production code at zero; FFI crates stay L7) in #980; deny flip after a fresh full census |
| L6b | `pedantic` / `nursery` at deny | Complete | B0–B8 by-lint/per-crate slices; B9 doc sub-rung (B9a–B9h, 781 sites, #1035/#1043/#1046/#1049/#1052/#1055+#1057/#1059/#1061); deny flip 2026-08-24 |
| L7 | Dual-homed FFI crates | In progress — mechanism decided, zwo family underway | |

**L6 split in two, and L2 moved back ahead of the policy half.** The original
sequencing note put L2 after L6 because L6's standing recommendation was
`pedantic = "warn"` with `nursery` off — under which L2's ~3,521-site `--fix`
sweep would have paid for fixes to lints the workspace never gates on.

That recommendation existed for one reason: both groups gain lints on the beta
channel, so denying them made the nightly `beta / clippy` job recurrently red.
That is a CI-policy problem, not a lint-policy one, and L6a fixes it directly —
beta now reports instead of failing. With the objection removed, `pedantic` and
`nursery` at `deny` on stable become viable (L6b), which restores the case for
L2: it is work the workspace will actually enforce.

L6a does not shrink the 7,643 sites. It removes the reason not to pay for them.

---

## L1 — `clippy.toml` + dead-allow sweep + the four free lints

One PR, mechanical, no per-site judgment.

- Add the four-key `clippy.toml` at the repo root.
- Delete the 360 dead attributes; trim the other 48 to the lints that still
  fire. **Same commit as the `clippy.toml`** — removing allows first would
  break the existing deny.
- Deny `todo`, `unimplemented`, `panic_in_result_fn`,
  `unchecked_time_subtraction`. Four lints for five fixes:
  - `services/session-runner/src/engine/exec.rs:547,575` — both auto-fixable
  - `services/pa-falcon-rotator/src/rotator_device.rs:355`
  - `services/pa-falcon-rotator/src/switch_device.rs:408`
  - `services/qhy-camera/src/backend.rs:784`
- Rewrite the `[workspace.lints.clippy]` comment block in `Cargo.toml`: it
  documents the per-test-module attribute pattern that this phase largely
  deletes. Cross-check `docs/workspace.md` and `docs/skills/testing.md`
  (rules 2 and 11).

**Verification.** Re-run the `--force-warn` census; the surviving set must
match the 48 kept attributes exactly, with no new diagnostics.

## L2 — mechanical sweep

`cargo clippy --fix` per crate, one PR per crate so review stays tractable.
This phase flips no lint to `deny`; it only removes debt so later phases are
smaller. Re-measured before running: **3,649 machine-applicable sites across
41 crates**, of 10,589 total. The sweep cleared 3,644 of them and took the
workspace from 10,589 sites to 6,381.

The six dual-homed FFI crates are out of scope — they carry no
`[lints] workspace = true` and belong to L7.

### `suboptimal_flops` is excluded

Its 87 fixes fold expressions into `mul_add`, which changes the result in the
last ulp and, in the image-analysis code, hides the shape of the maths:
`(1.0 - (smin/smax).powi(2)).sqrt()` becomes
`(smin/smax).mul_add(-(smin/smax), 1.0).sqrt()`, and the Gaussian model in
`fwhm.rs` goes the same way. That is per-site judgment in the code that feeds
autofocus, so it is deferred to L6b — recorded as a decision, like `exit` in L4.

`imprecise_flops` stays in: it yields `E.powf(x)` → `x.exp()` and
`(dx*dx + dy*dy).sqrt()` → `dx.hypot(dy)`, both strict improvements.

### Three things `cargo fix` does that a sweep has to plan for

1. **A single non-compiling suggestion reverts the whole crate, silently.**
   `cargo fix` applies, re-checks, and rolls back everything on error, exiting
   0. `pa-falcon-rotator` lost all 84 fixes because `missing_const_for_fn`
   made `mock::bit` `const` while `cast_lossless` rewrote its body to
   `u8::from(b)` — `From` is not const-stable, so the pair does not compile.
   `rp-catalog` lost all 21 because `string_lit_as_bytes` yields
   `&[u8; 8290]`, which does not implement `Read`. **Re-measure after every
   sweep**; a residual count is the only signal that this happened.
2. **A fix can create a fresh on-by-default warning.** `single_match_else`
   rewrote two wait-then-force `match`es into `if let Ok(_) = .. {} else`, an
   empty then-branch that `redundant_pattern_matching` rejects and refuses to
   auto-fix (it changes drop order). `-D warnings` then fails the tree.
   `.is_err()` is the fix, by hand.
3. **One pass is not enough.** Some suggestions only appear once an earlier
   one lands. Two passes with the target set, then one with the default set to
   absorb (2), reached a fixed point everywhere.

The sweep runs on Linux only, so `#[cfg(windows)]` blocks keep their debt.

## L3 — deny `panic`

Re-measured after L1, `panic` turned out to be the cheapest rung on the ladder:
442 sites, but only **20 outside `tests/`**, and 19 of those are `bdd-infra` —
test infrastructure shaped as a library, so the knobs never see it. Exactly one
was production.

| Where | Sites | Treatment |
|---|---:|---|
| `tests/` in 24 crates | 422 | `clippy::panic` appended to the crate-root `#![allow(...)]` each already carried |
| `crates/bdd-infra/src/` | 19 | same, on the existing crate-root allow |
| `services/doctor/src/catalog.rs` | 1 | fixed |

Two scope facts drove the mechanical part:

- **Every file directly under `tests/` is its own crate root.** Covering
  `tests/bdd.rs` alone missed `test_lib.rs`, `test_integration.rs`,
  `translations.rs`, `runner_integration.rs`, `supervision_integration.rs` and
  `test_mock_server.rs` — six more targets, each needing its own attribute.
- **The knobs see the `#[test]` fn, not what it calls.** A panic inside a
  closure or a `tests/common/mod.rs` helper still fires, which is why
  `rusty-photon-shared-transport`'s failure-injection helpers needed one.

The production fix: `doctor`'s `CATALOG` parsed each embedded
`pkg/doctor.toml` with `unwrap_or_else(|e| panic!(...))`. It now skips an
unparseable entry, and `test_catalog_covers_every_embedded_service` asserts
the catalog covers every `RAW` entry — so a malformed file fails CI loudly
instead of aborting every doctor run in the field.

## L4 — `string_slice` denied, `exit` deliberately not

**`string_slice` (41 sites, 38 in `src/`)** — done. Mostly `get(..)` in place
of a bare range, with three that read better rewritten outright:
`rp-fits`'s exponent split became `split_once('E')`, session-runner's duration
surface check became `strip_prefix`/`trim_start_matches` chaining with no
slicing or length arithmetic at all, and `dsd-fp2`'s mock command dispatch
became a `strip_prefix` chain instead of `starts_with` guards followed by
`[4..]`. Three test modules keep a scoped `#[allow]` (no knob exists), all
slicing a literal UUID to its 8-char disk key.

**`exit` (40 sites)** — **not** denied, and recorded as a decision rather than
a deferral. Every site is `services/*/src/doctor.rs`, where `pub fn run(...) -> !`
exits on doctor's documented 0/1/2 contract (see [doctor](../services/doctor.md)).
Denying it buys a pile of `#[allow]`s or a refactor of a deliberate signature.

## L5 — the expensive three

Real per-site judgment: `checked_*` / `TryFrom` / `get()`.

Re-measured after L2, over the 41 crates that inherit the workspace lints:

| Lint | Prod | Total | Where it concentrates |
|---|---:|---:|---|
| `as_conversions` | 508 | 604 | camera FFI boundaries (`qhy`/`svbony`/`zwo`/`sky-survey` `camera.rs`), `rp/src/mcp/internals.rs` |
| `arithmetic_side_effects` | 472 | 546 | `rp-catalog`, `rp/src/imaging/analysis/stars.rs`, `rp-fits/src/writer.rs`, `rp-ephemeris` |
| `indexing_slicing` | 250 | 525 | `ppba-driver/src/protocol.rs`, `skywatcher-motor-protocol`, `bdd-infra/src/rp_harness/config.rs` |

**L2 did not shrink these three, and `as_conversions` grew.** That is expected,
not a regression: `cast_lossless` converts exactly the casts that *are*
lossless, so what it leaves behind is the genuinely lossy set — and every
`f64::from(x)` it wrote in place of `x as f64` removes a site that was never
L5's problem. What remains is the work L5 was always going to be.

Crate by crate, `rp` last — it carries the largest share on its own.

### `as_conversions` is not one problem

Join each `as_conversions` span with whichever of clippy's five diagnostic cast
lints fired at the same span. That is the compiler's own verdict on what the
cast can lose, and it classifies far better than reading source text. Over the
485 sites left after #854:

| n | what also fired | what fixing it needs |
|---:|---|---|
| 162 | nothing | total by type — only a spelling to pick |
| 98 | truncation, float source | a rounding / clamp policy |
| 77 | truncation | genuine `try_from` candidates |
| 67 | sign loss / possible wrap | same-width sign flip |
| 66 | precision loss | int → float; no `From` impl exists |
| 8 | truncation, bounded on the same line | `#[expect]` with a reason |
| 7 | — | FFI / opaque platform types |

The 162 + 67 that clippy proves total then split by *shape*, and a shape has one
answer rather than 229:

| n | shape | answer |
|---:|---|---|
| 101 | `x as usize` | no `From<u32> for usize` — a boundary question, L5b below |
| 62 | `i32 as usize` from a cucumber `{int}` parameter | change the step signature; `{int}` parses via `FromStr`, so `usize` works and 37 steps already do it |
| 32 | trait-object coercion | not a value conversion — L5a below |
| 12 | `x as u64` | as with `usize` |
| 16 | masked / const narrowing, `char` ↔ int, byte-string | per-site |

Two shapes look mechanical and are not. `hfr.rs`'s `r as usize` sits in a loop
whose body needs signed arithmetic (`(r - cx) * (r - cx)`) and whose bounds feed
`f64::from` — retyping the loop breaks both. And a `const` cannot use `From` at
all (`u32::from` is not const-stable), so `const RAW16_MAX_ADU: u32 = u16::MAX
as u32` has no `From` spelling available.

### L5a — trait-object coercions

32 sites cast to a trait object. These are unsizing coercions, not value
conversions: nothing can be lost, and the fix is to give the compiler a
coercion site instead of an `as`. Three shapes, and only one is subtle.

**`Arc::clone(&x) as Arc<dyn T>` cannot simply lose its cast.** `Arc::clone`
takes its type parameter from the *expected* type, so an `Arc<dyn T>`
expectation makes it demand `&Arc<dyn T>` and the unsizing never gets a chance:

```
808 |     Arc::clone(&manager),
    |     ---------- ^^^^^^^^ expected `&Arc<dyn ServiceManager>`, found `&Arc<RecordingManager>`
```

Two spellings work. Where the concrete `Arc` is not needed afterwards, coerce
the binding once and every later `Arc::clone` reads normally:

```rust
fn spawn(manager: Arc<ScriptedDiscovery>) -> Self {
    let manager: Arc<dyn ServiceManager> = manager;
```

Where it *is* needed — which is 17 of these 19 sites, all test fixtures that
hand the trait object to the code under test and keep the mock for assertions —
pin the type parameter with a turbofish so the coercion lands on the result:

```rust
FalconManager::new(Arc::<MockFalconTransportFactory>::clone(&factory))
```

`x.clone()` also compiles (method resolution takes the type from the receiver),
but it drops the explicit `Arc::clone` spelling the workspace uses to keep
refcount bumps visible.

`Arc::new(Concrete) as Arc<dyn T>` just loses its cast — the argument alone
fixes the type parameter, so the coercion applies to the result.

The last 7 of the 32 are `s as &dyn ProgressEmitter` in `rp`'s MCP tools, all
one shape: an `Option<ProgressSink>` becoming the `Option<&dyn ProgressEmitter>`
the helpers in `internals` take. Two things block the obvious fix at once —
unsizing does not reach inside `Option`, and a closure with an inferred return
type is not a coercion site — so neither the argument's type nor the two
adapters' declared `-> Option<&dyn ProgressEmitter>` can drive the coercion.
Collapsing the trait object is not an alternative either: `ProgressEmitter` has
a second impl that the unit tests count progress notifications through.

An inherent method on `ProgressSink` gives the coercion one named home and
leaves every call site shorter than the cast did:

```rust
impl ProgressSink {
    pub(crate) fn as_emitter(&self) -> &dyn ProgressEmitter {
        self
    }
}

let emitter = sink.as_ref().map(ProgressSink::as_emitter);
```

An explicit closure return type (`|s| -> &dyn ProgressEmitter { s }`) and a
turbofish on `map` both compile too, but each repeats the trait object at all
seven sites.

### L5b — `x as usize` is a boundary question

`usize` has no `From<u32>` (it may be 16 bits), so these have no total named
spelling and the lint cannot be satisfied by picking a better one. Two answers
were measured and rejected before the third:

- **`usize::try_from` per site.** `usize` is at least 32 bits on every target
  the workspace builds for, so the error arm cannot fire on anything we ship —
  76 unreachable arms, uncoverable by construction, against a repo that does
  not allow production coverage exclusions. Red `codecov/patch` by design.
- **`#[expect]` per site.** Honest and cheap, but it annotates the confusion
  instead of removing it, permanently.

What the sites actually say is that a value is being used as a length while
typed as something else. So the rule is about *where* the conversion belongs,
not how to spell it:

> **`usize` must never appear in a serialized format**, because it is
> platform-dependent. Anything bound for disk or a wire carries a fixed width.
> Anything that indexes a buffer is a `usize`. Convert once, where those meet.

That resolves each site without judgment. `rp-fits`' reader hands back a buffer
and its shape, so it yields `usize` — which also deletes a step, since it had
been parsing `NAXIS` from `i64` into a `u32` that every caller immediately
widened again. Alpaca `NumX`/`StartX`, the `ImageBytes` header, the sidecar
JSON's `width`, and a PNG's dimensions are all fixed-width for the same reason;
`crop_subframe`, `Array2::from_shape_vec`, and a preview's subsampling are all
`usize`.

Boundary conversions that survive get folded into an error the function already
returns — an ASCOM subframe too large for a `usize` cannot fit the source
buffer either, so it lands in the existing bounds check rather than earning a
variant of its own.

The writer looked like the exception, because its dimensions are *both* a
`NAXIS` header card and the length of the buffer being validated against them.
Taking them as `u32` there was the first answer; measuring it changed the
verdict. `u32` parameters left the capture path narrowing `image_array.dim()`
from `usize` to `u32` only to widen it straight back for the `Array2` shape,
and left `FitsError::DimensionMismatch` carrying `got: usize` and
`expected: usize` beside `width: u32`. Taking `usize` collapses both, and the
`i64::try_from` it adds on the `NAXIS` side is offset by the `checked_mul`
overflow arm becoming *reachable* — with `u32` parameters that arm is dead code
on every 64-bit target, and with `usize` it is a two-line test. The narrowing
that remains sits at the JSON sidecar, which is genuinely a serialized field.

So the boundary belongs at the last consumer that needs a fixed width, not at
the first function that touches one. Two consequences worth carrying forward:

- **A conversion is not free wherever it lands.** Pushed into
  `gen_autofocus_fixtures`, one landed on `-D clippy::expect_used` and only
  resolved because `main` returns `Box<dyn Error>`, making `?` available.
- **Retyping a boundary can relocate rather than remove it.** The
  `sky-survey-camera` BDD harness holds dimensions that are simultaneously a
  `vec![0u16; n]` length and `f64` WCS header cards, and `f64::from(usize)`
  does not exist. It converts explicitly instead.

### L5b — SDK frame buffers

The camera backends size a download buffer from an ROI, which is the same
boundary in a different costume: the ROI is device state and arrives
fixed-width, the buffer length is a `usize`. Two things made this slice
cheaper than the FITS one.

Each vendor crate already had a function computing that length —
`zwo_rs::RoiFormat::buffer_len`, `svbony_rs::Camera::frame_buffer_len` — and
each vendor crate's download call already compares the caller's buffer against
it before handing the pointer to the SDK. So the conversion has exactly one
home per crate, and the drivers stopped recomputing the length from the ASCOM
request. `zwo-camera` had been restating the bytes-per-pixel as a literal `2`
while `zwo_rs` carried a real `bytes_per_pixel()` covering 1, 2 and 3.

Saturation was the first answer for a length that cannot fail into a `Result`:
`usize::MAX` makes every buffer too small, so the caller's existing
`BufferTooSmall` arm reports it and no arm has to be invented. Review caught
that this is only half true, and the half it gets wrong is the dangerous one.

> **Saturate what you compare, fail what you allocate.**

A saturated length is exact for a caller comparing a buffer against it and
catastrophic for one that *allocates* it — `vec![0u8; usize::MAX]` aborts the
process instead of reporting anything. The same slice that added the saturating
`buffer_len` also added the first callers that allocate from it, in three
places. Both vendor crates' length functions are therefore fallible
(`RoiFormat::buffer_len -> Option<usize>`,
`Camera::frame_buffer_len -> Result<usize>`), and a second saturating entry
point was deliberately *not* kept beside them: leaving one available leaves the
trap set for the next allocating caller. Saturation survives only where the
result is compared and never allocated — `to_image_array`, where a saturated
`needed` lands in the "buffer too small for frame" answer the function already
returns.

Unlike the `NAXIS` arm above, these arms are reachable and tested: `RoiFormat`
and `CaptureRequest` are plain structs a test builds with `u32::MAX`.

Reading the drivers this closely surfaced two defects that have nothing to do
with the lint, both filed rather than fixed here: #881 (`zwo-camera` sets
`Raw16` unconditionally and never reads the SDK's `SupportedVideoFormat`, so
an ASI120/ASI130-class camera cannot expose at all) and #882
(`svbony-camera` read the format list into `CameraProperty` and then ignored
it). #882 was closed by #884 while this slice was in review, which changed the
answer here: the negotiated format now rides in `CaptureRequest::image_type`,
so the buffer length follows the format actually selected instead of a
restated constant. That is a better shape than reading it back from the SDK,
and this slice adopted it.

`qhy-camera` was already the one getting this right, via
`set_if_available(TransferBit, 16.0)` and `GetQHYCCDMemLength`. #881 was then
closed by #887, which negotiates `SupportedVideoFormat` and carries the choice
in `CaptureRequest::image_type` — the shape #884 established for svbony — so no
driver assumes its download format any more. #887 consumed the fallible
`buffer_len` unchanged, which is the check that the rule above survives contact
with a caller that did not write it.

### L5b — QHY index casts

14 production sites across `qhyccd-rs` and `qhy-camera`, and unlike the frame
buffers none of them is a length. Three shapes, each with one answer:

- **An SDK `u32` indexing a `Vec`** (readout-mode tables, 5 sites). Every one
  already had an out-of-range answer — `ok_or(QHYError::Sdk)`, or a fall back to
  the full chip — so the conversion folds into it:
  `usize::try_from(i).ok().and_then(|i| modes.get(i))`. No arm is invented and
  none becomes unreachable, because a failed conversion and an out-of-range
  index are the same event.
- **`Vec::with_capacity` from a device-reported count** (3 sites). Capacity is a
  hint, so `unwrap_or(0)` is total and honest: a count too large to address just
  means no preallocation, and each loop is bounded by that same count. This is
  *not* the allocating case the rule above covers — nothing is sized by it.
- **ASCOM's `usize` surface** (6 sites). `FilterWheel` is `usize` throughout
  (`Position`, `set_position`, and the `Names` / `FocusOffsets` lengths that must
  match it), so the wheel's cached slot state was retyped to `usize` and the
  conversion moved to the SDK seam it actually belongs at.

That retype was worth more than the lint. `set_position` had been narrowing the
client's slot to the SDK's `u32` *before* range-checking it, so every value past
2^32 wrapped onto a real slot — an Alpaca client sending `Position=4294967299`
moved the wheel to slot 3 and got no error. Checking in the type ASCOM sends
makes the wrap impossible rather than merely detected, and the arm is reachable,
so it has a test. `GetQHYCCDMemLength` is the one genuinely allocating site here
and takes the fallible form per the rule above.

### L5c — the simulator's pixel loops

Re-measuring after the QHY index casts put one file at the top of all three
lints at once: `crates/qhyccd-rs/src/simulation.rs` held 218 of the 1,120
production sites left — 136 `as_conversions`, 65 `arithmetic_side_effects`, 17
`indexing_slicing`, where the next-biggest `as_conversions` file has 26. It is
also the only file where all three land on the *same lines*, which is the tell
for what they were pointing at: ten generator functions each walking a frame by
hand.

```rust
let idx = ((y * width + x) * channels) as usize;
for c in 0..channels as usize {
    data[idx + c] = value;
}
```

The cast, the index arithmetic and the indexing are one decision — to compute an
offset — and none of the three lints can be answered on its own. Iterating the
buffer instead deletes all three, because there is no offset to compute:

```rust
for row in data.chunks_exact_mut(frame.row_bytes) {
    for (x, pixel) in (0u32..).zip(row.chunks_exact_mut(frame.pixel_bytes)) {
        pixel.fill(value);
    }
}
```

Three things this settles that a per-site conversion would not have:

- **Coordinates stay `u32`, lengths become `usize`.** The same width is a
  coordinate the gradient ramp divides by and a stride that walks the buffer, so
  `Frame` names both and converts once. Zipping `(0u32..)` onto the chunk
  iterator is what keeps the coordinate in the type that *has* `f64::From` —
  `enumerate()` would have handed back a `usize` and relocated the cast rather
  than removing it, the trap the FITS writer hit above.
- **The bounds check moves into the conversion.** `Frame::pixel_mut` returns
  `None` for a pixel outside the frame, which replaced a four-way `x < 0 || x >=
  width || y < 0 || y >= height` guard in both star drawers: a negative
  coordinate is the `u32::try_from` error arm and an overhanging one is the
  `None`. Every arm is reachable and tested; none was invented.
- **The geometry is refused whole, or not at all.** `Frame::new` is the crate's
  one place that multiplies width by height by channels, and it does so with
  `checked_mul` in `usize`, so an unaddressable frame yields `None` instead of a
  wrapped length. `vec![0u8; frame.len]` is an allocating caller, so this is the
  fallible form the rule above requires — and the old `(width * height *
  bytes_per_pixel * channels) as usize` genuinely panicked (`attempt to multiply
  with overflow`) in any debug build rather than merely wrapping. It was not
  reachable in the field, because the only caller — `qhy-camera` — runs
  `check_geometry` against the chip first; `simulation`'s own `set_roi` stores
  whatever it is handed, so the contract was the caller's rather than the
  library's. Now it is the library's.

That last point has a consequence worth stating separately, because it is the
part that is easy to get wrong: **an entry-point guard is only as good as what
it promises downstream.** Two multiplies further in still took the old shape,
and one of them was a live panic — a starfield asks `random_range` for a centre
one pixel inside each edge, which is an empty range on a frame under three
pixels across. Reaching it needs a frame that is *both* narrow and large, since
the star count is 0.1% of the pixel count and rounds to zero below a thousand
pixels; 2 × 600 does it. The first test written for that guard passed without
the guard for exactly that reason, and only checking that it failed without the
fix caught it.

218 sites → 135, and `indexing_slicing` leaves the file entirely. What remains
is bucket F's value math (`(base as i16 + noise).clamp(0, 255) as u8`) and the
star geometry's signed detour, both of which want the rounding policy rather
than this shape.

### L5c — the simulator's value math

The second half of the same file: 51 lines carrying a cast clippy can
classify. Four groups, and only one of them needed a decision.

**Clamping to a type's own range is a saturating conversion, spelled out.**

```rust
let value = (base as i16 + gradient as i16 + noise).clamp(0, 255) as u8;
```

Two widening casts, a clamp whose bounds *are* `u8`'s, and a narrowing cast —
three lints on one line, nine lines like it. Naming what it does collapses all
of them, and the arms are live traffic rather than defensive padding, so this is
not the dead-arm trap `usize::try_from` was:

```rust
fn sample_u8(v: i32) -> u8 {
    u8::try_from(v).unwrap_or(if v < 0 { u8::MIN } else { u8::MAX })
}
```

**Float → int is already total; what was missing was saying so.** `as` truncates
toward zero and saturates at the destination's bounds, with NaN landing on zero
— and no `TryFrom<f64>` exists to spell it another way. So the 16 sites became
one `quantize` module of six one-line functions carrying a single `#[expect]`,
which is the file's only exemption. That is the template for bucket F's 98
sites: **one annotation at a named boundary, not 98 silent casts.** Truncation
was kept rather than switched to rounding, so no pixel value moves.

**Two total spellings worth carrying forward**, both found by asking whether the
conversion was needed at all rather than how to spell it:

- `Duration` has no `u64` accessor for whole microseconds — `as_micros` is
  `u128` — but `as_secs` is `u64` and `subsec_micros` is `u32`, so composing the
  two needs no conversion and saturates instead of wrapping.
- `(self.base_level >> 8) as u8` is the high byte of a `u16`, which
  `let [base, _] = self.base_level.to_be_bytes();` yields with no cast, no index,
  and no arm.

**And one more defect, with a blast radius worth stating precisely.**
`get_remaining_exposure_us` narrowed its `u64` remainder to the SDK's `u32`
microseconds, which run out at ~71 minutes, and nothing clamps the `Exposure`
parameter to its own control range on the way in. A two-hour exposure wrapped
round to 2,905,032,704 µs — about 48 minutes — so the value fell, jumped, and
fell again. Saturating is the honest answer: the SDK's word cannot express more.

What it does **not** do is shorten an exposure through `qhy-camera`, and the
distinction matters because the first write-up of this claimed otherwise.
`wait_for_exposure` times the exposure host-side from an `AtomicU64` of
microseconds and only *then* polls the camera for confirmation, bounded by a 5 s
`EXPOSURE_CONFIRM_TIMEOUT` it falls through regardless. A bogus remainder
therefore costs at most five seconds of extra polling. The real-hardware path is
untouched for a second reason: `GetQHYCCDExposureRemaining` is a `uint32_t` in
the vendor API, so no narrowing of ours sits on it — the ~71-minute ceiling is
the SDK's own shape. The defect is this crate's API contract, not the driver's
behaviour, and the guard belongs here because the next consumer may well poll it
as authority.

135 sites → 60.

#### What chunked iteration costs, and what actually pays for it

Worth reading before applying this shape to the remaining `indexing_slicing`
sites, several of which are in genuinely hot loops (`imaging/analysis/stars.rs`,
`ppba-driver/src/protocol.rs`). Full-frame 3072×2048, mean of 20, old → new:

| pattern | release | debug |
|---|---|---|
| gradient 16-bit *(the only pattern production generates)* | 1.66 → 1.74 ms | 6.2 → 18.4 ms |
| gradient 8-bit | 17.8 → 17.4 ms | 180 → 276 ms |
| starfield 16-bit | 23.6 → **12.1** ms | 181 → 555 ms |
| starfield 8-bit | 23.1 → **11.9** ms | 109 → 110 ms |
| flat 16-bit | 22.5 → **11.0** ms | 181 → 527 ms |
| flat 8-bit | 13.5 → **11.1** ms | 185 → **97** ms |
| test pattern 16-bit | 35.3 → **19.1** ms | 240 → 666 ms |
| flat, 3-channel 16-bit | 40.7 → **25.2** ms | 292 → 932 ms |

Release is uniformly faster or level. Getting there took two corrections, and
both are the transferable part:

- **Chunking by a runtime `1` is not free.** A mono 8-bit frame's pixel *is* its
  sample, and `chunks_exact_mut(1)` cost ~3.4 ns/pixel more than an indexed
  write — enough to make the cheapest loop in the file 2.6× slower than the code
  it replaced. Walking samples directly (`data.iter_mut()`) is both the faster
  and the more honest spelling for that case, so `generate_flat_8bit` branches
  on it. Passing the per-pixel value through a closure instead, to share one
  helper between the mono and colour arms, made it *worse* again (16-bit went
  10.4 → 23.8 ms): the noise state ends up behind a `&mut` capture and stops
  living in a register.
- **The loop shape was not the real cost — a divide was.** `PixelNoise` drew
  each sample with `next_u32() % span`, one hardware division per pixel, and the
  indexed loop happened to schedule around it better than any iterator spelling
  did (12.8 ms against ~25 ms, and every iterator form measured the same ~25 —
  `iter_mut`, `for_each`, `fill_with`, nested rows). Replacing the modulo with a
  high-word multiply by the span — same range, same negligible bias, one
  multiply — dropped the *iterator* form to 11.1 ms, below the indexed original.
  The indexed form gained nothing from it, which is the tell: it was never
  paying full price for the divide.

So the shape is worth having, but "remove the indexing" and "keep it fast" are
two separate pieces of work, and a cheap-looking loop body can be the thing that
decides. Debug is the other half of the trade: nothing is inlined there, so the
chunked 16-bit paths run ~3× slower and `#[inline(always)]` on the crate's own
helpers does not recover it — the cost is std's. That is accepted here because
production only ever generates the gradient and the debug cost lands on at most
14 BDD captures (~0.2 s against a 2.8 s suite), but CI builds fastbuild, so a
hot loop converted this way pays it on every run. Measure the site first.

### L5c — the star geometry's signed detour

The file's last structural shape, and 50 of the 60 sites it had left. Every one
of them dropped into `i32` to subtract two coordinates, and then threw the sign
away:

```rust
let x = cx as i32 + dx as i32 - size as i32;
let (Ok(x), Ok(y)) = (u32::try_from(x), u32::try_from(y)) else { continue };
let dist = (((dx as i32 - size as i32).pow(2) + (dy as i32 - size as i32).pow(2)) as f64).sqrt();
```

`dx - size` is computed twice — once to place a pixel, once to measure it — and
what each use wants is not a signed number. The distance wants a **magnitude**;
the coordinate wants a **direction**. `abs_diff` yields the first without
leaving `u32`, and a comparison picks the arm for the second:

```rust
let (ox, oy) = (dx.abs_diff(radius), dy.abs_diff(radius));
let (Some(x), Some(y)) = (
    if dx >= radius { cx.checked_add(ox) } else { cx.checked_sub(ox) },
    if dy >= radius { cy.checked_add(oy) } else { cy.checked_sub(oy) },
) else { continue };
let (fx, fy) = (f64::from(ox), f64::from(oy));
let dist = (fx * fx + fy * fy).sqrt();
```

Three things worth carrying to the other `as_conversions` clusters:

- **Clippy exempts float arithmetic**, because a float operation cannot panic or
  wrap. So moving a computation *into* `f64` through a total magnitude removes
  the whole cluster at once — no `checked_*`, no `#[expect]`, nothing to justify
  per site. It only works when the result was going to be a float anyway, which
  a distance is.
- **`u8` is a bound the compiler can see.** `draw_star_*` took `size: u32` and
  wrote `0..=size * 2`, which lints. Retyping the parameter to `u8` makes the
  doubling provably total — 2 × 255 cannot leave `u32` — with no invented cap
  and no arm that never fires. The callers pass `random_range(1..=3)`, so the
  type merely records what was already true.
- **A guard clippy cannot see still needs the saturating spelling.**
  `random_range(1..frame.width - 1)` sits under an early `frame.width < 3`
  return; `saturating_sub(1)` says so without adding a second guard.

**The old form was wrong at the seam, and the rewrite fixes it silently.** An
exhaustive sweep of both forms over 168,611 (centre, size, offset) combinations
— every centre from 0 to 40, plus 1000, 3071 and the `i32`/`u32` boundaries, at
every size the generators draw — agrees **bit-for-bit on coordinate and
distance for every centre below `i32::MAX`**, and diverges only above it, where
`cx as i32` goes negative and the old code dropped a pixel that was inside the
frame. With debug assertions the old form additionally *panicked* on 10,970 of
those cases. Neither is reachable through `qhy-camera`, but both are gone.

60 sites → 10, and the file's only remaining shapes are the four in the tail
(a filter-wheel decrement, a `BayerPattern` discriminant, a row seed, and
`PixelNoise::next_signed`).

#### What the total spelling cost, measured properly

The cross-binary comparison used for the pixel loops could not resolve this
change: over seven passes, `generate_gradient_8bit` moved −6.5% and the colour
flat path +8.6%, and **neither function was touched**. At that noise floor a
real ±3% is invisible. Building both forms into one binary and interleaving them
— same process, same allocator, same cache state — resolves it to ±0.2%:

| ring-distance form | full 3072×2048 frame |
|---|---|
| old, `i32` square then one convert | 12.3 ms |
| **new, magnitude then float square** | **12.7 ms (+3%)** |
| rejected: `f64::from(ox.saturating_mul(ox))` | 18.2 ms (+50%) |

The integer variant looks closest to the original and is by far the worst: the
`saturating_mul` cmov defeats vectorization of the inner loop. The float form's
3% buys ten sites and the seam fix, on the one pattern production never
generates — `ImagePattern::TestPattern` is a test and BDD fixture. Two lessons
generalize: **a same-process A/B is the only comparison that survives layout
noise**, and **a saturating spelling in a vectorizable loop can cost far more
than the widening it avoids.**

### L5c — the noise source, and three benchmarks that disagreed

`PixelNoise::next_signed` was the file's hottest body — one call per pixel, so
6.3 million per full frame — and carried five of the ten sites left after the
star geometry. It recomputed a value that could not change:

```rust
fn next_signed(&mut self, range: i32) -> i32 {
    if range <= 0 { return 0; }
    let span = u64::from(range.unsigned_abs()) * 2 + 1;   // per pixel
    let scaled = (u64::from(self.next_u32()) * span) >> 32;
    (scaled as i64 - i64::from(range)) as i32
}
```

`range` is fixed for a whole frame, so `span` belongs on the struct. Moving it
there deletes one site outright, lets the constructor spell the remaining
arithmetic saturating for free (once per frame), and drops the per-pixel
`range <= 0` branch — a zero range now yields a span of one, which scales every
draw to zero anyway. What is left is two lines under **one** `#[expect]`
carrying the bound proof: `range <= i32::MAX` gives `span <= 2^32 - 1`, so the
product is at most `(2^32 - 1)^2 = 2^64 - 2^33 + 1`, inside `u64` with `2^33 - 1`
to spare.

**Neither cast has a total spelling.** `scaled as i64` and the final `as i32`
are both provably in range, and both `try_from` forms would be arms that can
never be taken — the dead-arm trap this plan has hit before. An `#[expect]` is
the honest answer where a `checked_*` would be theatre.

10 sites → 5.

#### Three benchmarks, three answers, one that was measuring the right thing

This is the part worth carrying forward, because two of the three were
convincing and wrong:

| how it was measured | verdict on the hoist |
|---|---|
| microbenchmark accumulating draws into an `i64` | **−12 to −14%** |
| whole generators, separate binaries, min of 5 passes | inconclusive (±8% noise) |
| **the real loop body, both forms in one process** | **0%** |

The microbenchmark's `acc += next_signed()` creates a dependency chain that
exposes the span computation's latency. The real loop stores to an independent
buffer, so out-of-order execution hides that work entirely behind the xorshift's
own state chain — the recomputation was *already free*, and the 14% was an
artifact of the harness. The cross-binary run, meanwhile, could not see anything
at all: `generate_gradient_8bit` moved +8% between builds that do not change it.

**So the hoist is not a speedup, and this plan should not claim one.** It is
worth keeping because a frame-invariant value computed per pixel is wrong on its
own terms, and because it shrinks the exemption. What *is* a measured result is
the alternative:

| per-pixel noise draw, 6.29 M samples | |
|---|---|
| recompute the span (original) | 10.5 ms |
| hoisted, arithmetic under `#[expect]` | 10.5 ms |
| **hoisted, arithmetic saturating** | **15.3 ms (+45%)** |

That is the second time in this rung a saturating spelling cost far more than
the widening it avoided — the star geometry's integer variant measured +50%.
Two sites, two independent measurements, one rule: **inside a per-pixel loop,
reach for `#[expect]` with a bound proof, not for `saturating_*`.** Outside one,
saturating is free and should stay the default.

#### Frames are non-deterministic, so compare the stream

An attempt to verify the hoist by hashing generated frames failed before it
started: `generate_8bit` draws `frame_seed` from `rand::rng()` on every call, by
design, so two runs of the *same binary* disagree. Only the zero-noise cases
matched, which looked exactly like a real regression. The check that works is
the noise stream itself — `next_signed` for a fixed seed — which is identical
before and after across ranges 0, 10, 100, 65535 and `i32::MAX`.

### L5c — the tail, and the file at zero

Five sites, four decisions, and one of them paid for itself twice over.

**Two guarded decrements** — `f64::from(slots - 1)` under an `if slots > 0`, and
`settle_polls -= 1` under an `if settle_polls > 0`. Neither runs in a loop, so
`saturating_sub(1)` is free and the guard above it is what makes the arm
unreachable. This is now the third distinct place in the rung where the answer
was "the guard is real, clippy just cannot see it".

**A discriminant enum with no `From`.** The simulator read `BayerPattern`'s SDK
code with a double cast, written twice:

```rust
(bayer_pattern as u32 as f64, bayer_pattern as u32 as f64, 0.0)
```

and `types.rs` open-coded the same conversion four more times inside its own
`TryFrom<u32>`, as guard clauses comparing against `Variant as u32`. Writing the
conversion once turns all of it into ordinary code:

```rust
impl From<BayerPattern> for u32 {
    fn from(pattern: BayerPattern) -> Self {
        match pattern { GBRG => 1, GRBG => 2, BGGR => 3, RGGB => 4 }
    }
}
```

`TryFrom` becomes a plain `match value { 1 => Ok(GBRG), ... }`, the simulator
becomes `f64::from(u32::from(bayer_pattern))`, and `camera.rs`'s
`.map(|m| m as u32)` becomes `.map(u32::from)`. **Seven sites across three files
for one impl** — the best ratio in the rung, and the pattern to look for
wherever a `repr`-style enum is read back with a cast. The discriminants stay on
the enum because the doc comment promises the SDK's numbering, and a test now
pins `u32::from` to those four values so the two cannot drift.

**One `#[expect]`, for a conversion that has no total spelling.** `row_seed`
takes a `usize` row index and folds it into a `u32` seed. `usize → u32` has no
`From`, and every fallible spelling is worse than the cast: the arm cannot be
taken (the row index is bounded by `Frame::height`, a `u32`), and a
`unwrap_or(0)` fallback would alias row 0's seed. A seed needs only to differ
between rows.

5 sites → 0. **`simulation.rs` is at zero production sites, from 218**, and
`types.rs` with it. What remains in the crate is `camera.rs` (21) and the test
scope.

| L5c pass | sites |
|---|---:|
| starting point | 218 |
| pixel loops (chunked iteration) | 135 |
| value math (`quantize`, saturating samples) | 60 |
| star geometry (`abs_diff`, float distance) | 10 |
| noise source (span hoisted, one `#[expect]`) | 5 |
| **tail (`From` impl, guarded decrements)** | **0** |

Three exemptions in the whole file, each at a named boundary with a bound
proof: `quantize` for float→int, `next_signed` for the per-pixel draw, and
`row_seed`. That ratio — 218 sites to three annotations — is what the rung was
testing, and it holds.

### L5c — `camera.rs`, where a cast was hiding a bug

20 sites, one of them in test scope. Four clusters cleared without an exemption,
and one of them was not a lint problem at all.

**The filter-wheel codec was a hand-written `char::to_digit`.** Slots travel over
`CONTROL_CFWPORT` as a single hex digit — `'0'`..`'F'`, sixteen positions — and
both halves of the codec spelled that out by hand:

```rust
fn cfw_slot_to_ascii(slot: u32) -> u32 {
    if slot < 10 { slot + b'0' as u32 } else { (slot - 10) + b'A' as u32 }
}

fn cfw_ascii_to_slot(ascii: u32) -> u32 {
    match ascii {
        d @ 0x30..=0x39 => d - 0x30,
        d @ 0x41..=0x46 => d - 0x41 + 10,
        d @ 0x61..=0x66 => d - 0x61 + 10,
        other => other.saturating_sub(0x30),
    }
}
```

Seven sites between them. The decode is exactly `char::to_digit(16)`, which
accepts the same codes in either case and rejects everything else including
non-ASCII, so it collapses to one call with the legacy fallback intact:

```rust
char::from_u32(ascii).and_then(|c| c.to_digit(16)).unwrap_or_else(|| ascii.saturating_sub(0x30))
```

Verified identical on **all 4,294,967,296 `u32` inputs** (6.5 s in a release
build — cheap enough that there was no reason to sample).

The encode is where it got interesting. **`u32` is eight times wider than the
domain**, and that over-width is what made the arithmetic unprovable. Narrowing
to `u8` would not have helped — `u8` permits 255 and `255 + b'0'` still
overflows, so clippy warns identically. This is *not* the `size: u8` case from
the star drawer, where the `u8` was narrower than the `u32` arithmetic it fed and
so the bound was provable. The bound needed here is 16, and no integer type says
16.

`char::from_digit(slot, 16)` says it, and returns `None` above it:

```rust
fn cfw_slot_to_ascii(slot: u32) -> Option<u32> {
    char::from_digit(slot, 16).map(|d| u32::from(d.to_ascii_uppercase()))
}
```

Which surfaced the bug the total signature had been concealing: `set_fw_position(20)`
encoded to `'K'` and commanded an undefined slot, silently. Nothing shipping
reached it — the Alpaca layer bounds-checks against `get_number_of_filters` first
— but the library's own contract permitted it, and no test pinned the behaviour.
It is now `QHYError::InvalidFilterSlot`, in a function that already returned
`Result`.

**Generalisation:** a conversion forced to be total by pretending an
unrepresentable case cannot happen is worth a second look before it is made
lint-clean. The same shape appeared twice more in this file —
`get_number_of_readout_modes` truncating a `usize` count into the `u32` its
`Result` could have rejected, and `ControlType::Other(i32)` carrying a value the
SDK takes as `u32`, which `to_raw` then cast back. Both had an honest arm
available and unused.

**The buffer copies wanted `get_mut`, not a check.** Both frame downloads did an
explicit `if buf.len() < data.len() { return Err(BufferTooSmall) }` and then
indexed. Normally a fallible spelling after a real check just adds a dead arm —
but here the check and the slice are the same question, so they become one
statement:

```rust
let Some(destination) = buf.get_mut(..data.len()) else { /* BufferTooSmall */ };
destination.copy_from_slice(&data);
```

In `get_single_frame` this paid for itself: the early check existed only so a
short buffer would error *without consuming* the captured image, 20 lines before
the copy. Copying through the borrow instead of out of a `take` gets the same
guarantee from the ordering, and the whole bounds-checking preamble deletes.

**`quantize` moved up a level.** Two of the float→int sites (`cfw_slot_count`,
`cfw_position`) compile in real-SDK builds too, where `simulation.rs` — and its
private `quantize` module — does not exist. The module's doc comment already
stated a crate-wide policy; it just lived in the file that needed it first. It is
now `crates/qhyccd-rs/src/quantize.rs`, with `to_u32` unconditional and the other
five widths gated behind `simulation` so a real-SDK build has no dead code to
warn about. **One exemption now serves both backends.**

20 sites → 0, with no new exemption. **`qhyccd-rs` production code is at zero**;
what remains in the crate is test scope and `libqhyccd-sys`, which does not
inherit the workspace lints (`-sys` shims are L7's bucket — and its one site,
`const QHYCCD_ERROR_F64: f64 = u32::MAX as f64`, has no alternative anyway:
`f64::from` is not `const`).

### L5d — the gain range, cached at the wrong type

The three ASCOM camera services carry the same six accessors, and between them
26 of the `as_conversions` sites were one mistake repeated: the SDK's gain and
offset range cached in the SDK's own width, then narrowed at every read.

```rust
gain_min_max: Mutex<Option<(f64, f64)>>,   // qhy; (i64, i64) in zwo/svbony

.map(|(min, _)| min as i32)                // ×6 per service
```

All six exist to answer one question — *what integer does ASCOM report?* —
and ASCOM's `Gain`/`GainMin`/`GainMax` are `i32`. Nothing in any of the three
drivers needs the wider type for anything else, so the width was the
transport's word kept past the point where it meant something. **Converting
once, where the range enters the cache, removes every cast**: each read becomes
a copy and `set_gain`'s bounds check becomes native `i32`.

That forces the question the casts were dodging: *what if the SDK's range has
no `i32` spelling?* Clamping advertises a maximum the camera then rejects —
ConformU fails `SetGain` at the advertised bound and a client sees a lie. The
range is now validated at connect and a control that fails is left
**unadvertised**, with a `warn!`. Degrade, don't lie (tenet 2), decided once
rather than six times per request.

**Two defects fell out of the rewrite.** `qhy-camera` compared in the *lossy*
direction — `gain < min as i32`, narrowing the cached bound rather than
widening the caller's value — so a bound above `i32::MAX` saturated and the
comparison silently admitted or rejected the wrong thing. (`zwo`/`svbony`
already widened, and were unaffected.) And `set_gain`'s
`"gain control available but min/max not cached"` arm existed only because
"control available" and "range cached" were two facts that could disagree;
gating on the cache alone subsumes both, which also drops a live
`is_control_available` SDK round-trip from every QHY `Gain` read.

Collapsing two facts into one does carry an obligation, and review caught it:
the survivor must be written on **every** connect, including with
"unavailable". QHY assigned its cache only inside `if is_control_available`, so
a control that went away between sessions would have left the previous
session's bounds standing for the accessors to advertise — harmless while a
live probe was the real gate, a stale answer once the cache became the only
one. (`zwo`/`svbony` already assigned unconditionally.) *When a cache stops
being an optimisation and becomes the authority, every path that used to skip
writing it turns into a bug.*

QHY needs a rounding policy the other two do not, because the QHY SDK carries
every control through one `f64` parameter — the same uniform carrier
`qhyccd-rs`'s `quantize` documents — so a gain bound is an integer travelling
in a float:

```rust
fn ascom_bound(value: f64) -> Option<i32> {
    let rounded = value.round();
    if (f64::from(i32::MIN)..=f64::from(i32::MAX)).contains(&rounded) {
        Some(rounded as i32)
    } else {
        None
    }
}
```

`.round()`, not truncation: float error can land on either side of the integer
the SDK meant, and a `100` arriving as `99.999…` would otherwise advertise a
maximum one below the one the camera accepts. **`contains` is not a style
choice** — written as two comparisons, a NaN passes both tests and reaches
`NaN as i32`, which is `0`. A gain bound of zero conjured from a NaN is exactly
the class of silent-wrong value this rung exists to remove.

`zwo`/`svbony` report caps as `long`, so `TryFrom` does the whole job and
neither needs an exemption:

```rust
fn ascom_range(caps: &ControlCaps) -> Option<(i32, i32)> {
    Some((i32::try_from(caps.min).ok()?, i32::try_from(caps.max).ok()?))
}
```

**26 sites → 0 across three services for one `#[expect]`**, in the one place
no `TryFrom<f64>` exists to spell it otherwise. The remaining 60 sites in the
three files are the shapes that repeat verbatim across all three — `rescale_roi`,
the clamped `pct as u8`, the `bytes[..needed]` frame copies, and `zwo`'s `MaxADU`
shift.

`rescale_roi` was the one where the repetition itself was the finding, and it
went to #913 rather than here: `qhy` carried a `.max(1)` clamp its two siblings
lacked, with a comment describing the bug their absence implied. Copying the
clamp across then exposed the half nobody had — an explicit `NumX = 0` was being
rewritten to 1, inventing exactly the value the clamp existed to prevent, and on
`qhy`, which has no alignment rule to catch it, that cleared every check and
exposed a one-pixel frame. **Reading three copies of a function against each
other is its own audit**, and worth doing before deciding which copy is right.

### L5e — the rest of the camera services, and what reading three copies found

66 production sites across `qhy-camera`, `zwo-camera` and `svbony-camera` to
**zero**, under two exemptions. The rung was run as a deliberate three-way
audit — every shared shape read in all three files before any of them was
changed — because L5d had already shown that is where the defects are.

**It found three, none of them lint problems.**

*A copy that cost a frame.* `zwo` and `svbony` take the download buffer by
value, so their 8-bit path hands it straight to `Array2`; zwo's doc comment says
so explicitly. `qhy` took `&ImageData` and copied — on a 60 MP sensor, the frame
itself, once per 8-bit download. The caller already owned it and did not use it
afterwards.

*A zero that could reach a divide.* `check_geometry` divides the sensor extent
by the bin. Both `zwo-rs` and `svbony-rs` built `supported_bins` with
`u32::try_from(b).unwrap_or(0)`, and the `take_while(b != 0)` sentinel stops at
a literal zero but **not** at a negative — so a negative SDK entry became a `0`
in the list, `0` there reads as a supported bin factor, and `set_bin_x`
validates a client's `BinX` against that list. Both wrappers now drop the entry
rather than inventing one.

*Host byte order in the unpack* (#920): all three read 16-bit pixels with
`u16::from_ne_bytes` where the cameras put them on the wire little-endian. Latent
— every supported target is little-endian — but the fix's test says in its own
comment that it cannot catch a regression, since observing one needs a
big-endian target CI does not have. **A test that cannot fail for the reason you
wrote it should say so.**

**The general form worth carrying:** *reading three copies of a function against
each other is its own audit, and it is not the same as reading any one of them
carefully.* Each file was individually reasonable. What showed up was one copy
quietly paying a cost, or missing a guard, that the other two did not.

#### `NonZero` is where a zero guard belongs

The rung's recurring move, and the same shape as #911's `char::from_digit`: when
a guard exists only to make a later division total, the divisor's **type** can
carry it instead, so the two cannot drift apart.

```rust
// before: two facts, checked in different places
if expected == 0 { return Ok(0); }
let pct = (done as f64 / expected as f64 * 100.0).clamp(0.0, 99.0);

// after: one fact, carried by the type
let Some(expected) = NonZeroU64::new(expected) else { return Ok(0) };
let pct = done.saturating_mul(100) / expected;
```

`Div`/`Rem` by `NonZeroU*` are total, so nothing downstream needs a second
check. It applied four times: `PercentCompleted`'s ratio, `gcd`/`lcm` (`gcd`
now takes and returns `NonZeroU32`, so `a % b` needs no guard and `lcm` can
divide by the result), `aligned_sensor_extent`'s step, and `rescale_roi`'s bin.

#### Integer arithmetic beat the float it replaced, twice

`rescale_roi` scaled through an `f64` ratio and `PercentCompleted` divided two
floats. Both are now exact integer arithmetic — `v * old / new` and
`done * 100 / expected` — which truncates in exactly the same places while
removing the rounding error a divide and a multiply in `f64` accumulate between
them. The float form could land 49.9999 where the ratio is exactly 50.

The same applies to durations: exposure microseconds now come from
`Duration::as_micros` rather than `(as_secs_f64() * 1e6).round()`, and qhy's
exposure *range* goes the other way through `Duration::try_from_secs_f64`, so
the SDK's `f64` never becomes an integer at all. **A float in the middle of an
integer computation is usually a cast waiting to be justified.**

#### The two exemptions

Both are the same site in two services: `set_set_ccd_temperature` narrows a
validated `f64` to the SDK's `i64`, four lines below the range check that bounds
it to `[-273.15, 80]`. No `TryFrom<f64>` exists to spell that, and a fallible
form would add an arm the check above already makes unreachable. This is the
"guard clippy cannot see" case from L5c, and it is what an `#[expect]` is for.

Notably `zwo`'s `MaxADU` shift did **not** need one, despite looking like the
better candidate: its guard bounds `bit_depth` to `2..=15`, where every step is
exact, so `checked_shl` is identical rather than a clamp — and the
hardware-validated ceilings (65504 / 65528 / 65520) stayed test-pinned across
the change.

### L5f — `rp-catalog`

56 production sites, all in `lib.rs`, to **zero**; the test module contributes
none. The crate was already built around total field readers (`first_chunk`
parses, `position`'s saturating widen, `checked_add` in the pool-string walk),
so what remained was the arithmetic *around* those readers, and it fell into
shapes with one answer each.

**Address computation now saturates, and that closes a documented gap rather
than appeasing the lint.** Release builds run with overflow checks off, so
`section + 4 * idx` over a corrupt count or row index *wrapped* — and
`materialize`'s own comment already said what a wrapped read does: land on an
unrelated byte and yield a fully-formed target with a plausible position. Two
const helpers (`section_end` for the layout chain, `field` for row addressing)
now spell every offset; saturation yields `usize::MAX`, which addresses
nothing, so every reader turns corruption into its documented miss value. A
corrupt header still cannot slip through `load`: a saturated layout can never
equal the blob's real length, so the exact-length check reports `Truncated`
exactly as before.

**The Levenshtein DP went index-free instead of getting an exemption.** The
two-row textbook form carried 16 of the 56 sites. The rewrite keeps one row
and carries the two values the recurrence needs that the row no longer holds —
the previous row's value one column back (`diag`) and the value just written
(`left`) — while `iter_mut().skip(1).zip(b.bytes())` makes the bounds the
iterator's problem. That drops the second vector and the swap, keeps the
cap-4 early exit that makes the ~20k-key fuzzy scan affordable, and its
saturating adds are simply the total spelling of a distance the cap truncates
anyway. The classic-form tests (kitten/sitting, cap truncation, empty edges)
and the fuzzy-suggestion tests over the real key table pinned the behavior
across the change.

**Two exemptions, the same shape as L5e's.** `scan_band` turns a validated
declination (±90° by `IcrsCoord`) and a guard-checked finite radius into
milliarcsecond band bounds; no `TryFrom<f64>` exists to spell it, a float-to-int
`as` saturates rather than wraps, and a saturated bound only widens the dec
band that the exact great-circle test then filters. The workspace's exemption
count for the L5 three stands at four, all f64-to-integer narrowings below a
visible guard.

### L5g — `skywatcher-motor-protocol`

42 production sites to **zero**, no new exemptions. A hex wire codec is the
lint trio's home turf — nibble arithmetic, frame indexing, masked narrowing —
and every shape turned out to have a total named spelling:

- **Slice patterns are the length check.** The frame validators and
  `AxisStatus::decode` replaced an explicit length guard plus indexing with an
  exhaustive match whose short arms (`[] | [_] => Err(too short)`) *are* the
  check and whose main-arm bindings are the proof. Every arm is reachable and
  already tested — unlike a `let`-else after a guard, whose else arm is dead
  code coverage can never reach.
- **Validation now hands back what it proved.** `Response::decode` re-indexed
  the frame `validate_response_frame` had just checked. A `pub(crate)`
  `split_response_frame` returns `(prefix, payload)`; the public validator
  keeps its API as a thin wrapper over it.
- **`to_le_bytes` is the named spelling of masked narrowing.** The three
  `(value >> n & 0xFF) as u8` casts in `encode_u24` and the mount-type low
  byte became `let [lo, mid, hi, _] = value.to_le_bytes()` — exact,
  const-stable, so every `const fn` stayed const.
- **The copy audit fired again (L5e's lesson).** The crate carried the nibble
  decoder twice (codec + response, identical except the error variant) and
  the encoder twice (command's arithmetic fn vs codec's lookup table). Both
  directions now live once in `codec.rs`. The response copy returned
  `PayloadError` for a non-hex byte, contradicting the GTi service's
  documented taxonomy (`HexError` = non-hex byte, `PayloadError` = wrong
  shape); unification made the docs true, and no test pinned the old variant.
- The debias now mirrors the bias: `decode_position` uses `wrapping_sub`
  where `encode_position` already used `wrapping_add` — bit-exact, and the
  high byte `decode_u24` zeroes makes overflow unreachable anyway.

### L5h — `star-adventurer-gti`

31 production sites to **zero**. The service side of the mount arc: where the
protocol crate's operands were nibble-sized, everything here is bounded by the
wire's 24-bit fields (positions ±2²³, CPR and TMR_Freq ≤ 2²⁴), so the tick
sums live five-plus bits inside `i32` and `saturating_*` is simply the total
spelling — with the useful property that an impossible operand (a corrupt
config tick pair, say) degrades to a clamped sweep interval instead of a
release-mode wrap onto the wrong side of a slew-safety check.

- **The saturating-round semantic is named once, not exempted four times.**
  The four `(…).round() as i32/u32` casts (tick conversions in `units.rs`,
  step periods in `coordinates.rs`) route through `sat_round_i32` /
  `sat_round_u32` in `units.rs`, each carrying the crate's only `#[expect]`:
  float-to-int `as` has saturated since Rust 1.45 and no fallible `f64`
  conversion API exists, so the helper's contract *is* the saturating round.
  Call sites stay total, and the textual exemptions sit in one file — the
  shape `rp`'s much larger rung can reuse.
- **The copy audit fired a third time.** Four sites spelled the same
  subtract-then-fold dance (`RaTicks::new(a - b).fold_to_canonical_band(cpr)`)
  across the slew issue path and the pickup watcher; a typed
  `canonical_delta_since(prev, cpr)` on `RaTicks`/`DecTicks` now holds the
  saturating subtract beside the fold whose invariants justify it — `since`,
  not `from`, so the name doesn't borrow the `From` conversion vocabulary
  for what is a computation.
- **A deadline is a start plus an elapsed check.** The two poll loops that
  computed `Instant::now() + timeout` compare `start.elapsed()` against the
  timeout instead — same instant, no overflowing operator, and the retry
  counter that logged `attempt + 1` now just counts from 1.
- The codec's response normalizer traded its index-arithmetic tail trim for
  `split_last` + `ends_with` — the `\r\n` case is a pattern arm, not offset
  math.

Test-side sites (BDD steps, `world.rs`, in-file test mods) stay, matching
every prior rung's scope: the rung zeroes the production census, and the
test-side sweep belongs to the deny flip.

### L5i — `rp-fits`

26 production sites to **zero**, all in `writer.rs` and `reader.rs`. The
hand-rolled writer's arithmetic is length-and-padding bookkeeping against two
constants (`CARD_SIZE` 80, `BLOCK_SIZE` 2880), every quantity a live buffer
length capped at `isize::MAX` — `saturating_*` is the total spelling
throughout, and the card-overflow guards keep their exact semantics because
each subtraction is preceded by the comparison that bounds it.

- **The BZERO encode needed no exemption at all.** `(i32::from(p) - 32768)
  as i16` is exactly `p.wrapping_sub(32768).cast_signed()` — the u16→i16
  bias shift *is* a two's-complement wrap-then-reinterpret, and std has
  named spellings for both halves since 1.87.
- **Two `#[expect]`s, both on typed homes** (workspace count 8) — Igor
  vetoed a free-floating cast helper, and both callers turned out to have
  natural owners. `Pixels::scaled_to_i32(bscale, bzero)` hosts the
  physical-value equation: the guarded in-range truncation and the
  i64→f64 pixel widening live inside the method that owns the semantics.
  `KeywordValue::as_real()` names the FITS Int-or-Float card duality
  (foreign writers routinely emit `BZERO = 32768` as an integer card)
  for every numeric-keyword consumer, not just BSCALE/BZERO. A `From`
  impl was considered and rejected: i64→f64 is lossy past 2⁵³, and
  `From` is lossless vocabulary — std omits that impl for the same
  reason. Routing BSCALE/BZERO through the shared card mapping also
  turned a *corrupt or non-numeric* card from a silent fall-back to the
  default into a parse error — absent and undefined-value cards still
  take the default, but a present card that cannot mean a scale factor
  no longer quietly rescales every pixel.
- **The NAXIS bounds check became a slice pattern.** `let &[naxis1, naxis2]
  = naxis else` replaces the `len() != 2` guard *and* the four `naxis[0]` /
  `naxis[1]` indexings; the two fixed-size key-buffer copy loops zip
  destination and source instead of indexing by position, which also
  subsumes `pad_key`'s `.take(8)`.
- **Three padding sites simplified past the lint.** `push_padded_left`'s
  branch-and-count-loop is `repeat_n` over a saturating width;
  `pad_to_block`'s remainder dance is `len().next_multiple_of(BLOCK_SIZE)`;
  the comment truncation's re-sliced `min` is `bytes().take(max_comment)`.

Test-side residual: 2 sites (the u16-recovery casts inside unit tests) —
deny-flip territory, per the rung scope above.

### L5j — `session-runner`

44 production sites to **zero** — and the census surfaced a real,
input-reachable panic, not just spellings: a document poll trigger's
`interval` is user-authored, humantime accepts second counts right up to
`u64::MAX` (`"18446744073709551615s"` parses), and the engine's
`Duration`-typed deadline add (`monotonic() + interval`) overflows and
panics there. Per tenet 3 of the service's own design ("everything
validates before anything moves"), the fix is a validation rule, not a
runtime clamp:

- **Every document duration is now capped at 24 hours** — enforced once
  in `duration::parse_duration`, the shared gate for validation and
  parameter binding, so poll intervals, timeouts, backoffs, cooldowns,
  and duration parameters are all covered. A session is a single night;
  the `w`/`y` units stay surface-legal but unusable at full magnitude.
  The engine's two deadline adds become `checked_add` with the cap as
  their range proof.
- **The exact-integer boundary got a type, not a helper.** The three
  copies of the 2⁵³ dance (`fract() == 0` + magnitude guard + cast — tool
  arg canonicalization, loop bounds, array indexing) collapse into
  `expr::num::ExactInt`, an invariant-carrying newtype: the constructor
  validates, `as_i64` is exact *by construction* and carries the crate's
  only `#[expect]` (workspace count 9), and the unsigned views derive
  through `u64::try_from`/`usize::try_from` with no casts at all.
- **`seconds_until`'s cast vanished into chrono**: `signed_duration_since`
  + `TimeDelta::as_seconds_f64()` replace the operator subtraction and
  the hand-rolled seconds-plus-nanos float assembly.
- The trigger pump's parallel-array indexing (`queued[idx]`,
  `poll_due[idx]`) became `get`/`get_mut` with skip-on-absent — index and
  length are equal by construction (both vecs are built from the same
  `triggers` slice), so the fallback arms are dead by invariant.
- The rest is the mechanical catalogue: counters and recursion depths to
  `saturating_add` (parse depth caps at 64; `\u` escapes are exactly 4
  hex digits), the alternatives list to a `[rest @ .., last]` slice
  pattern, the SSE frame drain's delimiter offset saturated.

Test-side residual: 32 sites (engine golden tests, conformance and prop
tests) — deny-flip territory, per the rung scope above.

### L5t — the test-side answer, decided per scope

The L5 rungs zero the production census crate by crate, but the deny flip
gates `--all-targets --all-features`, so the test-side debt needs an answer
too. Re-measured after L5j with a two-run census — `--lib --bins` on default
features for what ships, `--all-targets --all-features` for what the gate
sees — the workspace held **1,081** sites: 428 production, 379 in `tests/`
targets, 256 in `src/` code the shipped build never compiles, 18 in `rp`'s
fixture-generator example. Decided with Igor, one answer per scope:

- **`tests/` targets (379) — crate-root allows**, the L3 panic precedent
  extended: the three lints join the `#![allow(...)]` each test-crate root
  already carries (or a fresh block where none existed — two integration
  roots). A panicking index in a test is a loud test failure, which is what
  tests are for; no knob exists for `arithmetic_side_effects` or
  `as_conversions`, and cucumber step fns are invisible to the knobs anyway.
  25 roots annotated.
- **`#[cfg(test)]` mods inside `src/` (~160) — one
  `#![cfg_attr(test, allow(...))]` per crate root**, 18 crates. The attribute
  is active only in the test-profile compilation, and every production line
  is still linted in the non-test compilation of the same target, so nothing
  escapes; per-mod attributes would have said the same thing a hundred times.
- **Mock simulators and `bdd-infra` (96) — fix as production, in their own
  rungs.** They are ordinary lib code (feature-gated, so neither knob nor
  `cfg_attr(test, ...)` reaches them), and the precedent is L5c: the
  `qhyccd-rs` simulator got the full treatment and surfaced real defects — a
  mock that wraps silently corrupts the very oracle the tests assert
  against. The census after this sweep names the set exactly: `bdd-infra`
  57, `star-adventurer-gti` 17, `qhy-focuser` 13, `sky-survey-camera` 4,
  `pa-scops-oag` 3, `pa-falcon-rotator`/`ppba-driver` 1 each.
- **Examples (18)** ride with their crate's rung (`rp`'s, in practice).

Verification: the post-sweep census reads 542 = 428 production + 96
mock/infra + 18 example, with the `tests/` and `cfg(test)` buckets at zero
— and the production census is byte-identical before and after, since no
annotation touches a non-test compilation.

### L5k — `polar-align`

65 production sites to **zero**, one new `#[expect]` (workspace count 10).
The crate is the workspace's geometry heart — its own 150-line `Vec3`/`Mat3`
— and the rung's centrepiece is the first use of clippy's type-level
exemption instead of per-site spellings:

- **`Vec3 ∘ Vec3` operators are declared panic-free in `clippy.toml`**,
  via `arithmetic-side-effects-allowed-binary = [["math::Vec3",
  "math::Vec3"]]`. The impls are componentwise `f64` arithmetic — the same
  property that makes clippy exempt float math on primitives, verified
  rather than assumed because the type is ours. The pair form deliberately
  blesses only that operand shape: a future `Vec3 * usize` still fires.
  Two measured spelling facts (clippy 0.1.95): the entry must equal
  `Ty::to_string()`, which prints **without** the local crate name
  (`math::Vec3`; both `Vec3` and `polar_align::math::Vec3` silently match
  nothing), and `[["*", "*"]]` can never match because the glob arm
  compares the other side literally.
- **The 1-based point number became the parameter.**
  `wait_for_manual_rotation` and `measurement_attitude` took a 0-based
  `usize` index and displayed `i + 1` seven times (once as `as u8 + 1`
  into the status field). They now take `point: u8` — the number ASCOM of
  the workflow actually speaks — and the measurement loop zips `(1u8..)`
  over precomputed per-point hints, writing into `[Vec3; 3]` /
  `[Option<Mat3>; 3]` arrays so the post-loop `centers[0..2]` reads are
  constant indices clippy can prove.
- **`tokio::time::sleep` is the total spelling of a deadline.** The
  adjustment loop computed `Instant::now() + max_duration` (panics on
  overflow); `sleep` performs exactly that add internally with a
  `checked_add` → far-future fallback, so a pinned `sleep` future replaces
  the deadline arithmetic outright.
- **`Mat3::column(j)` became `columns() -> [Vec3; 3]`** — every caller
  passed a literal, so the variable-index accessor was API surface nobody
  needed; `mul_mat` transposes first, turning each inner product into a
  row-by-row zip.
- **The stretch's float↔int seam got the `quantize` treatment**
  (`preview.rs::stretch`, the one `#[expect]`): percentile index, span,
  and the 0–255 map, each a one-liner whose widenings are exact below 2⁵³
  and whose narrowings land after a round/clamp. The percentile lookup
  folds its impossible-empty arm into the `PreviewError` the function
  already returns, matching L5b's rule.
- The rest is the catalogue: `serde_json` index-assign became a
  `Map::insert` build (the `args["…"]` sugar panics on a non-object),
  `checked_mul` turned the preview's geometry guard total, the subsample
  loop walks `chunks(native_w).step_by(stride)` instead of computing
  offsets, and counters/`Duration` division took `saturating_add` /
  `checked_div`.

### L5l — `phd2-guider`

20 production sites, and the first rung to close **without a single new
`#[expect]` or `clippy.toml` change** — every site mapped to a pattern an
earlier rung had already established.

- **Slice patterns absorb the length checks** (8 sites): `parse_roi`'s four
  `parts[i]` reads and the two JSON-array pairs in `client.rs` (lock
  position, camera frame size) were all index reads *after* an explicit
  `len() != N` check. `let [x, y] = arr.as_slice() else { return
  Err(...) }` folds check and access into one refutable pattern; the
  existing error messages moved into the `else` arms unchanged.
- **`serde_json` index-assign → `Map::insert`** (2 sites): `params["roi"]`
  and `params["temperature"]` — the sugar panics on a non-object; the file
  already built one request via `Map`, so this is also a consistency fix.
- **`v as u32` on PHD2's u64 JSON integers → `u32::try_from(v).ok()`**
  (3 sites): each cast already sat in an `ok_or_else` chain, so an
  oversized value now lands in the conversion error instead of truncating
  silently (message reworded in review to "Expected unsigned 32-bit
  integer…", truthful for both failure modes).
- **`sample_count` retyped `u32` → `usize`** (1 site, decided with Igor):
  the RMS window is capped at 50, so every option was safe; the retype
  makes `steps.len()` flow through `StatsSnapshot` and both `api.rs`
  response structs with no cast at all, and the JSON wire shape is
  unchanged.
- **Counters saturate** (3 sites): the reconnect `attempt` and the RMS
  `ra_n`/`dec_n` — all bounded in practice, `saturating_add(1)` makes the
  bound irrelevant.
- **Deadline arithmetic made total** (3 sites): `wait_for_settle` now
  computes its backstop with `Duration::saturating_add` and waits on a
  pinned `tokio::time::sleep` in a `select!` (the L5k adjustment-loop
  pattern — `sleep` is the total spelling of `now + d`); the stop-poll
  loop keeps its shape with `Instant::now().checked_add(stop_timeout)`,
  reading a `None` deadline as far-future (never times out), the same
  meaning tokio's own timers give an overflowing add.

Verification: the three lints report zero sites in `phd2-guider` on
`--lib --bins`; all 373 crate tests pass, one test assertion updated for
the `usize` retype.

### L5m — `rp-ephemeris`

17 production sites — 16 in `derived.rs`, 1 in `site.rs` — and only two
families: chrono `DateTime`/`Duration` operator arithmetic (12) and
`f64 as i64` millisecond casts feeding `Duration::milliseconds` (5).

- **All five casts were the same conversion** — fractional solar hours
  to a `Duration` (even the longitude shift: 240 000 ms/deg is exactly
  `degrees / 15` hours). Decided with Igor: a `SolarHours(pub f64)`
  newtype in `types.rs` owns the seam via `From<SolarHours> for
  Duration` under the rung's one `#[expect]` (workspace count 11) —
  beside `IcrsCoord`/`RiseSet`, module-internal (not re-exported).
- **Operator arithmetic became chrono's checked API, folding overflow
  into each function's existing "no answer" shape.** `bisect_dt`,
  `transit`, `rise_set` already return `Option`, so
  `checked_add_signed(...)?` adds no new paths; `twilight` returns its
  existing all-`None` window; `night_date` falls back to the unshifted
  local date, matching its documented graceful-degradation stance.
  `hi - lo` became `signed_duration_since` (total by construction — any
  two chrono instants' difference fits a `TimeDelta`), and the bisection
  midpoint halves whole milliseconds (`i64` division by a literal)
  through a small closure, sub-millisecond precision being irrelevant
  against the whole-second tolerance.

Verification: the three lints report zero sites in `rp-ephemeris` on
`--lib --bins`; all 42 crate tests (unit + reference values + leap-second
race) pass unchanged — the ERFA reference assertions pin the numeric
behaviour across the rewrite.

### L5n — `ppba-driver`

19 production sites in three shapes: 16 `indexing_slicing` in
`protocol.rs` (the two colon-split response parsers, both already
length-guarded), 2 `f64 as u8` dew-heater duty casts in
`switch_device.rs`, and 1 `len() as f64` mean divisor in `mean.rs`.

- **The parsers folded their `parts.len() < N` guards into slice
  patterns** (`let [_prefix, voltage, …, power_adj, ..] = parts.as_slice()
  else { … }`), the trailing `..` preserving the previous "at least N
  parts" tolerance for extra fields.
- **The duty casts moved behind a `PwmDuty(pub u8)` newtype in
  `protocol.rs`** (decided with Igor; the `SolarHours` pattern): the
  `SetDewA`/`SetDewB` variants now carry `PwmDuty`, and its `From<f64>`
  impl owns the one clamp-then-cast under the rung's one `#[expect]`
  (workspace count 12), keeping the 0-255 device fact beside the wire
  protocol that defines it.
- **A real NaN hole was closed while there**: NaN compared false against
  both range-validation bounds in `set_switch_value_internal`, so it
  slipped through and silently actuated (bool switches read it as off,
  the PWM cast saturated it to 0). The check now rejects non-finite
  values with the existing `InvalidValue` error, with a regression test.
- **The mean divisor took the lossless `u32` detour**
  (`f64::from(u32::try_from(len).unwrap_or(u32::MAX))`) — decided with
  Igor over an `#[expect]`, the window being poll-rate bounded.

Verification: the three lints report zero sites in `ppba-driver` on
`--lib --bins`; all 140 unit tests pass, plus new `PwmDuty` conversion
tests (round / clamp / NaN) and the NaN-rejection switch test.

### L5o — `doctor` and `rusty-photon-doctor-checks`

16 production sites across the doctor family — 13 in `services/doctor`,
3 in `rusty-photon-doctor-checks` (bundled per the mechanical-sweep
convention; the `doctor_toml.rs` hits the census also surfaces belong to
`rusty-photon-server-config` and wait for that rung). Every site fit a
shape an earlier rung had already settled, so no new decisions and no
new `#[expect]`s:

- **Counters became saturating** (`render.rs` ok/warn/fail tallies, the
  ACME retry counters, the ownership-scan counter, the renew.env
  1-based line number).
- **The two `scans[0]` collision attributions fold their `len() > 1`
  guards into `if let [first, _, ..]` patterns** — the pattern *is* the
  at-least-two check, and binds the first member it indexes for.
- **Guarded subtractions became total** (`entries.len() - SHOWN` under
  its `>` guard → `saturating_sub` + non-zero test; `zone_candidates`'
  provably-in-range tail slices → `labels.get(i..)`).
- **`time` arithmetic went through the checked API, folded into each
  site's existing shape**: both expiry-window comparisons rearranged as
  `not_after.checked_sub(window).is_none_or(|start| start <= now)`
  (underflow = window opened before representable time = due), and the
  two certificate validity ends moved behind a `validity_end(days)`
  helper that surfaces the (unreachable at the 10-year constants)
  overflow as a `TlsError::Config` instead of a panic.

Verification: the three lints report zero sites in both crates on
`--lib --bins`; the full Bazel gate and stable clippy `-D warnings`
pass unchanged.

### L5p — `pa-falcon-rotator`

13 production sites in two families:

- **6 `indexing_slicing` in `protocol.rs`** — the `FA` status parser's
  exact-six-fields read. The `len() != 6` guard folded into a rest-less
  slice pattern (`let [steps_field, …, reverse_field] = fields.as_slice()
  else { … }`), which *is* the exactly-six check.
- **7 `arithmetic_side_effects` at degree-newtype call sites**
  (`manager.rs` sync, `rotator_device.rs` position/target/move paths):
  `MechanicalDegrees + SyncOffset`, `SkyDegrees - SyncOffset`,
  `SkyDegrees - MechanicalDegrees`. Handled the `math::Vec3` way — the
  three frame-conversion pairs added to clippy.toml's
  `arithmetic-side-effects-allowed-binary`, each impl being a
  single-`f64` operation that cannot panic or wrap. Only the pairs the
  driver defines are blessed; a future mixed-type operator still fires.

No new `#[expect]`s. Verification: the three lints report zero sites in
`pa-falcon-rotator` on `--lib --bins`; full Bazel gate and stable clippy
`-D warnings` pass unchanged.

### L5q — `sky-survey-camera`

11 production sites in three files:

- **2 `arithmetic_side_effects` in `camera.rs`
  `build_full_sensor_request`** — the binned-pixel divisions. The
  `bin.max(1)` guard became the divisor's type:
  `NonZeroU32::from(NonZeroU8::new(bin).unwrap_or(NonZeroU8::MIN))` is
  `.max(1)` shaped as a `NonZero`, and `u32 / NonZeroU32` cannot panic,
  so the lint does not fire.
- **7 in `camera.rs` `crop_subframe`** — the row-walk arithmetic and
  `src[start..start + nx]` slicing. The bounds check now computes
  `x_end`/`y_end` via `checked_add` (usize overflow folds into the
  existing out-of-bounds error), and the copy loop walks
  `src.chunks_exact(src_w).skip(sy).take(ny)` with `row.get(sx..x_end)`
  instead of computed indices. A zero-area subframe returns early
  (empty crop, new unit test), which also pins `src_w >= 1` before
  `chunks_exact` sees it as a chunk size.
- **2 `arithmetic_side_effects` device-discovery counters**
  (`mount.rs`, `rotator.rs`): `idx += 1` → `saturating_add`, the
  standing counter pattern.

No new `#[expect]`s. Verification: the three lints report zero sites in
`sky-survey-camera` on `--lib --bins`; full Bazel gate and stable clippy
`-D warnings` pass unchanged.

### L5r — `rusty-photon-server-config`

10 production sites, all one family: `doctor_toml.rs`'s parser formats
`idx + 1` into every error message to report a 1-based line number.
These are the sites deliberately deferred from L5o (they live in
`server-config`, not `doctor`). Same shape as L5o's `acme_config.rs`
fix: one `let lineno = idx.saturating_add(1);` hoisted to the top of
the parse loop (the saturation is unreachable — a file with
`usize::MAX` lines does not fit in memory), and every message now
interpolates `{lineno}`. Error strings are byte-identical.

No new `#[expect]`s. Verification: the three lints report zero sites in
`rusty-photon-server-config` on `--lib`; full Bazel gate and stable
clippy `-D warnings` pass unchanged.

### L5s — `sentinel`

10 production sites in four families:

- **Bounded counters** — `corrective.rs`/`restart.rs`
  `attempt + 1 < RECOVERY_ATTEMPTS` and `state.rs`
  `consecutive_errors += 1` → `saturating_add`, the standing pattern.
- **Epoch-ms `as` casts** — the three per-module `current_epoch_ms()`
  helpers (`engine.rs`, `health.rs`, `watchdog.rs`) truncated
  `as_millis()`'s `u128` with `as u64` → `u64::try_from(..)
  .unwrap_or(u64::MAX)` (saturates in the year 584556019 instead of
  wrapping).
- **`Instant` deadline arithmetic** — `health.rs`'s restart gate
  (`Some(Instant::now() + wait)` → `Instant::now().checked_add(wait)`,
  same `Option` shape; unreachable overflow leaves the gate open
  rather than panicking the health loop) and `watchdog.rs`'s
  operation deadline (`Duration::saturating_add` for the buffer,
  `Instant::checked_add` via `and_then`; an expiry too far out to
  represent degrades to tracking the operation as untimed).
- **SSE frame drain** — `watchdog.rs` `buffer.drain(..idx + 2)` →
  `idx.saturating_add(2)` (`idx` locates a `"\n\n"`, so the bound
  always holds). `dashboard.rs`'s next-check epoch-ms for the JS
  clock likewise → `saturating_add` (display only).

No new `#[expect]`s. Verification: the three lints report zero sites in
`sentinel` on `--lib --bins`; full Bazel gate and stable clippy
`-D warnings` pass unchanged.

### L5u — the last three library crates

`rusty-photon-config` (1 site), `rusty-photon-shared-transport` (6) and
`rusty-photon-tls` (7) are each too small for a rung of their own, so this
rung bundles them — the L5o precedent. 14 production sites, all standing
patterns:

- **Capacity/offset arithmetic** — `config`'s
  `String::with_capacity(dotted.len() + 1)` and `tls`'s two
  bracket-literal offsets (`end + 1`, `end + 2`, both already inside
  `get(..)`) → `saturating_add`; `shared-transport`'s wire-trace tail
  count (`len - MAX_WIRE_TRACE_BYTES`, guarded by the surrounding `if`)
  → `saturating_sub`.
- **Trace-cap slicing** — `DisplayWire`'s `&self.0[..cap]` where
  `cap = len.min(MAX)` → `iter().take(MAX_WIRE_TRACE_BYTES)`, which also
  deletes the `cap` binding.
- **Frame-scan loop** — `read_frame_bounded`'s `budget = max - buf.len()`
  → `saturating_sub` (the loop's own budget check keeps `buf.len() ≤ max`,
  so the value is unchanged); the terminator scan `chunk[..scan_end]` →
  `iter().take(scan_end)`; the consumed prefix `&chunk[..pos + 1]` →
  `chunk.get(..=pos)` with an unreachable `Framing` error else-arm, the
  `crop_subframe` shape.
- **Request-head reader** — `read_request_head` already returns
  `Option`, so all four slice sites fold into the existing bail-out:
  `buf.get(..line_end)?`, `buf.get(..headers_end)?`,
  `chunk.get_mut(..read_len)?` (`capped_read_len` caps at `chunk.len()`),
  `chunk.get(..n)?`.
- **Plaintext-peek deadline** — `Instant::now() + PLAINTEXT_IO_TIMEOUT` →
  `checked_add` degrading to an already-expired deadline (drop the
  connection): fail-closed for a resource-sink guard, unlike `sentinel`'s
  health gate where the open shape was correct.

No new `#[expect]`s. Verification: the three lints report zero sites in
all three crates on `--lib --bins`; full Bazel gate and stable clippy
`-D warnings` pass unchanged.

### L5v — `bdd-infra`

The first rung of L5t's fix-as-production bucket, and the largest:
~69 sites on `--lib --bins --all-features` (the earlier per-crate rungs
ran default features, which is why these never appeared in them).

- **`rp_harness/config.rs` (33 sites, one shape)** — every hit is
  serde_json `IndexMut` insertion (`obj["key"] = json!(...)`) on a
  receiver that is a `json!({...})` object literal. A file-local
  `set_key` (`as_object_mut` + `insert`, `debug_assert` on the
  unreachable non-object arm) replaces them all; per-site
  `as_object_mut` dances would have said the same thing 33 times.
- **Reader-side JSON** — `doctor_smoke.rs`'s owned-`Value` assert and
  `omnisim.rs`'s Alpaca response fields move to `get()` chains with
  the same defaults the old `Index` (which returns `Null`, not a
  panic) produced.
- **Oracle-preserving fallbacks** — `guider_stub.rs`'s HFD script
  lookup clamps at the last entry via `get`; an empty script now
  yields `NaN` (`null` on the wire), which no HFD watch converges on —
  loud, where `0.0` could silently satisfy a below-threshold assert.
  `plate_solver_stub.rs`'s `Sequence` queue (asserted non-empty at
  `start`) degrades to a 500 response. CRVAL integer headers convert
  via `i32::try_from` → `f64::from`, out-of-range folded into the
  stub's existing `String` error path.
- **Standing patterns** — saturating index offsets (`test_service.rs`,
  `sse.rs`), `split_once` replacing `find` + slice arithmetic
  (`parse_bound_port`), `checked_add_signed` + `expect` naming the
  test-scale invariant (`computed_sky.rs`), `checked_rem` for the
  shard modulo (zero shards is a caller bug; shard 0 over-runs rather
  than panicking the harness), and `percent_decode` rewritten on
  `split_first`/slice patterns with no index arithmetic left.

No new `#[expect]`s. Verification: the three lints report zero sites in
`bdd-infra` on `--lib --bins --all-features`; full Bazel gate and stable
clippy `-D warnings` pass unchanged.

### L5w — the six services' mock/feature-gated code

The rest of the fix-as-production bucket, in one bundled rung: ~47
sites visible only on `--all-features` in `star-adventurer-gti` (18),
`qhy-focuser` (18), `pa-scops-oag` (6), `sky-survey-camera` (3),
`ppba-driver` (1) and `pa-falcon-rotator` (1).

- **Movement models saturate** — the SAG axis simulator, the qhy
  focuser mock and the scops OAG mock all step positions with
  saturating arithmetic (SAG's feeding the existing
  `clamp_to_wire_range`, scops staying a `const fn` — the saturating
  intrinsics are const).
- **Faithful-mock over panic** — SAG's `process_command` reads
  cmd/axis/payload via `get`; a frame too short to carry them earns a
  mount-error reply, which is what hardware does with a malformed
  request. The qhy mock's JSON fields move to `get()` chains; a
  nonsense speed saturates at `u8::MAX` rather than wrapping to a
  plausible one.
- **Range checks fold into `try_from`** — `extract_idx`'s manual
  `> u8::MAX` guard, SAG's `parse_position_ticks` 24-bit check (now
  `i32::try_from` then the `POSITION_MIN..=POSITION_MAX` contains),
  and the ASCOM position narrowing in `focuser_device` (out-of-range
  becomes `INVALID_OPERATION`). The qhy temperature parser's integer
  fallback arm turned out to be dead — `serde_json`'s `as_f64` returns
  `Some` for every JSON number — and was deleted rather than converted.
- **`parse_status` destructures once** — a ten-slot slice pattern
  replaces the length guard plus three indexed reads, the L5g
  `AxisStatus::decode` shape.
- **One new `#[expect]`** — the falcon mock's f64 step product
  (`signed * STEPS_PER_DEGREE`, bounded by the normalized degree
  range), joining the qhyccd-rs f64-narrowing exemption family. The
  attributes themselves are the exemption ledger (grep
  `expect(clippy::as_conversions`); the running totals earlier in this
  plan were stale the moment L5h grew the family and are not continued.
- **Display over cast** — the ppba mock's humidity prints
  `trunc().clamp(0.0, 255.0)`, the same digits the old saturating
  `as u8` produced for any in-range value.

Verification: the three lints report zero sites in all six crates on
`--lib --bins --all-features`; full Bazel gate and stable clippy
`-D warnings` pass unchanged.

### L5x — `rp`'s star detection

The `rp` finale opens with its densest cluster: `imaging/analysis/stars.rs`
(42 sites) and `fwhm.rs` (17). The census re-measured `rp` at 195
production sites across 39 files; the carve is star detection here, the
rest of imaging next, then `mcp/`, then the tail plus the
fixture-generator example.

- **`build_star`** — pixel reads via `view.get` with `?` folding the
  impossible out-of-bounds into the existing no-star `Option`; centroid
  coordinates convert `u32::try_from` → `f64::from` (lossless for any
  in-memory image); the zero-flux fallback reuses sums accumulated in
  the main loop instead of re-walking the component with casts.
- **`connected_components_4`** — `indexed_iter` over the mask,
  `checked_sub`/`checked_add` neighbour offsets folded into `mask.get`'s
  bounds check, `visited` through `get`/`get_mut`: no indexing left in
  the BFS.
- **`fit_2d_gaussian`** — one statement-scoped `#[expect]` for the two
  `f64` → `isize` centroid casts (the cast saturates on an absurd
  centroid and the stamp-bounds check rejects exactly those values; no
  total spelling exists); saturating stamp geometry; stamp pixels via
  `i32::try_from` → `f64::from` and `view.get` with `?`.
- **`StampFitter::eval`** — a six-slot slice destructure with
  `MPError::Eval` on the impossible wrong-arity call replaces six
  indexed parameter reads.

Verification: the three lints report zero sites in both files on
`--lib --bins`; full Bazel gate and stable clippy `-D warnings` pass
unchanged.

### L5y — the rest of `rp`'s imaging

27 sites: `background`/`stats`/`snr`/`hfr` analysis plus the
`measure_basic`, `measure_stars` and `auto_focus` tools.

- **Median helpers** — the three copies of the sorted-median shape fold
  `get()`/`checked_sub` into their existing `Option` surfaces;
  `background`'s infallible variant gains a real emptiness guard
  returning `NaN` (the review caught that without it,
  `select_nth_unstable_by` panics before any fold is reached — the
  guard makes the no-panic contract true); `stats`' even-length midpoint
  moves from `i64::midpoint` + `as i32` to `i32::midpoint` outright.
- **Counts** — star/pixel counts to `u32`/`u64` via `try_from` with
  saturating fallbacks; per-star pixel counts to `f64` via
  `u32::try_from` → `f64::from` on the `Option` surfaces.
- **Three statement-scoped `#[expect]`s** — two `usize` → `f64`
  pixel-count means (exact below 2^53; no total spelling — the
  polar-align `stretch` precedent) and the parabola vertex's
  `f64` → `i32` round (`as` saturates; a rail-hitting vertex is a
  degenerate fit the caller's grid-range check rejects).
- **Auto-focus grid estimate** — saturating/checked arithmetic folding
  the validated-nonzero `step_size` division into the `GridTooLarge`
  error path.

Deferred, recorded by the L5x review: `detect_stars`' `rows > 4` guard
is insufficient for `gaussian_filter` at `smoothing_sigma > 1.0` —
unreachable today (all four call sites hardcode `1.0`), to be tightened
if the sigma ever becomes configurable.

Verification: the three lints report zero sites under
`services/rp/src/imaging`; full Bazel gate and stable clippy
`-D warnings` pass unchanged.

### L5z — `rp`'s MCP layer

46 sites — counted as census diagnostics on `--lib --bins` (one line
can carry several, and the 13-router `+` chain counts once):
`internals.rs` (26) plus the built-in tool modules and the handler.

- **Deadline arithmetic** — every `Instant + Duration` poll deadline
  (capture, focuser, slew, park, rotator, cover calibrator, guide-frame
  sweep) moves to `checked_add` degrading to an already-expired deadline
  (immediate timeout, not a panic). This closes a genuinely reachable
  panic: an absurd-but-config-valid `steps_per_sec` could produce a
  `Duration` that `try_from_secs_f64` admits but the clock cannot carry.
  Durations compose with `saturating_add`/`saturating_mul` first.
- **Envelope milliseconds** — the three
  `(secs * 1000.0).round() as u64` deadline envelopes share one
  statement-scoped `#[expect]` shape (`as` saturates;
  `try_from_secs_f64` already rejected out-of-range budgets), and the
  same treatment covers the filename sensor-temperature round and
  `truncated_minutes`' bounded coordinate narrowing (joining its
  existing `cast_*` allow).
- **The router merge** — rmcp `ToolRouter`'s overloaded `+` is a catalog
  merge, not arithmetic; the sum moves into a `merged_tool_router()`
  associated fn carrying the `#[expect(arithmetic_side_effects)]` with
  that reason.
- **Counters and counts** — poll streaks, retry counters and capture
  sequence numbers saturate; star/filter counts convert via `try_from`
  with saturating fallbacks; the slug-suffix allocator's `n += 1`
  becomes `checked_add` folded into its `String` error (exhausting u32
  suffixes now errors instead of wrapping).
- **Pixel and grid folds** — the FITS-cache `clamp(0, max_adu) as u16`
  becomes `try_from` (the guard already proved `max_adu` fits `u16`);
  the two no-op `bin[i] as u8` casts are deleted; the guide-metric
  median and the parabola grid bounds move to `get`/`first`/`last` on
  their existing error paths; `v["progress"]` insertion goes through
  `as_object_mut`, gaining the same `debug_assert` the campaign's other
  object-literal inserts carry (the old `IndexMut` form panicked on any
  non-object, non-`Null` value).

Verification: the three lints report zero sites under
`services/rp/src/mcp`; full Bazel gate and stable clippy `-D warnings`
pass unchanged.

### L5aa — `rp`'s tail, the example, and census zero

The final production rung (~53 census diagnostics): persistence/cache,
the equipment layer, guiding_watch, config, cooling, routes, planner,
events, one late-arriving `rp-targets` site, and the fixture-generator
example that L5t assigned to ride here. With this rung `rp`,
`rp-targets` and the whole imaging/MCP/service core read zero.

**Residual before the deny flip** (the review's catch — this section
first claimed workspace zero): 23 production sites in five services the
ladder never assigned a rung — `ui-htmx` 6, `plate-solver` 5,
`calibrator-flats` 5, `planetarium-bridge` 4, `dsd-fp2` 3. All are
pre-campaign code inheriting the workspace lints. L5ab zeroes them; the
deny flip must not run before it.

- Ten identical equipment device-scan counters saturate; the alpaca
  backoff shifts take saturating exponents.
- `trains.rs`'s topological sort destructures its `windows(2)` pairs
  and walks the in-degree map through `get`/`entry` with saturating
  counts.
- `guiding_watch`'s window slices fold into their length guards, and
  its `median` gains the honest emptiness guard the L5y review taught
  (`NaN`, conspicuous, placed before the work — L5y's finding was a
  guard documented but not actually present).
- Cache byte accounting saturates end to end; the `i64` delta builds
  from `try_from` + `unsigned_abs`, deleting the negation.
- chrono subtraction moves to `signed_duration_since`; the sun-trend
  resample degrades to a flat trend on the unreachable overflow,
  falling to the recoverable wait branch.
- One new scoped `#[expect]` in production (the cooler rung round,
  guarded by ladder membership) plus two in the fixture generator's
  clamp-bounded f64 narrowings.

Verification: `-p rp -p rp-targets --lib --bins` and `-p rp --examples`
all report zero; full Bazel gate and stable clippy `-D warnings` pass
unchanged. **Next: L5ab (the five skipped services), then the deny
flip** — the three lints move into `[workspace.lints.clippy]` at deny
only after a fresh full-workspace `--all-targets --all-features` census
reads zero.

### L5ab — the five residual services, production census zero

The 23 sites L5aa's review surfaced — pre-campaign code in the five
services the ladder never assigned a rung: `ui-htmx` (6),
`plate-solver` (5), `calibrator-flats` (5), `planetarium-bridge` (4),
`dsd-fp2` (3). Every one lands on a standing pattern; no new shapes.

- The two `calibrator-flats` MCP argument builders drop serde_json's
  panicking `IndexMut` for `as_object_mut` + `insert` behind a
  `debug_assert` (the L5v `set_key` shape); the flat-count and
  iteration counters saturate; the target-ADU narrowing keeps its `as`
  under a scoped `#[expect]` — the saturating NaN→0 float cast is
  exactly the clamp an unvalidated `target_adu_fraction` needs.
- `dsd-fp2` folds both post-guard `as u16` casts into `u16::try_from`
  (the brightness ceiling guard stays — 4096 < `u16::MAX`, so
  `try_from` alone would widen the accepted range) and saturates the
  paren-scan offset already inside a `get`.
- `planetarium-bridge` saturates the spool line numbers, drop counter
  (now annotated `u64` — inference needs the type once `+=` is gone)
  and replay backoff (`Duration::saturating_mul`), and the health
  gauge takes the `u64::try_from(...).unwrap_or(MAX)` spelling.
- `plate-solver`'s FITS block pad saturates (exact: the remainder is
  bounded by the modulus); `coerce_float` converts integer cards via
  `i32::try_from` + `f64::from` into the existing `NonNumeric` error —
  WCS quantities are small reals, an integer beyond i32 is bogus data,
  not a value to round; the stderr tail-trim subtractions saturate
  under their existing guards and the read-chunk slice folds into
  `get`, degrading a contract violation like a read error.
- `ui-htmx` folds the probe-tier write and the roster replace into
  `get_mut` (the replace's unreachable miss reuses
  `SurgeryError::MalformedConfig`, the sibling arm's error), rewrites
  the single-element schema-type check as a slice pattern, and
  saturates the pointer capacity, goal label, and post-push index.

Verification: the five-crate `--lib --bins --all-features` census
reads zero. **This closes the L5 production ladder** — every crate
that inherits `[lints] workspace = true` is at zero. The dual-homed
FFI crates remain L7's bucket (~24 production sites in `zwo-rs`,
`svbony-rs`, `libqhyccd-sys`); none of the six carries a `[lints]`
section, so the deny flip cannot touch them. The flip remains gated
on its own fresh full-workspace `--all-targets --all-features`
census.

### L5 deny flip — the three lints join `[workspace.lints.clippy]`

The ladder's terminal step: `as_conversions`,
`arithmetic_side_effects` and `indexing_slicing` move to deny in the
workspace lint table, so every inheriting crate now fails `cargo
clippy` on a new site instead of accumulating debt. Enforcement is
cargo-side only (pre-commit hook + `check.yml`) — Bazel runs no
clippy, same as the L1–L4 lints above.

Gate evidence (fresh census on the flip's base commit, all targets,
all features, all workspace members; counted as unique `(file, line)`
primary spans — raw diagnostic count is higher because one line can
carry several lints): 274 diagnostics, every one outside the flip's
reach — 192 in `libzwo-sys`'s *generated* `bindings.rs` (build
output, not source; this number is host-dependent, bindgen runs over
the locally installed SDK headers), and 82 across `zwo-rs` /
`qhyccd-rs` / `svbony-rs` source, across their lib, test and example
targets. None of
the dual-homed FFI crates carries a `[lints]` section, so the flip
changes nothing for them; retiring those 82 sites is [L7](#l7--dual-homed-ffi-crates)'s
separately-decided scope. Every `[lints] workspace = true` crate
reads zero.

Test scope stays exempt through the existing three-layer mechanism
(clippy.toml's `allow-*-in-tests` knobs where they exist, crate-root
`#![cfg_attr(test, allow(...))]` for the knobless two, file-level
allows in `tests/bdd/` entry files — see the Cargo.toml comment
block); the scoped production `#[expect]`s placed during L5 are the
exemption ledger, and an obsolete `#[expect]` fails the build (as it
already did pre-flip — `#[expect]` is level-independent, so
`unfulfilled_lint_expectations` fires under `-D warnings` whatever
the ambient lint level).

Census scope caveat: the census is linux-gnu. `#[cfg(windows)]` /
`#[cfg(target_os = "macos")]` production code is outside it — and
outside every CI gate (`check.yml`'s clippy leg is ubuntu-only;
Bazel runs no clippy, and its `-Dwarnings` is rustc's set, which
never evaluates `clippy::` tool lints). The flip's review audited
all 131 such items: clean, by hand and — where the `aws-lc-sys`
cross-build allows — by `cargo clippy --target
x86_64-pc-windows-msvc`. A violation there would have surfaced only
on a Windows contributor's pre-commit hook, not in CI; that hole is
now closed off-PR by check.yml's `windows / clippy` + `macos /
clippy` legs (#984), which run the full `-D warnings` clippy on those
hosts on push to main and nightly — and their first dispatch run
caught three live `clippy::unimplemented` sites the audit's scope had
not included (test-side, `ui-htmx`'s browser harness stubs; the audit
counted production items). The `--all-features` blind spot the legs
initially shared — `simulation` ON in every clippy run anywhere, so
`#[cfg(all(windows, not(feature = "simulation")))]` production code
was compiled by no clippy at all — is closed too (#988): every clippy
job and the pre-commit hook now run a second, default-features pass
(`--workspace --lib --bins`). The pass's shape is load-bearing, both
points found by the #990 adversarial review: it is `--lib --bins`
because with `--all-targets` the dev-dependency edges (four services
force `simulation` onto their FFI wrapper; `rp` forces `mock` onto
rp-guider/rp-plate-solver) re-enable the features via resolver
unification, silently dropping five crates back out of the complement
(unit-graph-proven); and "every crate declares `default = []`" is
false — zwo-rs/libzwo-sys default to their device features (harmless:
still sim-off). The census of the never-linted surface found real
violations: platform-conditional C-type conversions in the FFI
wrappers (`c_char`/`c_long` differ per OS — a cast clippy calls
useless on one target is required on another), fixed at the site
(byte-exact `from_ne_bytes`; svbony-rs's `ffi_util::c_long_field`
with a cfg'd-to-LP64-unix allow) rather than crate-wide. Everything
else was clean. Residuals, recorded: test-target code behind
`not(feature = ...)` is compiled by neither pass; mixed
feature-on+feature-off cfg gates exist ONLY in zwo-rs (src/lib.rs
`all(not(simulation), any(...))` — covered, its defaults enable the
device features), a future one elsewhere escapes both passes (hack's
powerset still rustc-compiles it), and finding them needs a
paren-aware cfg scan — the multi-line form is invisible to a line
grep, which is exactly how the first "none exist" claim went wrong.

## L6a — split the CI channels

Being strict on stable and getting early warning from beta are two goals, and
running one job for both forced a compromise on the stricter one. `check.yml`
now runs them as separate jobs:

- **`stable / clippy`** — unchanged required gate, `-D warnings`.
- **`beta / clippy`** — report-only, and on the schedule plus
  `workflow_dispatch` alone. Deliberately *not* on push to main: only the
  scheduled run acts on the census, so a per-merge beta build would compute a
  report nobody reads. `--cap-lints warn` on clippy's
  argument line downgrades every lint, *including the ones
  `[workspace.lints.clippy]` denies*, so the job exits 0 on lints and fails
  only on a genuine compile break. The cap rides on the argument line rather
  than `RUSTFLAGS` so it applies to the workspace packages being linted and
  leaves dependency artifacts cached.

`tools/ci/beta_clippy_census.py` aggregates the JSON diagnostics per lint
(deduplicating on file/line/column — `--all-targets` reports each source line
once per target, over-counting by ~40%), and a `github-script` step keeps one
`beta-clippy`-labeled issue per lint: opened on first sighting, body rewritten
each night, closed automatically once the lint stops firing. Above 20 distinct
lints it opens nothing and fails instead — truncating the set would make the
auto-close wrongly retire the lints left out, so a mass rename upstream goes to
a human.

The property that makes this cheap: because `stable / clippy` gates every PR at
`-D warnings`, `main` is silent on stable, so **every** finding beta reports is
new on the beta channel. No stable-vs-beta set differencing is needed.

`notify-clippy-failure` covers the stable, OS (#984), and beta clippy jobs;
its body distinguishes the legs where a failure likely IS a lint (`windows /
clippy` / `macos / clippy`, whose purpose is OS-cfg'd violations) from those
where it is not (beta caps lints and files per-lint issues instead).

## L6b — `pedantic` / `nursery` at deny

The ladder's last wide rung: both groups join `[workspace.lints.clippy]` at
`deny`, finishing the target set at the top of this plan. L6a removed the
one structural objection (both groups gain lints on the beta channel; beta
now reports instead of failing), and L5 removed the overlap (its three lints
covered most of the old `cast_*` estimate).

### Fresh census (2026-08-14)

Three passes on stable clippy 0.1.96 — the same toolchain as the L2/L5
censuses — driving `-W clippy::pedantic -W clippy::nursery` with
`--message-format=json`, deduplicated on (lint, file, line, column) via
`tools/ci/beta_clippy_census.py`'s logic: `--all-targets --all-features`
(everything), `--all-features` without `--all-targets` (the production
split), and `--lib --bins` on default features (the #988 complement).
**In scope** means the `[lints] workspace = true` crates; the dual-homed FFI
crates carry 324 further sites that belong to [L7](#l7--dual-homed-ffi-crates).

| In-scope bucket | Sites |
|---|---:|
| All targets, all features | 3,964 |
| Production (lib/bins) | 1,461 |
| — doc trio (`missing_errors_doc` 486, `too_long_first_doc_paragraph` 265, `missing_panics_doc` 24) | 775 |
| — non-doc production | 686 |
| Test-side | ~2,503 |
| Default-features-only complement | **4** |

The old post-L2 table sized this rung at 4,257 sites with 349 of them in the
`cast_*` quartet; the fresh census reads 3,964 with ~45 — L5's fixes and
exemption ledger absorbed the difference, which is the re-measure lesson
holding one more time. The test side is dominated by cucumber macro shape:
`needless_pass_by_ref_mut` 1,192 (steps take `&mut World` whether or not they
mutate), `unused_async` ~265 (steps are `async` whether or not they await),
`used_underscore_binding` ~203. The four-site complement (three
`must_use_candidate` in mock-gated camera backends, one `unnecessary_wraps`
in doctor) means the `--all-features` blind spot is negligible this rung.

Top in-scope production lints (non-doc): `significant_drop_tightening` 122,
`option_if_let_else` 96, `manual_let_else` 71, `needless_pass_by_value` 53,
`suboptimal_flops` 50, `too_many_lines` 35, the `cast_*` quartet ~45 (almost
all under L5's `#[expect(clippy::as_conversions)]` ledger — an expect for one
lint does not cover another, so those entries widen rather than refactor),
`match_same_arms` 23, `missing_const_for_fn` 18. Per crate, `rp` leads again
(175 non-doc prod sites), then `star-adventurer-gti` 65, `polar-align` 44,
`session-runner` 40, `sentinel` 34.

### Decisions (2026-08-14)

- **Every lint in both groups ends at deny — no permanent carve-outs.** The
  sequencing controls risk instead: shallow one-answer lints first, judgment
  lints later, the lock-scope rewrites and the analysis-math flops last,
  when the rung has built pattern knowledge.
- **The doc lints are written, not allowed** — as their own sub-rung at the
  end; the flip waits for it. The bar is an accurate 1–2 sentence summary of
  the failure classes the function actually returns, read from its body and
  error enum — no exhaustive variant catalogs (they drift stale), no history.
- **`suboptimal_flops` splits easy/hard.** Strict-win rewrites in
  non-analysis code go early; the residue in `rp/src/imaging/analysis/` —
  where `mul_add` changes the last ulp of what feeds autofocus and hides the
  shape of the math — is decided per site at the end, with the same-process
  A/B harness from L5c where the loop is hot.
- **Packaging is hybrid, decided before sweeping** (the L2 lesson): by-lint
  family PRs for shallow lints so each rule is reviewed once, per-crate PRs
  for judgment lints, every PR under Copilot's 300-file cap.
- **Test scope: clean first, then a curated named allow list.** The
  mechanical and cheap-hand sites (~150, real readability value) are fixed
  before any allow lands — an allow beats the census's `-W` flags. What
  still fires afterwards (the macro-shaped signature lints plus judgment
  lints, ~15 names) goes into the L5t attribute mechanism as one canonical
  named list; everything else in both groups stays enforced on test code
  after the flip. No `clippy.toml` knob exists for any of this.
- **`too_many_lines` (35) is per-site judgment**: split where a real seam
  exists, `#[expect]` with a reason where the function is one cohesive table
  that splitting would obscure.

### Slice sequence

Every slice: measure → fix → re-measure (the `cargo fix` silent-revert
gotcha makes the residual count the only trustworthy signal) → the local
quality gate → PR. Since every lint that fires is in `pedantic` ∪ `nursery`
(main is clean on the on-by-default and denied sets), no set differencing is
needed; group membership is verified per slice where it matters.

- **B1a — test-side cleanup (~150 sites).** The 44 machine-applicable sites
  (`suboptimal_flops` 36 in test fixtures, `redundant_closure_for_method_calls`
  5, three singletons — `string_lit_as_bytes` gets a hand check, it is the
  L2 revert trap), then the cheap hand sets: `unreadable_literal` 62,
  `ignore_without_reason` 10 (write the actual reason),
  `default_trait_access` 22, `items_after_statements` 25. Flops fixes in
  fixtures can move a strictly-compared expected value by an ulp; the full
  Bazel test gate is the check. The signature churn
  (`needless_pass_by_ref_mut`, `needless_pass_by_value`'s test share,
  `unused_async`, `used_underscore_binding`) is deliberately not fixed.
- **B1b — the curated test-scope allow list.** The post-B1a re-census found
  a 52-site shallow residue across 15 lints that B1a's families had not
  covered; rather than let the curated list balloon to 32 names, the residue
  was fixed by hand first (three sites kept a reasoned `#[expect]` instead:
  a `needless_collect` that launders a borrow into a `'static` boxed
  iterator, `CountingHooks`' deliberate `_calls` field postfix, and a
  sentinel notifier template whose `{name}` braces are template syntax).
  The surviving lints — 17 names, all inherent to test-code shape — are the
  canonical list, applied on the existing mechanism: crate-root
  `#![cfg_attr(test, allow(...))]` plus the per-entry-file allows for every
  file directly under `tests/` (each is its own crate root; the knobs never
  see cucumber step fns), 130 files total, enumerated from `cargo metadata`
  targets. rp's two fixture examples (their own targets, so outside both
  carriers) carry minimal per-file allows for exactly what fires in each —
  the cast trio + flops in the generator, flops alone in the checker —
  instead of the full list. One canonical list, documented in the `Cargo.toml`
  comment block. `bdd-infra/src/` stays production scope, as in L5v.
  Verified: the all-targets census equals the production census.
- **B2 — quick wins, two PRs.** B2a (87 sites, this PR): the non-flops
  sweep, all hand-fixed — per-lint `cargo clippy --fix` applies *nothing*
  for lints enabled only via a command-line `-W` on this toolchain (three
  workspace rounds plus a single-crate probe all produced firing
  diagnostics and zero applied fixes), so the hand-fix-with-re-measure
  workflow is the whole story, not just the fallback. Highlights: the
  `suspicious_operation_groupings` × 4 in `auto_focus.rs` were
  hand-verified against the normal-equation matrix — all four are textbook
  Cramer's-rule cofactor expansions, a known false-positive shape — and
  became a three-column `det3` helper whose first-row expansion reproduces
  the inlined arithmetic bit-for-bit; `future_not_send` was fixed at the
  root by bounding `ConfigurableDriver::Config` (`Send + Sync`) and
  `::Overrides` (`Sync`); sentinel's message templates migrated from
  `{name}` to `%name%` placeholders (a deliberate breaking config change,
  chosen over a standing `#[expect]` — the lint now enforces the
  format-args bug class in sentinel too); the cooler warm-up ramp became
  an `iter::successors` rung ladder paced by a deadline-anchored
  `tokio::time::interval` (`MissedTickBehavior::Skip` — late wakeups
  under load neither stretch the ramp nor burst missed rungs; no float
  loop condition, no cast, no lint attributes at all). Three production
  `#[expect]`s were kept with sign-off: `unused_async` on
  `bind_dual_stack_tokio` (async-as-runtime-contract), `needless_collect`
  in i18n `filenames_iter` (external trait demands a `'static` boxed
  iterator), and `unnecessary_wraps` on planetarium's serde `default =`
  fn (must return the field's `Option` type). B2b measured empty
  (2026-08-16): all 50 remaining in-scope flops sites are
  `mul_add`-suggestion `suboptimal_flops` — the L2 pass already took
  every strict win, and the only `hypot` suggestions left live in
  qhyccd-rs (L7 scope) — so the flops residue folds into B8 wholesale
  (`mul_add` is per-site judgment: last-ULP shifts, obscured matrix
  math, wins only where FMA hardware is guaranteed).
- **B3 — must-use attributes (22 sites) — DONE (2026-08-16).**
  `#[must_use]` on the 19 consumed-self `with_*` builder methods
  (`return_self_not_must_use` — dropping the returned builder discards
  it silently) and on the 3 hardware-handle `new` constructors only the
  default-features pass sees (`must_use_candidate` on the
  svbony-camera / zwo-camera / zwo-focuser backends, whose real paths
  `--all-features` cfgs out behind mock). Attributes only; census
  1,374 → 1,355 with the complement re-measure at zero for both lints.
- **B4 — structural pedantic, by lint (329 sites, six slices a–f).**
  Slicing decided 2026-08-16: mechanical first, judgment density rising —
  a `manual_let_else` 71, b small-tail structural (`match_same_arms` 23 —
  keep explicit exhaustiveness where collapsing hurts,
  `items_after_statements` 15, `struct_excessive_bools` 8 — decided
  per-struct with Igor, `struct_field_names` 3, `implicit_hasher` 3,
  `ref_option` 2, `option_option` 2 — replaced by a `Patch<T>` tri-state
  enum for rp's absent-vs-null overrides per the same decision round,
  `fn_params_excessive_bools` 1), c `needless_pass_by_value` 53 (internal
  signature changes), d `option_if_let_else` 96 (nursery — apply where it
  reads better, `#[expect]` its known borrowck false positives), e naming
  (`similar_names` 15 + `many_single_char_names` 3 — math code may take a
  `clippy.toml` threshold or a reasoned `#[expect]`), f `too_many_lines`
  35 per the decision above.
  - **B4a — `manual_let_else` (71) — DONE (2026-08-16).** All 71 sites
    rewrote to `let … else`; no `#[expect]`s. Half the count was two
    clone families: rp's ten per-device-kind equipment connect helpers
    and twelve copies of the planner's `site`-required guard. Clippy
    correctly skips the divergent unwraps whose binding needs a type
    annotation (`let bytes: &[u8; 6] = …try_into()` in the GTi mock),
    so those stay. Rode along: the two drift sites the doctor probe fix
    (#1001) landed after the B3 census (`format_push_string` in
    `aggregate.rs`, and its test's socket-hold `Vec` —
    `collection_is_never_read` — replaced by a single held accept).
    Census 1,356 → 1,284 (the drift made the baseline 1,356),
    all-targets == prod + 0 test-side, no new sites.
  - **B4b — small-tail structural (57 sites + 2 drift) — DONE
    (2026-08-16).** The eight `struct_excessive_bools` restructures, all
    typed homes and zero `#[expect]`s, per the per-struct decisions:
    `AxisStatus` now mirrors the `:f` wire nibbles (`ModeKind` /
    `Direction` / `Speed` enums reused from the command side, plus
    `MotionFlags` and `InitFlags` two-bool groups) and the GTi mock's
    `AxisSimState` adopts the same enums; `FalconStatus` splits into
    `FalconMotion` (live `FA` state) + `FalconSettings` (persisted
    `DR`/`FN` state), reused by its mock; `PpbaStatus` grows a
    `PpbaSwitches` group (`P1`/`P2`/`PD` echoes), reused by its mock;
    svbony's `SensorInfo` groups its `Capabilities`; the GTi
    `DriverState` pair becomes `PulseGuiding { ra, dec }`. The two
    `option_option` sites became the `Patch<T>` tri-state
    (`Keep`/`Clear`/`Set`) with one generic deserializer replacing both
    `double_option` copies. `struct_field_names`: the sky-survey
    position wire structs renamed to `PositionDegrees{Response,Request}`
    with plain `ra`/`dec`/`rotation` keys (unit in the type name —
    breaking wire change decided with Igor; harness, steps and design
    doc updated); the fwhm `StampFitter` dropped its `pixel_` prefix.
    `match_same_arms` 23 collapsed into or-patterns (comments merged);
    `items_after_statements` 15 hoisted to fn tops; `implicit_hasher` 3
    took `BuildHasher` generics; `ref_option` 2 became `Option<&T>`;
    ui-htmx's four-bool `field_hints` became a `FieldGuard` enum (the
    four flags were one mutually-exclusive priority chain). Rode along:
    the #1004 drift's two `doc_markdown` sites (its two
    `option_if_let_else` sites stay for B4d). The re-measure caught four
    sites my own fixes created (a derivable `Default`, a missing `Eq`,
    a `map_or`-shaped match, an overlong first doc paragraph) — fixed;
    census 1,287 → 1,229 (#1004 drift had made the baseline 1,287),
    all-targets == prod + 0 test-side.
  - **B4c — `needless_pass_by_value` (53) — DONE (2026-08-16).** Internal
    signature changes only, zero `#[expect]`s, no wire or behavior
    change. The families: the five service managers' `new(config:
    Config, …)` became `&Config` (every test call site drops its
    `cfg.clone()` — ~60 in the GTi suite alone), and the sync
    `build_hooks` helpers take `&Arc<…>` and clone inside (the async
    `handshake`/`poll_loop` fns keep owned `Arc`s — the lint skips
    async fns, whose params live in the returned future); rp's twelve
    imaging fns (`sigma_clipped_stats` → `detect_stars` →
    `measure_*`, plus the three mcp `*_outcome` wrappers) take
    `view: &ArrayView2<T>` — the view is provably `Copy` (the
    pipeline re-passes it from `FnMut` closures), clippy just cannot
    see it through ndarray's impl, and a reference is equivalent;
    error translators (`apply_error_to_ascom`,
    `serialization_error`, `translate_write_err`, sentinel's
    `describe`, config's `ownership_error`) borrow and the point-free
    `.map_err(f)` sites became `.map_err(|e| f(&e))`; message/report
    helpers take `&str`/`&[T]` (session-runner's `error_response` /
    `issues_response` / `validate_report`, rp's `not_found`);
    `credential_check`'s `auth_pointer` and the synthetic-FITS `wcs`
    became `Option<&T>`; the maud `layout` pair takes `&Markup`
    (maud 0.27's `Render for &T` keeps `(body)` unescaped); rp's
    `ImageCache::insert` takes `&str` (callers drop their clones);
    planetarium's `Spool::append` takes `&str` (one allocation
    saved). The re-measure caught two sites the fixes themselves
    created — `apply_status` became const-eligible once its
    `String`-carrying by-value param turned into a borrow (made
    `const fn`), and an `insert` reorder tripped
    `significant_drop_tightening` (reverted to the original
    statement order under the `&str` signature) — both cleared.
    Census 1,229 → 1,176 (exactly −53), all-targets == prod +
    0 test-side.
  - **B4d — `option_if_let_else` (98) — DONE (2026-08-17).** All 98
    sites (96 planned + 2 drift from B4b's window) cleared with zero
    `#[expect]`s. The judgment rule applied: the rewrite must read at
    least as well as the match, and a different shape wins where one
    exists. The families: the twelve camera ROI setters (qhy/zwo/svbony
    ×4) became `let area = (*roi).ok_or(INVALID_VALUE)?` on the copied
    `Option` under the guard — no closure, and zwo's
    `is_pulse_guiding` deref-copy also cleared a
    `significant_drop_in_scrutinee` site (the old `match` held the
    guard as scrutinee); the five mock `recv_frame`s fused into
    `pop_front().ok_or(Eof)?`; session-runner's eight
    absent-key-is-valid fields share a new `optional()` walker whose
    `OptionalField<T>` enum (Igor's pick over an `#[expect]`) names
    `Absent`/`Present` while the walk-abort stays the outer `Option`'s
    `None` — the file's own abort idiom — because a three-variant enum
    would re-create `Option<Option<T>>` at its conversion signature;
    dsd-fp2's three parse-setters share `set_parsed()`; bool positions
    took `is_some_and`/`is_ok_and`/`is_none_or` (writing
    `map_or(false, …)` would fire default-set `unnecessary_map_or`);
    identity arms became `unwrap_or`/`unwrap_or_else`; Some→Some/None→None
    chains became `map`/`and_then`; both `trim_trailing_slash` copies
    (rp-guider, rp-plate-solver) were the lint's classic borrowck
    false positive (`map_or(s, …)` moves `s` while `strip_suffix`
    borrows it) and were restructured to `ends_with` + `pop`, which
    also drops the reallocation; bdd-infra's guider-scoping assert
    hoisted above the mount-block `map_or`; omnisim's nested
    diagnostic match became a double let-else helper
    (`startup_log_evidence`). The re-measure caught two sites the
    fixes created — `to_icrs` looked pure once its `warn!` moved into
    a closure (`must_use_candidate`; attribute added) and the first
    `optional()` draft returned `Option<Option<T>>`
    (`option_option`; resolved by the enum above) — both cleared.
    Census 1,181 → 1,082 (the −99 = 98 + the drop-in-scrutinee
    bonus; #1008 drift had made the baseline 1,181, its
    `missing_errors_doc` ×2 / `significant_drop_tightening` ×2 /
    `too_many_lines` ×1 stay for B9/B7/B4f, and its one test-side
    `duration_suboptimal_units` rode along here), all-targets == prod
    + 0 test-side.
  - **B4e — naming (`similar_names` 15 + `many_single_char_names` 3) —
    DONE (2026-08-17).** All 18 sites cleared with zero `#[expect]`s and
    no clippy.toml threshold. The lint earned its keep three times over:
    `side`/`site` in both planner meridian helpers (→ `pier_side`) and
    `last`/`lst` in the planner decision scan (→ `last_filter`) were
    genuine confusability hazards, as was ppba's `stats`/`state`
    (→ `power_stats`). The mechanical rule discovered empirically (and
    matching clippy's implementation): a pair differing in one char is
    exempt only when that char is its own final snake-case word — so
    `pixels_x`/`pixels_y` never fired while `sum_wx`/`sum_wy` did.
    Renames rode that rule where the axis-last spelling also reads
    better: `weighted_sum_x/y` (stars centroid), `denom_x/y` (fwhm
    Gaussian denominators), `arcsec_per_pixel_x/y` (sky-survey plate
    scale), `binned_sensor_width/height`; `target_ha` → `target_hour_angle`
    (vs the `target_ra` param), `pptr` → `poll_ptr`. The two structural
    escapes: rp's connect-time camera-metadata lets folded into the
    `CameraEntry` literal (log reads the entry's fields; `device: Some(cam)`
    moved last so the metadata awaits still borrow `cam`), and
    `do_capture`'s four cached optics reads became one `cached_optics`
    tuple — the shape the consuming match already had. The
    `pixel_size_x_um`/`pixel_size_y_um` pair in
    `Optics::from_camera_geometry` is crate-wide vocabulary with no
    better name, so per Igor's pick the five scalars became a
    `CameraGeometry` struct (fields don't fire the lint; the body uses
    field access because destructuring would re-create the bindings).
    `many_single_char_names`: `encode_u24`'s six hex bytes became
    `l0,l1,m0,m1,h0,h1` (byte origin + position, and ≥2-char names are
    exempt); Rodrigues' `(s, c)` became `(sin_a, cos_a)` leaving
    `t,x,y,z` at exactly the threshold with the matrix notation intact;
    `value_eq` adopted `left`/`right` + `l`/`r`-prefixed pairs.
    Census 1,082 → 1,064 (exactly −18, no drift in the baseline, no
    self-inflicted sites), all-targets == prod + 0 test-side.
  - **B4f — `too_many_lines` (36: the 35 planned + svbony's
    `open_handshake` drift) — DONE (2026-08-17), zero sites remain.**
    Per-site judgment in two rounds. Round one: 31 sites had a genuine
    seam and were split — builders into named constructors (bdd-infra
    `RpConfigBuilder::build`, rp `ServerBuilder::build`, gti
    `ServerBuilder::build`), wire dispatch along the protocol's own
    taxonomy (mock_phd2 canned-vs-stateful, gti transport mock
    inquiries-vs-setters), pipelines into phases (polar-align
    `run_inner`, rp `do_capture` / `refocus_train_inner`, sentinel
    `health::run`), validators per concern (rp trains, session-runner
    validate/catalog, ui-htmx forms), the Pratt parser per grammar
    production, and sentinel's dashboard scaffold into a `const` filled
    via `replace()` (un-doubling the JS braces `format!` had forced).
    Small structs (`SweepOutcome`, `CaptureSnapshot`, `OutageCounters`,
    `SessionStack`, …) thread phase state where tuples would blur it.
    Census 1,064 → 1,031 (−31 `too_many_lines` and a bonus −1
    `missing_panics_doc` from moving bdd-infra's builder assert into a
    private helper; plus the sidecar chore PR's −1 doc site in
    between), all-targets == prod + 0 test-side, no new sites.
    Round two resolved the held-out 5 per Igor's per-site calls, and
    only one survived as an `#[expect]`: ppba `switches::info` (one
    `SwitchInfo` row per physical switch — pure data, and `from_id` /
    `MAX_SWITCH` lean on it staying the single table; the `#[expect]`
    is fulfilled even pre-flip, since the `Expect` lint level runs the
    pass regardless of the group being allow-by-default). The other
    four split after all: rp `cooldown_pass` grew a `CooldownPhase`
    sampling struct (pure verdict methods; every device command and
    the shared `cooler_off_and_clear` exit stay in the controller);
    session-runner `lex` grouped its operator arms by *shape* into
    family helpers (`arith_op`, `eq_op`, `cmp_op`, `logic_op`, …,
    each owning its teaching messages as data — chosen over a
    match-generating macro DSL, since macros cannot expand in
    match-arm position and the helpers keep fmt/clippy native); gti
    `slew_completion_step` became `SlewWatchCtx::step` →
    `try_pickup` → `finish_slew` + a pure `pickup_deltas`, with
    `SlewWatchCtx`/`PickupState` also deleting the slew path's two
    `#[allow(clippy::too_many_arguments)]` (the shared
    `run_completion_watcher` keeps its allow: its params include the
    per-operation closures); ui-htmx `card_text` split into five
    domain-family catalog functions (mount/equipment/imaging/guiding/
    session) chained by `or_else`, chosen because the event catalog
    only grows. Census 1,031 → 1,025: −5 `too_many_lines` (now **0**
    in scope) and −1 sentinel `significant_drop_tightening` that the
    dashboard skip-and-log fix had already cleared after the previous
    census was taken; all-targets == prod + 0 test-side, no new
    sites. Behavior notes: the gti dwell anchor (`started`) is seeded
    by the caller at slew-issue time but re-anchored by the spawner
    right after its session acquire (a round-2 review catch: the
    acquire can block behind a concurrent teardown holding the
    transport lock, which would have eaten into the client-visible
    2 s `Slewing == true` floor — the re-anchor keeps the original
    reference point exactly), and a NaN-safe `over_tolerance`
    spelling preserves the original fall-through-to-completion on
    unordered residuals.
- **B5 — `missing_const_for_fn` (18, nursery) — DONE (2026-08-18), zero
  sites remain.** Every site took `const` as-is; the L2 collision (`const
  fn` calling `From` does not compile) never bit because no body in the
  set calls `From`. Half the set (9) was the `default_server()`
  serde-default helper each service repeats — `ServerConfig::new` is
  already const, so the helpers follow, matching the `default_true` /
  `default_stop_timeout` idiom already in those files. The rest are small
  pure helpers whose callees const-stabilized out from under them:
  `Pixels::len` (`Vec::len`, 1.87), the gti `sat_round_*` pair
  (`f64::round`, 1.90), plus bit/cast bodies (`to_wire_bytes`,
  `nibble_to_hex`, `as_real`, `as_i64`, `stretch::range`,
  `Payload::text`). One cascade, per the B4c `apply_status` precedent:
  `Pixels::is_empty` became const-eligible the moment `len` went const —
  the re-measure caught it and it went const too. Census 1,025 → 1,007
  (net −18: 19 fns made const, but the cascade site was never in the
  baseline), all-targets == prod + 0 test-side, no new sites.
- **B6 — the cast quartet (44) — DONE (2026-08-18), zero sites remain.**
  Every site sat inside an existing L5 `as_conversions` ledger entry — no
  genuinely new site existed, and the `cast_possible_wrap` singleton from
  the planning census no longer fires at all. The fix is pure widening: 23
  `#[expect]` blocks across 19 files each gained exactly the quartet lints
  that fire in their scope (`cast_possible_truncation` 26,
  `cast_sign_loss` 11, `cast_precision_loss` 7) — exactly, because a named
  lint that does not fire raises `unfulfilled_lint_expectations`, which
  keeps the ledger self-verifying under `-D warnings`. No reason text
  changed: the L5 entries were written as bound proofs (range checks,
  saturation semantics, the 2^53 exactness argument), which is precisely
  the claim the cast lints audit. Census 1,007 → 963 (−44, the quartet
  and nothing else), all-targets == prod, 0 test-side (the curated list
  already names the cast lints), no new sites, no unfulfilled
  expectations.
- **B7 — the lock-scope pair, per-crate (~131).**
  `significant_drop_tightening` 123 (19 crates; the count wobbles a few
  sites as surrounding code changes — re-census at slice start) +
  `significant_drop_in_scrutinee` 8. Each site reasons about lock ordering
  and atomicity; a lock deliberately held across an await gets a reasoned
  `#[expect]`, not a rewrite. Explicit tenet-3 check on any connect or
  supervisory path touched. Most likely slice to genuinely improve tenet-2
  robustness, and the most able to break it — hence late.

  Slice-start census (2026-08-19) confirmed 131 sites (123 + 8), all
  prod-side. Triage split them three ways, with the `#[expect]` policy
  settled per family: sites whose guard's true last use precedes real
  tail work get an explicit `drop()` there (compiles, and several are
  genuine wins — doctor's ACME account mutex was held across the whole
  multi-minute order flow; filemonitor held its connected-state write
  lock across `start_polling`); sites where the suggested early drop
  cannot compile because the derived reference borrows the guard to the
  final expression (the lint does not track reborrows) get either a
  shared `with_session`/`with_camera` helper carrying one reasoned
  `#[expect]` (the pa-falcon-rotator pattern) or a per-site `#[expect]`;
  and sites where the hold is load-bearing (connect check-and-modify
  spans, refcount+flag pairing, the event bus's exactly-once
  replay/live handoff, state-file lockstep) keep the hold under a
  per-fn `#[expect]` naming the invariant. Landed per crate group:
  - **B7a — rp, sentinel, shared-transport, rp-targets, filemonitor,
    doctor, polar-align (25 → 0, DONE 2026-08-19).** 14 mechanical
    drops/hoists, 2 rewrites that remove the named guard (sentinel
    watchdog target resolution, shared-transport's reconnect cell
    clone), 9 expects (rp events ×2, session ×1, cache ×1, cooling ×4,
    polar-align ×1). Census 963 → 938, only the drop pair moved.
  - **B7b — the serial drivers: pa-falcon-rotator, ppba-driver,
    qhy-focuser, pa-scops-oag, dsd-fp2 (29 → 0, DONE 2026-08-20).**
    qhy-focuser and pa-scops-oag gain pa-falcon-rotator's
    `with_session` helper (one reasoned `#[expect]` each) and route
    is_moving/halt/move_ through it; pa-falcon-rotator's two existing
    helpers carry the same expect. 14 mechanical drops at true last
    use — in dsd-fp2 the session read guard now releases before each
    cached-snapshot write, un-nesting the two locks. Uniform per-fn
    expects on the six connect check-and-modify spans and dsd-fp2's
    mock responder. Census 938 → 909.
  - **B7c — star-adventurer-gti + phd2-guider (18 → 0, DONE
    2026-08-20).** gti gains a `with_session` helper generic over the
    caller's error type (one reasoned `#[expect]`; `send` and five
    session-guard sites route through it — the tracking enable, both
    fresh-wire `poll_axes_now` snapshots, the encoder reset, and
    `stop_and_wait`). The slew motion block keeps its shape (it
    interleaves `stop_and_wait`, which takes its own read guard) and
    gets a tail `drop(guard)` instead. 6 mechanical drops in gti state
    writers, 4 in phd2 (event-state writers and both process-lifecycle
    fns — the kill-serialization hold is preserved, the drop lands
    after the reap). One connect expect on gti's `set_connected`.
    Census 909 → 891.
  - **B7d — svbony-camera + zwo-camera (40 → 0, DONE 2026-08-20).**
    Both backends gain a `with_camera` helper (one reasoned
    `#[expect]` each); the SDK accessor fns (11 in svbony, 7 in zwo)
    and the multi-statement capture-configure blocks route through it,
    collapsing the guard-open-check boilerplate. Mechanical drops on
    the open/abort/download tails (svbony's `open` keeps the
    slot-write → `open`-flag store pairing under the lock; the drop
    lands after both), the intended-ROI quartets in both devices, and
    5 scrutinee hoists (cancel-token take, target-temperature reads,
    stored-error reads). Census 891 → 851.
  - **B7e — qhy-camera + zwo-focuser + sky-survey-camera (19 → 0,
    DONE 2026-08-20; closes the rung).** zwo-focuser gains the
    `with_focuser` helper (one reasoned `#[expect]`, seven accessors
    routed). qhy-camera's backend connect/disconnect refcount pair
    keeps its documented refcount+flag pairing under two per-fn
    expects; its ROI quartet and both scrutinee sites take the
    mechanical drop/hoist. sky-survey-camera: stored-error hoist,
    image-array guard drop after the owned array is built, and the
    pointing-override drop before the response. Census 851 → 832;
    **the drop pair reads zero in scope** — the remaining sites live
    in the census-excluded FFI crates (qhyccd-rs ×20, zwo-rs ×9,
    svbony-rs ×4), which are L7's separately-decided scope.
- **B8 — flops-hard (56 → 0, DONE 2026-08-21).** The analysis-math
  residue: 56 production `suboptimal_flops` sites, every one a `mul_add`
  suggestion. One real fix — `fit_2d_gaussian`'s eccentricity now uses the
  factored `(1 − r)(1 + r)`, which keeps the tiny `1 − r²` of a
  near-circular star from cancelling away. The other 55 are **standing
  exemptions, recorded here like `exit` was in L4**: 26 function-level
  reasoned `#[expect]`s in three families, approved as a slice.
  - *Reference-formula fidelity* — the haversine pair (rp-catalog,
    `center_on_target`), polar-align's `dot`/`cross`/`mul_vec`/
    `determinant`/Rodrigues/CD-matrix functions, the ERFA-mirroring
    trio in rp-ephemeris, and gti's altitude formula. The naive
    symmetric form *is* the documented algorithm; for `cross`
    specifically, `mul_add` breaks the exact cancellation of `v × v = 0`
    on the near-parallel inputs axis extraction feeds in.
  - *Hot-path platform reality* — `scaled_to_i32` (rp-fits) and the
    mpfit `StampFitter::eval` loop are per-pixel paths; `mul_add`
    lowers to a *software* fma call on targets built without hardware
    FMA (baseline x86-64: CI, dev boxes, the Windows MSIs), so the
    "more efficiently" claim only holds on the aarch64 rig. A rewrite
    that pessimizes half the deployment targets is not an optimization.
  - *Trivial sums with unit semantics* — seconds + ns·1e-9 (×3),
    `ra·15 + offset/3600`, `zone + 24k`, sweep progressions, and the
    `mean ± k·σ` family, where the plain form documents the units and
    the accuracy stake is nil.
  The in-scope flops census reads zero; the remaining four sites are
  qhyccd-rs's (census-excluded FFI, L7's scope).
- **B9 — the doc sub-rung (702 in scope at the post-B8 re-census:
  `missing_errors_doc` 476, `too_long_first_doc_paragraph` 259,
  `doc_markdown` 40, `missing_panics_doc` 29, the doc-link pair 2; the
  ~104 FFI-crate sites are L7's).** Real docs at the accurate-summary
  bar — an `# Errors` section names the failure classes the body
  actually returns, 1–2 sentences, no variant catalogs; long first
  paragraphs split into a summary line plus detail. Seven docs-only
  sub-slices, batched per crate cluster:
  - **B9a — infra + small `rp-*` crates (85 → 0, DONE 2026-08-21).**
    rusty-photon-{tls, config, shared-transport, camera-core,
    server-config, driver, service-lifecycle} and rp-{auth, catalog,
    ephemeris, fits, guider, mcp-client, plate-solver, targets}.
    Two shapes worth recording: `default_config_dir` got an `# Errors`
    section on **both** cfg variants — the Windows arm is invisible to
    a Linux census but real to the post-flip `windows / clippy` leg —
    and adding a section to a >200-char single-paragraph doc makes
    `too_long_first_doc_paragraph` fire where it previously did not
    (single-paragraph docs are exempt; the re-measure caught it).
  - **B9b — rp (105 → 0, DONE 2026-08-21).** 62 first-paragraph splits
    and 43 `# Errors` sections across 44 files, every claim checked
    against the body in both directions. Three shapes recur: a
    `Result` that never fails in practice (`SessionManager::stop`)
    gets an `# Errors` section saying so plus why the `Result` exists
    (route-handler parity with `start`); an aggregated surface
    (`ServerBuilder::build`) is summarized by error *class* — `Config`
    for every config-derived piece, `SiteMismatch`, `Io` for the bind
    — with the explicit non-error named (equipment that fails to
    connect is recorded, not returned); and docs that already
    described their failures in prose (`EventBus::from_config`,
    `SessionManager::new`, `TrainModel::try_from_equipment`) are
    restructured under the heading rather than reworded — the lint
    wants the heading. Rustdoc's pre-existing `private_intra_doc_links`
    and four unresolved links in rp predate this rung and show
    `cargo doc -D warnings` is not a gate; none are on touched lines.
  - **B9c — bdd-infra (61 → 0, DONE 2026-08-22).** 28 `# Errors`, 23
    `# Panics`, and 10 first-paragraph splits across 14 files. The first
    slice where `# Panics` dominates: bdd-infra's `src/` is prod scope
    (L5v) but its contract is test infrastructure, so a panic *is* the
    failure mode — every stub `start`, fixture `generate`, and `assert_*`
    names what takes the scenario down. Two shapes recur: the
    process-spawning helpers (`ServiceHandle::start_with_env`,
    `spawn_service_handle`, `start_sky_survey_camera`) name the binary
    discovery / spawn / pipe-capture / bound-port steps and say the plain
    path has no deadline; the `OmniSim` HTTP helpers summarize by the same
    four classes (request cannot be built or sent, non-success status,
    non-Alpaca body, non-zero `ErrorNumber`) rather than per call. One
    pre-existing inaccuracy corrected in passing: `ServiceHandle::try_start`
    documented a 10 s bind deadline against a 30 s body.
  - **B9d — doctor + doctor-checks (73 → 0, DONE 2026-08-23).** 24
    `# Errors` and 49 first-paragraph splits across 21 files, every claim
    checked against the body in both directions. The provisioning modules
    are the substance: the `rusty_photon_tls` surface is summarized by
    `TlsError` variant (`Io` for the pki writes, `Other` for a symlinked
    target, `CertGen` for rcgen, `Config` for a validity overflow or a
    malformed ACME domain, `DnsProvider` for the Cloudflare leg), and the
    `String`-error orchestration (`ensure_material`, `run_acme`, `renew`)
    names its steps and what survives a failure — material already
    written stays on disk, a saved `acme.json` is renewal's recovery
    input, a `RenewError` carries what was renewed before the failing
    step. Two non-errors are stated explicitly: `save_acme_config`'s
    serialization step cannot fail for its shape, and
    `diagnose_and_fix`'s diagnosis outcome is never an `Err`. One site
    the Linux census cannot see was documented alongside its twin: the
    `#[cfg(not(unix))]` `align_pki_ownership_with_warnings`, which the
    off-PR Windows clippy leg would otherwise flag after the flip.
  - **B9e — cameras and focusers (111 → 0, DONE 2026-08-23).** 94
    `# Errors`, 16 first-paragraph splits, and one `doc_markdown` backtick
    across 17 files. The substance is the four SDK-seam traits (`qhy-camera`,
    `svbony-camera`, `zwo-camera`, `zwo-focuser`): every method's contract
    names the not-open refusal and the SDK failure it wraps, plus the
    specifics a caller acts on — `get_single_frame`'s buffer-too-small check,
    `set_transfer_bit_16` failing on a model without the control, the
    `svbony-camera` `Gain` write the SDK refuses while its auto-exposure
    state is on, each `capture` composite's full failure list (including
    which side of a mid-capture disconnect reports an error and which
    `Ok(None)`), and the `close` methods that cannot fail because the close
    is a drop. The `qhy-focuser` protocol parsers name the field and type
    each rejects; both `build`/`start` pairs name their SDK-or-transport,
    bind, and serve failure classes.
  - **B9f — mounts, rotators, and serial drivers (127 → 0, DONE 2026-08-23).**
    89 `# Errors`, 36 first-paragraph splits, one quoted doc link, and one
    code-adjacent link across 39 files. The substance is the wire seams:
    `skywatcher-motor-protocol`'s codec and decoders name the framing, hex,
    payload, and mount-error classes each rejects (and `encode_into`'s
    partial prefix left behind on error); the three `SharedTransport`
    drivers' manager methods name the session failure — transport, codec,
    skip budget — plus the non-echo / non-status reply rejection and what
    happens to the cached state; the `star-adventurer-gti` `send` /
    `poll_axes_now` pair names the pre-wire tick validation, the codec's
    wrong-device error, and the `!` error reply, and the config newtypes'
    `try_new`s name their ranges; `polar-align`'s geometry names each
    degeneracy (collinear points, pole-centred solves, singular CD matrices,
    parity flips, sub-1° segments), the MCP client names the tool each call
    fails on, and `workflow::run` lists the refusal classes behind
    `PolarAlignError::Workflow`. The four `build`/`start` pairs reuse the
    B9e transport-bind-serve wording.
  - **B9g — the remaining services (140 → 0, DONE 2026-08-23).** 79
    `# Errors` and 61 first-paragraph splits across 62 files
    (ui-htmx, sky-survey-camera, session-runner, sentinel,
    calibrator-flats, plate-solver, filemonitor, planetarium-bridge) —
    the last doc slice. Recurring shapes: the two MCP-client wrappers
    (calibrator-flats, session-runner) name the tool each call fails on
    and summarize by the `rp-mcp-client` classes (request cannot be
    sent, rp reports a tool error, reply shape); the seven byte-identical
    per-service `doctor` module docs take one shared split; the
    `build`/`start` pairs reuse the B9e bind-serve wording. The
    substance is the failure contracts: session-runner's blackboard
    (which bookkeeping heals, which failures fail loud because resume
    depends on them, and the in-memory updates that cannot fail),
    sentinel's two explicit non-errors (`SentinelBuilder::build` never
    errors — a dashboard bind failure degrades to running without the
    dashboard; `RestartManager::restart` reports a failed platform
    restart inside its `Ok` report), plate-solver's supervision
    (deadline expiry is a `SpawnOutcome`, not an error; signal-delivery
    failures are logged, never returned), and sky-survey-camera's
    follow-mode read seams (mount/rotator failures surface per F2/F8,
    the `Static` arm never errors). One `doc_markdown` hit was
    self-inflicted (an unbackticked `SkyView` in a new summary line) —
    the re-measure caught it.
  - **B9h — the crates the slice partition missed (78 → 0, DONE
    2026-08-24).** The fresh full-workspace census taken as B10's gate
    evidence read 78 residual doc sites in three crates no slice had
    claimed — phd2-guider (60), dsd-fp2 (15), rusty-photon-i18n (3):
    every per-slice re-measure ran per-crate (`-p`), so each slice saw
    only the crates it named, and only a full-workspace census could
    catch the dropped ones. 71 `# Errors` and 7 splits at the same bar:
    the phd2-guider client's 38 JSON-RPC wrappers share the uniform
    failure classes (not connected, send/timeout/drop, PHD2 rejection,
    reply decode) plus two honest non-errors (`disconnect` and
    `stop_phd2` never fail today); the service layer maps client
    failures onto the frozen wire codes per method; dsd-fp2's protocol
    parsers name what makes a body malformed; the two missed `doctor`
    module docs take the shared split verbatim. Also fixed here: the
    doctor `gather_facts` `unnecessary_wraps` — the one non-doc
    residual, visible only on default features (the #988 complement no
    `--all-features` re-measure could see); the cfg split moved to the
    call site and the always-`Ok` wrapper is gone. B9 truly closed:
    B9a–B9h, 781 sites.
- **B10 — the flip (DONE 2026-08-24).** `pedantic = { level = "deny",
  priority = -1 }` and `nursery = { level = "deny", priority = -1 }` join
  the workspace table. Gate evidence, three layers:
  - **Linux three-pass census read zero** (all-targets/all-features,
    lib/bins all-features, lib/bins default features) — after B9h closed
    the last doc residue.
  - **Windows, mechanically where the cross-build allows**: a full
    `--workspace` msvc cross-clippy dies in `aws-lc-sys`'s C build, but a
    per-crate sweep cross-checks 16 in-scope crates (everything outside
    the TLS dependency cone; bdd-infra and rp-auth on default features).
    It found 9 OS-gated sites the Linux census could never see — two
    `doc_markdown` docs, the scm mod's wildcard import, a
    `significant_drop_in_scrutinee` in the SCM error hand-off, and the
    two `cfg(not(unix))` atomic-save stubs (`unnecessary_wraps` +
    `missing_const_for_fn`). All fixed in the flip PR; the stubs keep
    their signatures for cfg parity and carry `#[expect]` with the
    reason.
  - **Hand audit of the remaining OS-cfg surface** (the TLS-cone crates
    the cross-build cannot reach): every `cfg(windows)` /
    `cfg(target_os = "macos")` region read against the high-yield
    pedantic/nursery classes. Six findings, all fixed here: the windows
    DLL preflight's missing `# Errors`/split and `#[must_use]`
    (qhy-camera), the SCM service-manager doc split (sentinel), an
    ungrouped hex literal (plate-solver), a macOS-only
    `option_if_let_else` that Linux compiles only in test scope where
    the curated list hides it (rp), and `const`/`#[must_use]` parity on
    doctor's `cfg(not(unix))` provision stand-ins.

  The flip itself then caught what every `-W` census had missed: the
  feature-gated `pub mod fixtures;` / `pub mod sse_fixtures;` declaration
  docs in ui-htmx (>200-char single paragraphs, compiled only under
  `--all-features`). Their diagnostics carry a broken mod-declaration
  span and render **span-less**, so `--message-format=short` prints them
  with no `file:line:` prefix — a grep-based census filters them out
  silently. Split here; the lesson is that the deny flip is the only
  census that cannot lose a diagnostic.

  The standing toolchain consequence arrived the same day: CI updated
  stable to 1.98 between the local gate and the PR run, and 1.98 adds
  `unused_async_trait_impl` to pedantic — five sites, four of them test
  doubles (async trait impls with no awaits), one the `rmcp::tool_handler`
  expansion in rp. Absorbed here per the policy: the curated test-scope
  list grew by the new name (all carriers together), and the macro site
  carries `#[expect]` with the expansion reason.

  Standing gap, closed just after the flip: the five
  `#[cfg(not(any(unix, windows)))]` fallback arms were compiled by no CI
  leg and stayed unlinted, and a hypothetical third-family port would
  have silently gotten degraded behavior (a shutdown watcher that never
  fires). They are now `compile_error!` arms asking the porter to open a
  GitHub issue naming the platform — no unlinted first-party code
  remains, and an unsupported target fails loudly at compile time. The off-PR
  `windows / clippy` + `macos / clippy` legs are the ground truth for the
  OS-cfg surface and can go red on main when the set widens — the first
  post-merge run is watched. Updated here: `docs/workspace.md` § Lints,
  the `Cargo.toml` comment block, this table.

  That first post-merge run (2026-08-24): macos green; windows red on one
  residual site — a `redundant_closure_for_method_calls` in the qhy-camera
  DLL preflight. The crate sits in the TLS cone the per-crate msvc sweep
  cannot compile, so the hand audit was its only pre-flip net. Fixed in the
  immediate follow-up; verified by a manual `workflow_dispatch` of the same
  workflow on the fix branch — the only way to run the OS legs pre-merge
  (they skip PRs), and the first msvc clippy over the crate's bin and
  tests, which the red run never reached past the lib error.

  That dispatch then surfaced round two: `unused_self` +
  `missing_const_for_fn` on a ui-htmx BDD helper whose body is entirely
  `#[cfg(unix)]` — on Windows the interior compiles out and the
  *enclosing*, un-cfg'd function lints differently. This is a search
  class the hand audit never ran: it audited `cfg(windows)` regions, but
  the hazard lives in unix-gated interiors changing how surrounding code
  lints on the OS legs. Fixed with a `cfg_attr(windows, expect(...))`
  carrying the cfg-parity reason, after a workspace sweep for the whole
  class; ui-htmx, like qhy-camera, is msvc-blocked locally (TLS cone),
  so the dispatch legs are the only verifier.

### Standing consequences

- Every stable toolchain bump can add newly-denied pedantic/nursery lints;
  the L6a beta census is the ~6-week early warning, and toolchain-bump PRs
  absorb new sites. Renamed or removed nursery lints surface via
  `renamed_and_removed_lints` under `-D warnings`.
- The loop fired for real on beta 1.99 (nightly 2026-08-24): three new
  findings — `assert_is_empty` ×135 (#1069), widened `suboptimal_flops`
  detection ×4 (#1070), `branches_sharing_code` on match arms ×1 (#1071)
  — absorbed ahead of the bump (#1077 + the flops/branches PR). Two
  lessons: a new lint's machine suggestion can itself violate the deny
  set (`[] as [T; 0]` trips `as_conversions`; empties must be cast-free),
  and a site only beta detects cannot take `#[expect]` yet — the
  expectation is unfulfilled on stable, which `-D warnings` rejects — so
  the two lerp-shaped flops sites wait for the 1.99 bump PR to land
  their approved expects.
- The curated test-scope list is a maintenance surface: a new lint that
  fires on cucumber patterns makes the bump PR either fix the test code or
  grow the list — a deliberate trade, chosen so test code stays enforced on
  everything else in both groups.
- The `#[expect]` ledger grows (flops-hard, lock scopes, math naming); each
  entry carries a reason, and an obsolete one fails the build.

## L7 — dual-homed FFI crates

Adding `[lints] workspace = true` to `qhyccd-rs`, `zwo-rs`, `svbony-rs` and
their `-sys` shims affects what is published to crates.io, not just this repo.
1,038 sites with the knobs applied.

### Mechanism (decided 2026-08-26)

**A concrete `[lints]` table, copied verbatim into each dual-homed manifest,
plus a parity guard in CI** — not `workspace = true`. The facts behind the
choice:

- `cargo package` *does* inline `workspace = true` lints into the published
  manifest (probe-verified on qhyccd-rs: the packaged Cargo.toml carries the
  full concrete table). So inheritance and a verbatim copy publish **identical
  artifacts**; the copy forecloses nothing.
- The real blocker for inheritance is `scripts/verify-publishable-crate.sh`:
  it `cp -R`s the family out of the workspace, where `workspace = true` has
  nothing to resolve against. Teaching it to copy from `cargo package` output
  (which normalizes the whole manifest and would retire its hand-rolled dep
  inliner) is the upgrade path if the per-manifest copies ever chafe.
- The published table is inert for consumers — registry dependencies build
  under `--cap-lints allow` — and pre-1.74 cargo (libqhyccd-sys's 1.68 MSRV
  leg) ignores `[lints]` with an unused-manifest-key warning. Bazel never
  runs clippy, so the cargo surface is the whole story.

The parity guard (lands with the first table): a small tomllib check that
each opted-in dual-homed manifest's `[lints]` table matches the workspace's,
wired next to the other repo-shape checks, so six copies cannot drift.

### Census method

Per family, the census is the **union of two passes** — all-targets
all-features, and lib-only default features — because the `simulation`
feature replaces the real SDK paths: code under `#[cfg(not(feature =
"simulation"))]` is invisible to an all-features pass (99 of zwo's sites
were default-only). The same two configs are the compile gate for every
fix: `--fix` under one config can promote a function whose *other*-config
body cannot satisfy the change (see the interior-cfg hazard below).

### zwo family (first; complete)

Baseline union: **476 sites** — 197 in bindgen's generated `bindings.rs`
(libzwo-sys's crate-root "do not lint generated bindings" allow covered the
groups but not the named restriction lints; widened), 95 in the three
`examples/probe_*.rs` bench diagnostics, 183 in `src/` + `build.rs`, 1 in
test scope (curated-list carrier).

Probe decision (Igor, 2026-08-26): the probes are operator-run bench
instruments — dying loudly without hardware is their intended failure mode.
Mechanical sites fixed (`try_into` for chunk pairs, `u64::try_from` for
durations); each file keeps a documented header allow for `expect_used`,
`arithmetic_side_effects`, and the display-statistics casts
(`as_conversions`, `cast_precision_loss` — counts/sums to `f64` for
printing, with no lossless std path).

**Interior-cfg const hazard** (new `--fix` failure class for dual-config
crates): the all-features fix pass const-promoted nine functions whose
`simulation` body is const-able but whose real body calls the SDK — the
default-features build then fails E0015/E0658. Reverted with a per-fn
`#[allow(clippy::missing_const_for_fn)]`; an `#[expect]` is wrong here
because the config where the fn cannot be const leaves the expectation
unfulfilled, which the default-features `-D warnings` pass rejects.

Slices: **Z1** bindings allow + probe treatment + machine-applicable sweep
(`&raw mut`, safe const promotions, one-offs); **Z2** judgment residue —
`unwrap_used`/`expect_used` in src and build.rs, FFI-boundary casts on the
L5 playbook, `significant_drop_tightening` with tenet-3 eyes on anything
near connect paths; **Z3** the `[lints]` tables in both manifests + parity
guard + docs (`docs/workspace.md` § Lints, root Cargo.toml comment block) +
an OS-cfg cross-clippy census before the deny lands (the `windows / clippy`
leg starts covering these crates on merge). Then the qhy family, then
svbony, each on the same template.

**Z2 (complete): 87 → 0 with no `#[expect]`s** — every judgment site had an
honest total fix. The load-bearing moves:

- `asi_check` now takes `sys::ASI_ERROR_CODE` (c_uint on LP64, c_int on
  MSVC), so all ~20 call sites drop their `} as i32` platform-bridge casts;
  the one alias→i32 narrowing lives inside it as a saturating `try_from`
  feeding `AsiError::from_code` (garbage folds into `Unknown`). `const`
  dropped — `try_from` is not const-stable. Pre-1.0 signature change on the
  published API. `efw_check`/`eaf_check` were already alias-compatible
  (those enums are signed everywhere) and are untouched.
- `ControlType`'s roundtrip crosses the platform-dependent enum width with
  saturating `try_from` in both directions — an invalid id stays invalid
  to the SDK instead of wrapping onto a real control.
- The `c_long` seams (caps min/max/default, control read) use `i64::from`
  — identity on LP64, widening on LLP64. The old comment claiming
  `i64::from` "would not compile on LP64" was wrong (the identity `From`
  impl exists); what IS true is that clippy flags the identity direction:
  `useless_conversion` fires on Linux where the conversion is a no-op while
  being load-bearing on Windows, so those two functions carry an `#[allow]`
  with the cfg-parity comment (an `#[expect]` would sit unfulfilled on the
  Windows clippy leg — the same allow-not-expect rule as the interior-cfg
  const class). The control write saturates via `c_long::try_from`.
- **The identity orientation flips with the alias width, and only the Linux
  orientation is visible locally.** `asi_check`'s original `i32::try_from`
  was a real narrowing on LP64 (`ASI_ERROR_CODE` = c_uint) but an identity
  on MSVC (c_int), so `useless_conversion` fired only on the post-merge
  `windows / clippy` leg (#984 design: the PR clippy gate is ubuntu-only),
  and the slice's local msvc cross-check missed it because it ran
  `cargo check`, which compiles but runs no lints. Fixed allow-free in
  #1091 by widening the seam instead of narrowing it: `AsiError::from_code`
  takes `i64` and `Unknown` stores `i64`, so `asi_check` widens via
  `i64::from` — a real, lossless conversion from either width of the alias.
  The saturating fallback disappears and an out-of-range raw code reaches
  `Unknown` intact instead of folding into `i32::MAX`; `from_code` stays
  `const`. The two camera.rs c_long allows upgraded in the same PR to
  **target-scoped expects**: `cfg_attr(all(unix, target_pointer_width =
  "64"), expect(clippy::useless_conversion, reason = ...))` exists only on
  the configs where the identity fires, so it is fulfilled on every clippy
  leg and goes stale loudly if the orientation ever changes (an
  unconditional expect would fail the Windows leg as unfulfilled; an allow
  reports nothing when it stops matching). `control_value`'s predicate also
  carries `not(feature = "simulation")` — its `i64::from` site lives only
  in the FFI body. Scoping the expect immediately exposed what the broad
  allow had been masking: `control_caps_from_raw`'s
  `i32::try_from(raw.ControlType)` was an identity on MSVC
  (`ASI_CONTROL_TYPE` = c_int there) — a second instance of the class,
  invisible to the Linux census AND silenced on Windows by the very allow
  that was documented as covering the c_long fields. Fixed like
  `asi_check` and per the crate's own alias convention (`BayerPattern`,
  `ExposureStatus`, `ImageType`): `ControlType::from_raw` takes the
  bindgen alias (no conversion at the call site) and `Other` stores `i64`,
  widened losslessly from either alias width. The L7 msvc cross-check is
  therefore **`cargo clippy --target x86_64-pc-windows-msvc`, both config
  shapes** — never `cargo check`.
- All 23 sim-backend state-mutex unwraps take the svbony-rs
  `.lock().unwrap_or_else(PoisonError::into_inner)` pattern; the seven
  `significant_drop_tightening` scopes close with drop-at-last-use (B7);
  `SIM_MAX_STEP`'s expect became a saturating `try_from`; libzwo-sys's
  build.rs is a `Result` main (env vars and bindgen failures propagate as
  build errors instead of panics).

**Z3 (complete): the copies land, plus the guard that keeps them honest.**

- Both zwo manifests carry the full `[workspace.lints]` table as a concrete,
  verbatim copy, under a short comment header pointing at the root table and
  the guard; the policy's rationale stays in the root Cargo.toml block, so
  six copies of it cannot drift apart.
- The guard is `tools/ci/check_lints_parity.py` (stdlib `tomllib`, no
  arguments, repo root derived from its own path). Two rules: an OPTED_IN
  manifest — a roster in the script, the zwo pair today, each family joining
  in the PR that lands its copy — must equal the workspace table verbatim,
  where a missing tool table is drift rather than a smaller opt-in; and any
  other member with a concrete `[lints]` table must mirror the workspace at
  whole-tool granularity. The second rule recognises qhyccd-rs's
  pre-existing `[lints.rust]` `unexpected_cfgs` carrier (declared
  crate-locally so the crate stays publishable standalone before its own
  rung) while still catching a stale entry inside it.
- Wiring deviates from the sketch above deliberately: not parity.yml
  (nightly + push-to-main; its per-PR-risk-is-low argument does not
  transfer, because the drift scenario is exactly a toolchain-bump PR
  widening the workspace table while the copies lag — and nothing else
  fails when a copy goes stale, the crates just lint more loosely). It is
  instead a first step of the required `stable / clippy` PR gate: the job
  that enforces the policy asserts its copies are in lockstep, sub-second,
  before linting.
- Old-cargo surface, settled: pre-1.74 cargo ignores `[lints]` with an
  unused-manifest-key warning, and verify-publishable-crate.sh runs plain
  `cargo +<msrv> check` with no `-D warnings`, so libzwo-sys's 1.70 MSRV
  leg cannot fail on the table.
- Census with the tables live, all clean: native both shapes, msvc all
  three shapes (all-features/all-targets, default/lib, and
  default/all-targets — the cfg'd-test shape), darwin both shapes.
- Docs: `docs/workspace.md` § Lints (concrete-copy mechanism + guard), the
  root Cargo.toml lints block (historical parenthetical corrected + a
  dual-homed footer under the table), `docs/skills/pre-push.md` § check.yml
  (the new step, runnable locally).

### qhy family (second; in progress)

Baseline union (post-#1096 main, §Census method): **404 sites** — pass 1
(all-targets, all-features) 305, pass 2 (lib, default features) 204, with a
99-site default-only complement: `simulation` hides the real-SDK paths
exactly as in zwo. Unlike zwo's pre-Z1 state there is **no bindings.rs
flood** (libqhyccd-sys's generated-bindings allow already held) and no
probe-examples bucket. Top buckets: `as_conversions` 58,
`missing_errors_doc` 50, `expect_used` 48, `borrow_as_ptr` 34,
`missing_const_for_fn` 25 (the interior-cfg const hazard applies — a
sim-body-const fn must not be promoted), `single_match_else` 23,
`significant_drop_tightening` 22 (tenet-3 eyes near connect paths),
`must_use_candidate` 19.

**Z1 (this slice): the machine-applicable sweep, 404 → 289.** Eleven lints
via per-lint `--fix` under BOTH configs with a per-lint re-measure
(`cast_lossless`, `uninlined_format_args`, `ignored_unit_patterns`,
`borrow_as_ptr`, `use_self`, `derive_partial_eq_without_eq`,
`semicolon_if_nothing_returned`, `manual_midpoint`,
`unnecessary_semicolon`, `unreadable_literal`, `map_unwrap_or`); no fix
pass reverted a crate, and every swept lint re-measures zero under both
shapes. clippy's MSRV awareness split the `borrow_as_ptr` shapes correctly
(`&raw mut` in qhyccd-rs at its 1.85 floor; libqhyccd-sys sits at 1.68).
One fixer-skipped test literal took its separators by hand, and the one
`manual_let_else` site (a read-lock match) collapsed to let-else. The
`cast_lossless` rewrites also cleared two thirds of `as_conversions`
(58 → 17) as a side effect. Deferred to Z2 with analysis:
`single_match_else` (23, the L2 redundant-pattern trap class),
`useless_let_if_seq` ×1 in libqhyccd-sys build.rs (`found` accumulates
across three side-effectful search-path blocks; every mechanical rewrite
either breaks the 1.68 floor via `is_some_and`, trades the lint for
`option_if_let_else`, or hides the println in a closure), plus the
expect/unwrap, cast-diagnostic, const-promotion, and drop-tightening
residue and the doc sub-rung.
