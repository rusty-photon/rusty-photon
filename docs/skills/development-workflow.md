# Skill: Development Workflow

## When to Read This

- Before starting a new feature or service
- Before beginning any significant development task (new functionality, major refactor)
- When planning the implementation order for a body of work

## Prerequisites

- Familiarity with the project's documentation taxonomy (see docs/workspace.md)
- Read docs/skills/testing.md for BDD and unit test conventions
- For ASCOM services: read docs/references/ascom-alpaca.md

---

## Procedure

This project follows a **design-first, test-first** workflow. The rp service
pioneered this approach and it is now the standard for all development work.

The workflow has three phases, executed in strict order:

```
Phase 1: Design Doc  -->  Phase 2: BDD Tests  -->  Phase 3: Implementation
```

Do not skip phases. Do not write implementation code before BDD scenarios exist.

---

### Phase 1: Write the Design Document

Write or update the service design document (`docs/services/<service>.md`)
**before** writing any code or tests. The design doc is a specification — it
defines what the system does, not how it is built.

#### What the design doc must cover

1. **Overview** — What the service does in 2-3 sentences
2. **Architecture** — Component diagram, communication flows, dependencies
3. **Behavioral contracts** — For each feature area:
   - What the system does on the happy path
   - What happens on errors (invalid input, unreachable devices, timeouts)
   - State transitions and their triggers
   - Edge cases and defaults
4. **Configuration** — JSON format with all fields documented
5. **MVP scope** — Explicitly state what is in-scope for the first iteration
   and what is deferred. The MVP boundary drives BDD scenario selection.

#### Design doc guidelines

- **Be specific about behavior, not implementation.** "When the device is
  unreachable, the equipment status reports `Disconnected`" is good.
  "Use a HashMap to track device state" is premature.
- **Include examples.** Show sample requests/responses, event payloads,
  configuration snippets. These translate directly into BDD scenarios.
- **Define error cases.** Every operation that can fail should have its failure
  behavior specified. These become BDD error scenarios.
- **Iterate the design before moving on.** The rp design doc went through 10
  commits before any tests were written. Refining the design on paper is far
  cheaper than refining it in code.

#### MVP scope definition

The design doc should clearly separate MVP from future work. Example from rp:

- **MVP:** equipment connectivity, MCP tool execution, event delivery, session
  lifecycle
- **Deferred:** safety constraints enforcement, planner/target selection,
  meridian flip, multi-camera orchestration

This boundary determines which BDD scenarios to write in Phase 2.

---

### Phase 2: Write BDD Tests

Write BDD feature files and test infrastructure **before** writing any
implementation code. The tests are executable specifications derived directly
from the design doc.

Treat the feature files as the **canonical contract** for plugin authors,
frontend developers, and integrators reading the repo from outside. Two
consequences flow from that:

1. Scenario titles state outcomes, and feature descriptions read as
   specifications -- not test plans. (See [testing.md §2.2](../skills/testing.md#22-feature-descriptions-state-the-contract)
   and [§2.3](../skills/testing.md#23-scenarios-describe-outcomes-not-procedures).)
2. Constants the contract pins (status codes, wire-format offsets,
   field-by-field expected values) live **in the feature file**, not
   hidden behind a `should be valid` step. (See
   [testing.md §2.5](../skills/testing.md#25-make-contract-constants-explicit-in-steps).)
   A reader should learn the contract from `tests/features/` alone.

#### Step 2a: Scaffold the BDD infrastructure

Create the test directory structure per docs/skills/testing.md Section 5:

```
tests/
  bdd.rs                    # Entry point (harness = false)
  bdd/
    world.rs                # World struct + helpers
    steps/
      mod.rs
      <concern>_steps.rs    # One per feature file
  features/
    <concern>.feature       # One per behavioral area
```

Register in `Cargo.toml`:

```toml
[[test]]
name = "bdd"
harness = false
```

If the BDD tests spawn child processes (via `ServiceHandle`), use the
`bdd_infra::bdd_main!` macro for Miri compatibility — see
docs/skills/testing.md Section 5.2.

#### Step 2b: Build test doubles

For services that depend on external systems, build test infrastructure early:

- **Simulators** for hardware or external services (e.g., OmniSim for Alpaca
  devices, mock PHD2 server)
- **Stub servers** for webhook receivers, plugin endpoints
- **Process management** helpers to start/stop subprocesses in tests

Process management (binary discovery, port parsing, graceful shutdown) is
centralized in the `bdd-infra` crate (`crates/bdd-infra`). Each service's
`tests/bdd/steps/infrastructure.rs` re-exports `ServiceHandle` from this
shared crate and adds any service-specific helpers (configuration builders,
mock setup, etc.). See `docs/skills/testing.md` Section 5.1 for details.

#### Step 2c: Write feature files from the design doc

Map each design doc section to a feature file. Each feature should cover:

1. **Happy path scenarios** — The normal operation described in the design doc
2. **Error scenarios** — Every error case the design doc specifies
3. **State transitions** — Connect/disconnect, start/stop, state changes
4. **Edge cases** — Missing parameters, unreachable services, timeouts

Example mapping from rp:

| Design Doc Section | Feature File | Scenarios |
|-------------------|--------------|-----------|
| Equipment connectivity | equipment_connectivity.feature | 9 |
| MCP tool catalog | tool_execution.feature | 14 |
| Event delivery webhooks | event_delivery.feature | 9 |
| Session lifecycle | session_lifecycle.feature | 14 |

Follow docs/skills/testing.md for all BDD conventions (scenario naming,
step reusability, World struct patterns, `@serial` tagging).

#### Step 2d: Write step definitions (stubs)

Write step definitions that compile but are not yet fully implemented. Given
steps set up test state, When steps call the system under test, Then steps
assert outcomes. At this point the When/Then steps will fail because there is
no implementation — that is expected.

##### Committing Phase 2 before Phase 3

If you need to commit Phase 2 (feature files + step defs) before Phase 3
implementation lands — for design review, to share progress on a feature
branch, or to mark a clean phase boundary — tag the new feature(s) with
`@wip` so the default test suite stays green. The `@wip` filter in
`bdd.rs` skips tagged scenarios at runtime; remove the tag in the same
commit that lands the implementation. See
[testing.md §2.7](testing.md#27-use-wip-tag-for-scenarios-without-implementation-yet)
for the exact convention and the runner snippet.

This is the only sanctioned way to commit failing scenarios. Do not
disable scenarios by commenting them out, prefixing with `#`, or
deleting them — `@wip` keeps them visible and easy to re-enable.

---

### Phase 3: Implement Code to Pass the Tests

Write the minimum implementation needed to make BDD scenarios pass, one feature
area at a time.

#### Implementation order

1. Pick a feature file (start with the simplest one)
2. Run the BDD tests — they should fail
3. Implement the minimum code to make scenarios pass
4. Run again — verify they pass
5. Move to the next feature file
6. Repeat until all MVP scenarios are green

#### After MVP scenarios pass

1. **Add unit tests** for internal components that BDD doesn't cover (protocol
   parsing, serialization, config defaults) — see docs/skills/testing.md
   Section 1.2
2. **Expand error scenarios** — add negative test cases for robustness
3. **Fix flaky tests** — address race conditions, timing dependencies, platform
   differences
4. **Run the full CI suite** — see docs/skills/pre-push.md
5. **Update the design doc** if behavior changed during implementation
   (AGENTS.md Rule 2)
6. **Open the PR and babysit it to merge readiness** — see
   docs/skills/babysitting-prs.md

---

## Conventions

### Typed physical quantities (newtypes over bare `f64`/`i32`)

When a service does unit- or frame-laden arithmetic (angles, encoder
ticks, sync offsets), wrap the quantity in a newtype that carries its
unit and reference frame rather than threading a bare `f64`/`i32`.
Distinct types make unit and frame mix-ups **compile errors** instead of
silent runtime faults, and conversions between frames become named,
type-checked methods rather than inline float math.

Precedents:

- `services/pa-falcon-rotator/src/units.rs` — `MechanicalDegrees` vs
  `SkyDegrees`, bridged by a `SyncOffset`.
- `services/star-adventurer-gti/src/units.rs` — `MechHa` vs `HourAngle`/
  `Ra`/`Lst`, `MechDec` vs `Dec`, and `RaTicks`/`DecTicks`/`Cpr`.

Style: hand-roll the constructors (which normalise/fold) and only the
operations that are *valid*. Do **not** derive blanket same-type
arithmetic (`derive_more`'s `Add`/`Sub`) — it skips normalisation and
re-admits meaningless operations (e.g. summing two absolute angles).
Keep the types service-local until a second device needs them; no
external units crate (`uom`). See
[ADR-006](../decisions/006-typed-physical-quantities-for-mount-pointing.md).

### Parse-don't-validate for config

Config invariants belong in the field *types*, checked at deserialize —
not in a runtime `validate()` pass. A field with a range or a cross-field
rule becomes a newtype (or a small grouped type) with a `serde`
`try_from` that rejects a bad value during `serde_json::from_str`, with
the offending field named, so a bad config fails at **load** (startup)
rather than mid-session. `star-adventurer-gti`'s `config.rs`
(`FlipRangeHours`, `CwExclusionZone`, `MinAltitudeDegrees`, …) is the
reference; it retired a hand-rolled `MountConfig::validate`.

---

## Example: rp Service Timeline

The rp service followed this workflow. The full commit history
([PR #41](https://github.com/rusty-photon/rusty-photon/pull/41)) shows the
phases clearly:

### Phase 1: Design (8 commits, Mar 3-4)

The design doc evolved through rapid iteration over two days. Each commit
refined the architecture based on review feedback:

1. Initial design: event-driven plugin system, exposure document model
2. Rename main-app to rp
3. Add plugin barrier and two-step webhook protocol
4. Add action system and workflow plugins
5. Adopt MCP as the wire protocol (replacing custom REST actions)
6. Move orchestration out of rp into a workflow plugin
7. Rework barriers and corrections for MCP-driven orchestration
8. Fix internal inconsistencies

The design was iterated on paper until it was internally consistent before
any code was written.

### Phase 2: BDD tests (1 commit, Mar 6)

Two days after the design stabilized, the full BDD test infrastructure was
scaffolded in a single commit (2,031 insertions):

- 4 feature files, 22 scenarios, 136 steps
- OmniSim Docker management for Alpaca device simulation
- In-process webhook receiver for event capture
- Configurable test orchestrator for session lifecycle testing

### Phase 3: Implementation (1 commit, Mar 8)

Two days after the tests, all 7 core modules were implemented in a single
commit (1,230 insertions). All 22 BDD scenarios passed.

### Phase 4: Stabilization (26 commits, Mar 8-17)

The longest phase addressed real-world CI and cross-platform concerns:

- OmniSim Docker replaced with native process for cross-platform CI
- Windows compatibility (temp_dir paths)
- macOS compilation fixes (trait solver overflow)
- Race condition fixes (port TOCTOU → two-phase ServerBuilder, session restart)
- Coverage setup for child-process BDD tests (graceful shutdown + SIGTERM)
- Sanitizer workflow fixes (env var isolation for OmniSim, per-package matrix)
- Error scenario expansion (22 → 46 scenarios with negative test cases)

### Key takeaway

The design and test phases (10 commits) produced a clear specification.
The implementation phase (1 commit) was fast because the behavior was
already fully defined. The stabilization phase (26 commits) was where CI,
cross-platform, and edge-case work happened — but the core design never
changed.

---

## References

- [AGENTS.md](../AGENTS.md) — Rule 1 (read docs before working), Rule 2 (update docs when behavior changes)
- [Testing skill](testing.md) — BDD conventions, step definitions, World struct patterns
- [Pre-push skill](pre-push.md) — Running CI quality gates
- [Service-lifecycle skill](service-lifecycle.md) — Standard `main.rs`
  shape, `ServiceRunner` API, shutdown / reload / SCM. Read when
  scaffolding a new long-running service binary.
- [rp design doc](../services/rp.md) — Reference example of a comprehensive design doc
- `services/rp/tests/features/` — Reference example of BDD feature files derived from a design doc
