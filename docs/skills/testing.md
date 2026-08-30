# Skill: Writing and Organizing Tests

## When to Read This

- Before writing any new tests (unit, BDD, or property-based)
- Before adding a new feature to a service (see the checklist in Section 9)
- Before migrating existing integration tests to BDD
- Before writing tests for `ui-htmx` or any other browser-facing service (see Section 10)

## Prerequisites

- Read the service's design document (`docs/services/<service>.md`) per AGENTS.md Rule 1a
- Familiarity with the Rust test ecosystem (`cargo test`, `#[test]`, `#[tokio::test]`)
- For BDD tests: the `cucumber` crate (cucumber-rs)

---

## Procedure

### 1. Test Pyramid: Which Test Type to Use

The project uses four testing layers. Each serves a distinct purpose.

#### 1.1 BDD Tests (Feature Files + Cucumber)

**Purpose:** Living specifications. These are the primary test type for service behavior.

**Why this matters:** Feature files are the canonical contract for
everyone outside the service team -- plugin authors, frontend developers,
reviewers, integrators. When someone asks "what does this endpoint
return?" or "what is the wire format for X?", the answer should be a
feature file, not the Rust source. This is what makes BDD primary, not
optional: the test suite **is** the spec, and a reader should be able
to learn the system's behavior from `tests/features/` alone. Every
assertion in a scenario is also documentation -- write the scenarios
with that in mind (see §2.5 on keeping contract constants visible in
the feature file rather than buried in step code).

**Use BDD tests for:**
- All observable device behavior (connect, disconnect, read values, evaluate state)
- Configuration loading and validation
- Error conditions that a user or integrator would encounter
- Cross-cutting concerns (concurrency, polling, state transitions)
- Any scenario where the Gherkin description itself has documentation value

**Do NOT use BDD tests for:**
- Wire protocol serialization/deserialization (use unit tests)
- Internal data structure invariants (use unit tests or property tests)
- ASCOM compliance (use ConformU)
- Fuzz-like edge case exploration (use property-based tests)

#### 1.2 Unit Tests (Rust `#[test]` / `#[tokio::test]`)

**Purpose:** Fast, focused verification of internal components.

**Use unit tests for:**
- Protocol parsing and serialization (in `src/protocol.rs` `#[cfg(test)]` module)
- Error type conversions and Display implementations (in `src/error.rs` `#[cfg(test)]` module)
- Configuration defaults and deserialization (in `src/config.rs` `#[cfg(test)]` module)
- Pure functions and data transformations
- In-source `#[cfg(test)]` modules for module-private logic

#### 1.3 Property-Based Tests (proptest)

**Purpose:** Discover edge cases through randomized input.

**Use property tests for:**
- Determinism invariants (same input always produces same output)
- Robustness (no panics on arbitrary input)
- Round-trip properties (serialize then deserialize returns original)
- Domain invariants that should hold for all inputs

#### 1.4 ConformU Integration Tests

**Purpose:** ASCOM Alpaca protocol compliance.

**Use ConformU tests for:**
- Verifying a service conforms to the ASCOM Alpaca standard
- These are always `#[ignore]` and run manually or in dedicated CI

---

### 2. BDD Feature File Rules

These are the most important rules. Feature files are both tests and documentation.

#### 2.1 One Feature File Per Concern

Organize feature files by functional area, not by implementation module. Each feature file should answer the question: *"What does the system do regarding [concern]?"*

**Good file names (concern-oriented):**
- `safety_evaluation.feature` -- How safety rules are evaluated
- `connection_lifecycle.feature` -- How the device connects and disconnects
- `file_polling.feature` -- How file changes are detected
- `configuration.feature` -- How configuration is loaded and validated

**Bad file names (implementation-oriented):**
- `device_tests.feature` -- Too vague
- `serial_manager.feature` -- Names an internal component, not a behavior
- `misc.feature` -- No organizing principle

#### 2.2 Feature Descriptions State the Contract

The `Feature:` line plus its description should read as a specification, not a test plan. State the behavioral contract: what the system does, what rules apply, what the defaults are.

**Good:**
```gherkin
Feature: Safety evaluation rules
  The safety monitor evaluates file content against configured parsing rules.
  Parsing rules are evaluated in order; the first match determines safety.
  No match defaults to unsafe.
```

**Bad:**
```gherkin
Feature: Safety evaluation tests
  Tests for the safety evaluation logic.
```

#### 2.3 Scenarios Describe Outcomes, Not Procedures

A scenario title should state **what happens** (the outcome), not **what you do** (the procedure). A reader should understand the expected behavior from the title alone.

**Good:**
```gherkin
Scenario: Device starts disconnected
Scenario: First matching rule wins
Scenario: Disconnected device reports unsafe via is_safe
Scenario: Fail to connect when monitored file does not exist
```

**Bad:**
```gherkin
Scenario: Test device connection
Scenario: Check rule priority
Scenario: Call is_safe when disconnected
```

#### 2.4 Use Scenario Outlines for Parameterized Behavior

When the same behavioral pattern applies to multiple inputs, use `Scenario Outline` with `Examples`. This makes the pattern explicit and the variations easy to scan.

```gherkin
Scenario Outline: Contains rule evaluation
  Given case-insensitive matching
  And a contains rule with pattern "OPEN" that evaluates to safe
  And a contains rule with pattern "CLOSED" that evaluates to unsafe
  And a device configured with these rules
  When I evaluate the safety of "<content>"
  Then the result should be <expected>

  Examples:
    | content              | expected |
    | Roof Status: OPEN    | safe     |
    | Roof Status: CLOSED  | unsafe   |
    | roof status: open    | safe     |
    | Unknown status       | unsafe   |
```

Use individual scenarios (not outlines) when different inputs need different setup or tell different stories.

#### 2.5 Make Contract Constants Explicit in Steps

Step expressions should expose the values being checked, not hide them
behind opaque adjectives. A reader of the feature file should learn the
contract -- specific numbers, field names, status codes, wire-format
positions -- without opening any Rust code. This is what makes feature
files documentation; an opaque `should be valid` step is a hole in the
spec.

**Bad -- the contract lives in the step implementation, invisible to a reader:**
```gherkin
Then the response should have a valid ImageBytes header
```

**Good -- the constants live in the feature file, where plugin authors and reviewers can see them:**
```gherkin
Then the image pixels header should match these constants (i32 little-endian):
  | field                     | offset | value |
  | metadata_version          | 0      | 1     |
  | data_start                | 16     | 44    |
  | image_element_type        | 20     | 2     |
  | transmission_element_type | 24     | 8     |
  | rank                      | 28     | 2     |
```

A single step definition iterates the table, parses each value at the
declared offset, and asserts equality. The spec is now legible to a
plugin or frontend author without `cargo doc` or grepping the source.

**When to use a Gherkin data table:**
- Validating a wire-format header, status-code matrix, or any structured
  output where the relationship between fields is the contract.
- Parameterizing the same assertion shape across many fields.

**When literal step text is enough:** for one or two values, a plain
step is more readable than a one-row table -- `Then the response status
should be 200` is already explicit. The same principle applies:
`Then bitpix should be 16` beats `Then bitpix should be valid for U16`,
because "valid" forces the reader into the step source to recover the
meaning.

#### 2.6 Use `@serial` Tag for Tests With Side Effects

Tag features or scenarios with `@serial` when they depend on timing, shared resources, or file I/O that cannot safely run in parallel.

```gherkin
@serial
Feature: File content polling
```

#### 2.7 Use `@wip` Tag for Scenarios Without Implementation Yet

This project's design-first / test-first workflow (see
[development-workflow.md](development-workflow.md)) sometimes requires
landing BDD scenarios on a feature branch *before* the production code
that makes them pass exists. The `@wip` tag lets such scenarios live in
the repo as durable design artifacts without breaking the green-suite
invariant.

```gherkin
@wip
Feature: Basic image measurement tool
  ...
```

The runner in `bdd.rs` filters scenarios tagged `@wip` (at feature or
scenario level) out of the default suite. Once the corresponding
implementation lands, **remove the tag** in the same commit that turns
the scenarios on for real.

To enable this in a service's `bdd.rs`, swap `.run_and_exit("tests/features")`
for `.filter_run_and_exit("tests/features", filter_fn)`:

```rust
.filter_run_and_exit("tests/features", |feat, _rule, sc| {
    let is_wip = feat.tags.iter().any(|t| t == "wip" || t == "@wip")
        || sc.tags.iter().any(|t| t == "wip" || t == "@wip");
    !is_wip
})
```

**Use `_and_exit` — never plain `filter_run` / `run`.** The bare variants
return the cucumber summary and let the test binary exit 0 even when
scenarios fail. Because BDD tests use `harness = false`, `cargo test --test
bdd` only fails when the binary exits non-zero, so plain `filter_run` makes
scenario failures silently green in CI. See issue #171 for the precedent
where 81 failed rp scenarios passed CI for months. The `_and_exit` variants
have identical signatures plus the side effect of `process::exit(1)` on
failure — there is no reason to prefer the bare form.

Use `@wip` only for not-yet-implemented behavior. A scenario that fails
intermittently belongs in an issue, not behind `@wip`. A scenario that
documents a deferred feature you are not actively working on belongs in
a follow-up ticket, not in the repo with `@wip`.

#### 2.8 Avoid Gherkin Parser Pitfalls

These are known issues with the Gherkin parser used by cucumber-rs:

- **Do NOT start description lines with `Rule`** -- it is a Gherkin 6+ keyword and will be parsed as structure, not text.
- **Do NOT use `|` in step text** -- it is the table delimiter. Use symbolic names mapped in step definitions instead.
- **Regex patterns go in step definitions, not feature files.** Use human-readable names in features (e.g., `"safe_or_ok"`) mapped to actual patterns in code via a resolver function.

---

### 3. Step Definition Rules

#### 3.1 Organize Steps by Concern, Matching Feature Files

Each feature file should have a corresponding step definition module. Steps that are shared across features (like connection steps used in both `connection_lifecycle.feature` and `file_polling.feature`) go in the module matching their primary concern.

```
features/                        steps/
  configuration.feature    ->      config_steps.rs
  connection_lifecycle.feature ->  connection_steps.rs
  safety_evaluation.feature  ->   safety_steps.rs
  file_polling.feature       ->   polling_steps.rs
  concurrency.feature        ->   concurrency_steps.rs
```

#### 3.2 Steps Must Be Reusable Across Scenarios

Write step definitions that are generic enough to be composed across scenarios. The same `Given a monitoring file containing {string}` step is used in connection, polling, and concurrency scenarios.

**Good (reusable, parameterized):**
```rust
#[given(expr = "a monitoring file containing {string}")]
fn monitoring_file_containing(world: &mut MyWorld, content: String) {
    world.create_temp_file(&content);
}
```

**Bad (scenario-specific):**
```rust
#[given("a file with OPEN content for safety test")]
fn setup_safety_file(world: &mut MyWorld) {
    world.create_temp_file("OPEN");
}
```

#### 3.3 Use `expect()` in Given/When Steps, `assert!` in Then Steps

- **Given** steps set up preconditions. If setup fails, the test infrastructure is broken -- use `expect()` or `unwrap()` to fail fast with a clear message.
- **When** steps execute the action under test. Use `unwrap()` for actions that should succeed, or capture errors into `world.last_error` when testing failure paths.
- **Then** steps verify outcomes. Use `assert!`, `assert_eq!`, or `assert_ne!` with descriptive messages.

```rust
// Given: fail fast on setup problems
#[given(expr = "a monitoring file containing {string}")]
fn monitoring_file(world: &mut MyWorld, content: String) {
    world.create_temp_file(&content);  // uses expect() internally
}

// When: capture errors for failure scenarios
#[when("I try to connect the device")]
async fn try_connect(world: &mut MyWorld) {
    match device.set_connected(true).await {
        Ok(()) => world.last_error = None,
        Err(e) => world.last_error = Some(e.to_string()),
    }
}

// Then: assert with context
#[then("the result should be safe")]
fn result_safe(world: &mut MyWorld) {
    let result = world.safety_result.expect("no safety result");
    assert!(result, "expected safe but got unsafe");
}
```

#### 3.4 Use a Pattern Resolver for Complex Values

When feature files need to reference technical values (regex patterns, JSON, binary data), use human-readable symbolic names in Gherkin and resolve them in step definitions.

```rust
fn resolve_regex_pattern(name: &str) -> String {
    match name {
        "safe_or_ok" => r"Status:\s*(SAFE|OK)".to_string(),
        "danger_or_error" => r"Status:\s*(DANGER|ERROR)".to_string(),
        "invalid_bracket" => "[invalid(".to_string(),
        other => other.to_string(),
    }
}
```

This keeps feature files readable while allowing precise technical control.

#### 3.5 Distinguish "I do X" from "I try to do X"

Use separate step definitions for actions expected to succeed vs. actions expected to fail:

- `When I connect the device` -- calls `unwrap()`, fails the test if the action fails
- `When I try to connect the device` -- captures the error into `world.last_error`

This makes the intent clear in both the feature file and the step definition.

---

### 4. World Struct Rules

#### 4.1 Use `Option<T>` for State That Builds Incrementally

The World struct accumulates state across Given/When steps. Use `Option<T>` for values that are set during the scenario and `Vec<T>` for collections that grow.

```rust
#[derive(Debug, Default, World)]
pub struct MyWorld {
    pub config: Option<Config>,           // Set once during setup
    pub device: Option<Arc<MyDevice>>,    // Built from accumulated state
    pub rules: Vec<ParsingRule>,          // Grows with each Given step
    pub last_error: Option<String>,       // Captured in When steps
    pub temp_dir: Option<TempDir>,        // Manages temp file lifetime
}
```

#### 4.2 Put Setup Helpers on the World Struct

Common setup logic (creating temp files, building configs, constructing devices) should be methods on the World struct, not free functions in step files. This keeps step definitions thin.

```rust
impl MyWorld {
    pub fn create_temp_file(&mut self, content: &str) -> PathBuf { ... }
    pub fn build_config(&self, file_path: PathBuf) -> Config { ... }
    pub fn build_device(&mut self) { ... }
}
```

#### 4.3 Use `TempDir` for File Lifecycle

The `tempfile::TempDir` stored in the World struct ensures automatic cleanup when each scenario ends. Never use hardcoded paths for test files.

#### 4.4 Wrap Devices in `Arc` for Concurrency Scenarios

Concurrency scenarios spawn multiple async tasks that all need access to the device. Store devices as `Arc<Device>` in the World struct from the start. This avoids needing different World types for concurrent vs. sequential scenarios.

---

### 5. BDD Test Infrastructure Rules

#### 5.1 Shared Infrastructure: `bdd-infra` Crate

The `crates/bdd-infra` crate provides shared process lifecycle management for all
services' BDD and integration tests. It eliminates per-service duplication of
binary discovery, spawning, port parsing, and graceful shutdown logic.

**Key types (default feature set):**

- `ServiceHandle` — spawns a service binary, parses its bound port from stdout,
  provides `stop()` for graceful SIGTERM shutdown, and drains stdout to prevent
  pipe deadlocks. On `Drop`, sends a best-effort SIGTERM.
- `parse_bound_port()` — standalone function also usable by ConformU tests.
- `run_once_async()` / `run_once()` — run a binary once (a `doctor` subcommand,
  a `--help`) and reap its output. Steps use the async form; see
  [§5.7](#57-never-block-in-a-step--the-whole-suite-shares-one-poll-loop).
- `tls_auth::shared_pki()` — the suite's throwaway CA, service certificate and
  credential pair, generated once per test process on the blocking pool. Steps
  call this, never `PkiFixture::generate` directly; see
  [§5.7](#57-never-block-in-a-step--the-whole-suite-shares-one-poll-loop).

**Labeled stderr forwarding.** Every spawned child's stderr (where every
service's `tracing` output goes) is captured and re-printed line-by-line,
prefixed with a `<package>#<seq>` label unique to that spawn — e.g.
`[sentinel#12] ...`. Cucumber runs up to 64 scenarios concurrently by default,
and most feature files aren't tagged `@serial`, so many same-named service
instances (several scenarios each discovering a stub as, say,
"plate-solver") can be alive at once, each its own child process. Without a
label, their stderr lines merge into one shared, unattributed stream and a
reader (or an agent) cannot tell which lines belong to which instance — this
made a real CI flake (issue #578) misread as one continuously-unhealthy
service when it was most likely several concurrent instances' logs
interleaved. When debugging BDD/CI output, `grep` for one instance's label to
isolate its full lifecycle from the merged stream.

**Optional `rp-harness` feature** — adds the higher-level helpers needed when a
test spawns `rp` alongside OmniSim and/or an orchestrator plugin:

- `OmniSimHandle` — per-test-process Alpaca simulator shared across that
  process's scenarios. Spawned with the fork's `--multi-instance` flag on a
  dynamically chosen port with a private settings dir (the fork's
  `OMNISIM_SETTINGS_DIR` env var — works on every OS, unlike
  `XDG_CONFIG_HOME`, which .NET ignores on macOS), so concurrent test
  processes (parallel Bazel suites, `rp:bdd` shards, a dev OmniSim on the
  default port) never contend for one simulator or leak persisted profile
  settings into each other. The spawn also pins the .NET culture
  (`DOTNET_SYSTEM_GLOBALIZATION_INVARIANT=1`) so the seeded profile parses
  the same on a comma-decimal host as on an `en_US` runner, and gates on
  the device roster once OmniSim is healthy: an entry without a `UniqueID`
  fails the spawn with a panic that names the device and quotes OmniSim's
  startup exception, instead of letting rp's discovery fail for every
  device and the suite fail on downstream symptoms (#918). See
  `docs/references/omnisim.md` § "Profile values are parsed with the
  current culture".
- `RpConfigBuilder` + `CameraConfig` / `FilterWheelConfig` /
  `CoverCalibratorConfig` — fluent builder that emits rp's JSON config.
- `start_rp`, `wait_for_rp_healthy`, `write_temp_config_file`,
  `sibling_service_dir` — launch helpers.
- Everything the harness writes to disk — the configs those helpers write,
  and the default `session.data_directory` / `session.session_state_file`
  the builder mints when a scenario does not pin its own — lands in **one
  per-process scratch directory** with a random name component
  (`rp_harness/scratch.rs`), created on first use under Bazel's per-action
  `TEST_TMPDIR` (wiped by Bazel before every run) or the system temp
  directory under cargo. Paths are therefore unique across processes by
  construction, not by naming: the machine-wide temp directory is shared by
  every shard, every suite, and whatever ran on the image before, and a
  registry left there by an earlier process — rp keeps an active registry
  across a stop by design — would be *restored* by any later process that
  happened to mint the same name, dropping a scenario into a session it never
  started. Windows recycles PIDs freely enough for a `<pid>-<seq>` scheme to
  do exactly that. Scenarios that need a path across an rp restart still pin
  one explicitly (`with_data_directory` / `with_session_state_file`).
- `WebhookReceiver`, `TestOrchestrator`, `OrchestratorBehavior` — in-process
  plugin stand-ins.
- `McpTestClient` — persistent rmcp client for calling rp's MCP tools. Each
  `call_tool` / `list_tools` request is bounded by `MCP_CALL_TIMEOUT` (360 s):
  rmcp has no built-in client timeout, so if an rp tool handler hangs the
  `await` would otherwise block forever. (A `do_capture` loop that span
  indefinitely on a failed `sky-survey-camera` exposure burned a full 6 h per
  CI job this way before it was fixed at the source.) The bound is a backstop
  so any future handler hang fails the scenario fast rather than running the
  job to its `timeout-minutes` cap.

Turn the feature on **only** for tests that actually spawn rp. Services whose
BDD tests only need `ServiceHandle` (filemonitor, qhy-focuser, ppba-driver,
sentinel, …) should leave it off so they don't compile axum, reqwest, and
rmcp transitively.

**Optional `tls-auth` feature** — the shared TLS + HTTP Basic Auth test
fixture behind every service's `auth.feature`:

- `PkiFixture` — per-scenario throwaway PKI (generated CA + service
  certificate signed by it) plus a per-run generated password and its
  Argon2id hash. Because the password is random, no suite carries a
  hard-coded credential (CodeQL's `hard-coded-cryptographic-value` query
  stays quiet — no used-in-tests dismissal ritual). Accessors: `ca_path()` /
  `cert_path()` / `key_path()`, `https_client()` (a reqwest client trusting
  the CA), and the JSON fragments `tls_block()` / `auth_block()` /
  `server_block(port)`.
- `TlsAuthState` + `TlsAuthSmokeWorld` + `tls_auth_smoke_steps!` — the smoke
  suite in a box. The World embeds a `TlsAuthState`, implements the trait
  (base config JSON, launch mechanism, and — for non-Alpaca services — a
  `PROBE_PATH` override, usually `/health`), and invokes the macro in its
  `auth_steps.rs`; the feature file is the byte-identical service-neutral
  `auth.feature` (copy dsd-fp2's). Deep TLS/auth suites (ppba-driver,
  ui-htmx, …) keep their own scenario sets but build on `PkiFixture` and
  `wait_until_ready` instead of hand-rolled cert plumbing.

**Doctor smoke (always available, no feature)** — the shared fixture behind
every service's `doctor.feature`
([doctor.md §Per-service doctors](../services/doctor.md)):
`DoctorSmokeState` + `DoctorSmokeWorld` + `doctor_smoke_steps!` in
`bdd_infra::doctor_smoke`. The World embeds the state, implements the trait
(`valid_config()` — a config JSON the service's own `deny_unknown_fields`
load accepts in full), and invokes the macro in its `doctor_steps.rs`; the
feature file is the byte-identical service-neutral `doctor.feature` (copy
ppba-driver's). The two scenarios spawn the suite's staged binary with
`doctor --json` and assert the clean-report and unknown-key contracts; the
runner's deep behavior is unit-tested in `rusty-photon-doctor-checks`.

Under Bazel, BDD targets link the matching crate variant:
`//crates/bdd-infra:bdd-infra_tls_auth`, or
`:bdd-infra_rp_harness_tls_auth` when the suite also spawns rp.

```toml
# rp's own tests and any rp-client plugin's tests:
bdd-infra = { workspace = true, features = ["rp-harness", "tls-auth"] }

# Services whose tests only spawn themselves:
bdd-infra = { workspace = true, features = ["tls-auth"] }
```

**Convention: per-plugin BDD suites.** End-to-end tests for an rp orchestrator
or event plugin live in that plugin's own `services/<plugin>/tests/` tree, not
in `services/rp/tests/`. Each plugin owns a small world type that embeds the
handles it needs and calls `rp_harness::start_rp(&config)` — the helper
derives `RP_BINARY` from the package name, so nothing needs to know where
`services/rp/` lives on disk. This keeps rp's test run time bounded as more
plugins land — Bazel only re-runs the plugin test whose code (or deps) changed.

**Usage in test code:**

```rust
use bdd_infra::ServiceHandle;

let handle = ServiceHandle::start(env!("CARGO_PKG_NAME"), &config_path).await;
// handle.port, handle.base_url are available
handle.stop().await;
```

The `env!("CARGO_PKG_NAME")` macro resolves to the calling service's package
name at compile time. `bdd-infra` derives the binary discovery env var from
it: `rp` → `RP_BINARY`, `ppba-driver` → `PPBA_DRIVER_BINARY`, etc.

A child that needs extra flags takes `start_with_args`; one that needs a
per-scenario **environment** takes `start_with_env` (e.g. sentinel's
`SENTINEL_SERVICE_MANAGER_DIR` stub seam — see
[sentinel.md §The test seam](../services/sentinel.md#the-test-seam-sentinel_service_manager_dir)).
Never `std::env::set_var` for a spawned child: scenarios run concurrently in
one process, so process-global env mutation races across scenarios.

**Binary discovery order:**

1. Explicit env var `{PACKAGE_UPPER_SNAKE}_BINARY` (e.g., `FILEMONITOR_BINARY=/path/to/bin`).
2. `$CARGO_TARGET_DIR/debug/<pkg>` (or `$CARGO_LLVM_COV_TARGET_DIR/debug/<pkg>` under
   `cargo llvm-cov`) when either env var is set. If `CARGO_BUILD_TARGET` is also set,
   the triple is inserted: `.../<triple>/debug/<pkg>`. When one of these env vars is
   set we look **only** there — falling through to step 3 could silently pick up a
   stale, non-instrumented binary and skip coverage data collection.
3. Walking up from the current directory looking for `target/debug/<pkg>`.
   `cargo test -p <pkg>` runs tests with the cwd at the package dir, so the
   workspace `target/` is typically one level up.

If none match, the call panics with a diagnostic pointing at the fix.

**Pre-build requirement.** BDD tests do not compile the service binary
themselves. Under `bazel test` the binary is built automatically as a BDD-target
dependency, with the right features (e.g. `ppba-driver`'s `mock` hardware) wired via
its deps:

```
bazel test //services/<pkg>:bdd          # builds the binary as a dep
bazel test //...                          # BDD suites run by default (result-cached)
```

A local `bazel build //...` pre-builds the affected packages; the nightly
`.github/workflows/test.yml` builds the full workspace.

**Port discovery:** All services print `bound_addr=<host>:<port>` to stdout when
they bind. The parser looks for `bound_addr=` in any line (the human-readable
prefix before it can vary per service).

**Picking a port a child process will bind.** Default to the announced-port
pattern above: let the child `bind(0)` and print what the kernel gave it. When a
test genuinely has to know the port *first* — because the port is an input to the
API under test, or because the test needs a port with **nothing** listening —
take it from a reserved band **below the platform's ephemeral floor** (32768 on
Linux, 49152 on Windows and macOS), entered at a per-process start so concurrent
test binaries do not march in step. `reserved_test_port()` in
`services/phd2-guider/tests/test_integration.rs` is the reference implementation.

Do **not** probe by binding `:0` and releasing it. Two things go wrong:

- The port comes from the range the OS hands to every other `bind(0)` in the
  process, so a server started concurrently can be given that port between the
  probe and its use. This was issue #745: the losing test's `Phd2ProcessManager`
  found the winner's mock on "its" port, reported success without spawning
  anything, then shut that mock down over RPC — failing whichever unrelated test
  owned it.
- The probe's own listener is duplicated into any child a sibling thread forks in
  that instant, and survives the probe's close until that child execs. So the
  probe can hand back a port that still answers — measured at ~6% of probes at
  that suite's spawn rate. **Probe by connecting instead**: a refused connect is
  the same "nobody is there" answer and leaves nothing behind.

Where a bind-and-release probe does survive (`omnisim.rs::pick_free_port`,
`pebble.rs::free_ports`) it is paired with a retry on the lost race. Keep that
pairing if you add another.

#### 5.2 Entry Point Structure

Each service's BDD tests follow this structure:

```
tests/
  bdd.rs                    # Entry point (harness = false)
  bdd/
    world.rs                # World struct + helpers
    steps/
      mod.rs                # pub mod for each step file
      connection_steps.rs
      ...
  features/
    connection_lifecycle.feature
    ...
```

Services that spawn rp (plugin workflows) import shared helpers from
`bdd_infra::rp_harness` directly — there is no `steps/infrastructure.rs`
re-export layer.

The `bdd.rs` entry point uses `#[path = "..."]` imports because test crate roots see siblings, not children.

**BDD tests that spawn child processes** (i.e. use `ServiceHandle`) **must** use the
`bdd_infra::bdd_main!` macro instead of a hand-written `#[tokio::main] async fn main()`.
The macro expands to an empty `fn main() {}` under Miri, because Miri does not
support the `pidfd_spawnp` FFI that tokio uses for process spawning on Linux.
BDD tests that are purely in-process (e.g. qhy-focuser with mock serial ports)
do not need the macro.

```rust
#[path = "bdd/world.rs"]
mod world;

#[path = "bdd/steps/mod.rs"]
mod steps;

bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::MyWorld;

    MyWorld::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    if let Some(handle) = world.service_handle.as_mut() {
                        handle.stop().await;
                    }
                }
            })
        })
        .run_and_exit("tests/features")
        .await;
}
```

If your `World` holds an `McpTestClient` (or any other long-lived
streaming client), the `after` hook needs an extra step to avoid silently
losing BDD coverage — see [§5.4](#54-drop-mcp--streaming-clients-before-stopping-the-service).

#### 5.3 Register in Cargo.toml

```toml
[dev-dependencies]
bdd-infra = { workspace = true }

[[test]]
name = "bdd"
harness = false
```

No per-service metadata is required — `bdd-infra` derives everything from the
package name passed to `ServiceHandle::start`.

#### 5.4 Drop MCP / streaming clients before stopping the service

**Critical for coverage.** If your `World` holds an `McpTestClient` (or any
client that keeps a long-lived HTTP/streaming connection to a child service),
you **must** drop it *before* calling `ServiceHandle::stop()` in the cucumber
`after` hook. Otherwise the service hangs in graceful shutdown, gets SIGKILLed
after `stop()`'s 5-second timeout, and skips its `atexit` handlers — which
means **no `.profraw` is written and BDD coverage is silently lost**.

The mechanism: `rmcp`'s `StreamableHttpClientTransport` opens a long-lived
HTTP request to the server's `/mcp` endpoint. axum's
`with_graceful_shutdown` waits for in-flight connections to close before
returning. As long as the client is alive, the connection stays open, axum
never returns, the service can't run `atexit`, and SIGKILL claims the process
along with its in-memory LLVM coverage counters.

```rust
bdd_infra::bdd_main! {
    use cucumber::World as _;
    use world::MyWorld;

    MyWorld::cucumber()
        .after(|_feature, _rule, _scenario, _finished, maybe_world| {
            Box::pin(async move {
                if let Some(world) = maybe_world {
                    // Drop streaming clients FIRST so the server's graceful
                    // shutdown can complete. Skipping this turns BDD
                    // subprocess coverage silently into 0%.
                    world.mcp_client = None;
                    if let Some(handle) = world.service_handle.as_mut() {
                        handle.stop().await;
                    }
                }
            })
        })
        .filter_run_and_exit("tests/features", |_, _, _| true)
        .await;
}
```

The same applies to any other long-lived client connection (custom WebSocket
transports, gRPC streams, etc.) — drop the client before stopping the
service it talks to.

**When this does NOT apply:** plugin BDD harnesses like `calibrator-flats`
where the MCP client lives *inside* the plugin's child process (talking to
rp), not in the BDD test process. Those harnesses just stop the plugin
first, then rp — when the plugin shuts down it closes its own MCP client
gracefully, freeing rp's connection.

**How to spot a regression of this issue:** `mcp.rs` (or any tool-dispatch
module) shows much higher coverage from unit tests than from BDD — e.g. a
big gap between BDD-only coverage and combined coverage. Confirm by
temporarily replacing `debug!("did not exit after SIGTERM, sending SIGKILL", ...)`
in `bdd-infra` with `eprintln!` and re-running BDD with stderr captured. A
SIGKILL count > 0 means the lifecycle ordering is wrong somewhere.

#### 5.5 Reset OmniSim devices in the `before(scenario)` hook

**Critical for isolation.** OmniSim is a per-process singleton — every
scenario shares the same simulator instance, so device state (mount
position, AtPark/Tracking, cover position, calibrator brightness, filter
slot, focuser position, camera config) leaks from scenario N into
scenario N+1. The mount-state leak that hung `mount.feature::park` in
issue #143 is the specific case we already hit; the fix generalises to
every device class.

The pattern is to call `OmniSimHandle::reset_all_devices()` from a
cucumber `before(scenario)` hook. The helper issues sequential
`PUT /simulator/v1/{class}/{n}/restart` requests to OmniSim's private
restart endpoint for every class our suites touch (`telescope`,
`camera`, `filterwheel`, `focuser`, `covercalibrator`); see
`crates/bdd-infra/src/rp_harness/omnisim.rs`. Each PUT also takes a
process-wide mutex, so hooks of concurrently-running scenarios never
have more than one restart in flight — OmniSim's restart handler
mutates unsynchronised static state, and concurrent restarts have
corrupted its device list (#171) and deadlocked it outright (#431).
Total per-scenario overhead is five localhost round-trips
(~10-25 ms) when the runner is healthy. That is the typical case, not a
bound: when a dependency bump invalidates the graph and every BDD suite
re-executes cache-cold at once, individual restarts have taken seconds
on a Windows runner. Each PUT is therefore sent with a deliberately
generous `RESET_ATTEMPT_TIMEOUT`, and only a **connect** failure — where
the request provably never reached the simulator — is retried. A timeout
is not: it means the client stopped waiting while the server may still be
mid-reload, and replaying onto that is the concurrent-restart overlap the
serializer mutex exists to prevent. A reset that fails anyway panics the
hook and fails the shard, and the message carries reqwest's full
`source()` chain so a refused connection (the simulator is gone) reads
differently from a blown deadline (it is alive but slow). Before the singleton has been initialised the helper is
a no-op: the test process hasn't spawned its OmniSim yet, and the
private instance `OmniSimHandle::start()` eventually spawns is fresh
by construction (pre-existing instances are never reused). The hook
is safe to wire up unconditionally.

```rust
MyWorld::cucumber()
    .before(|_feature, _rule, _scenario, _world| {
        Box::pin(async move {
            bdd_infra::rp_harness::OmniSimHandle::reset_all_devices().await;
        })
    })
    .after(/* ... */)
```

`OmniSimHandle::reset_all_devices` already covers every device class our
BDD suites exercise today: telescope, camera, filter wheel, focuser, cover
calibrator, safety monitor, switch, rotator, observing conditions, and dome.
If a future device class exposes the same `/restart` endpoint shape, add its
`reset_X` helper call there too so contributors get the isolation for free.

**Why scenarios that touch OmniSim must serialize.** A `before(scenario)`
reset that disconnects the shared simulator's devices is unsafe to
issue while another scenario is mid-flight against the same singleton —
scenario N+1 would yank Connected back to false out from under scenario
N's `StartExposure`. cucumber-rs draws at most one `@serial` scenario
at a time and refuses to drain Concurrent scenarios while a Serial
scenario is queued, so tagging every OmniSim-touching feature with
`@serial` (file-level) is sufficient. Look at
`services/rp/tests/features/*.feature` and
`services/calibrator-flats/tests/features/*.feature` for the expected
pattern: a single `@serial` tag on the line above `Feature:`. Adding
a new feature that touches OmniSim? Tag it `@serial` too. (Some
other features are tagged `@serial` for their own reasons — auth
tests share an Argon2id keying constant — so don't take tagging as
evidence one way or the other about OmniSim. The TLS / ACME features
are deliberately untagged: their scenarios use per-scenario temp
dirs and never touch OmniSim devices. Note cucumber refuses to
drain Concurrent scenarios while `@serial` ones are queued, so all
untagged scenarios launch simultaneously once the serial queue
empties — their before-hooks fire as one burst, which is why the
restart PUTs are serialized process-wide; see #431.)

**Parallelism comes from processes, not from dropping `@serial`.**
Since #467 every BDD test process owns a private OmniSim
(`--multi-instance` + dynamic port + per-instance settings dir), so
the wall-clock lever is more processes, each with its own simulator:
the four OmniSim suites run concurrently under Bazel on Linux/macOS,
and `rp:bdd` is additionally split into parallel shard processes via
Bazel `shard_count`. To shard a suite: (1) set `shard_count` on its
`rust_test` target, and (2) route its cucumber filter through
`bdd_infra::sharding::scenario_in_current_shard(feat.path.as_deref(),
&feat.name, sc.position.line)` — `bdd_main!` already advertises
sharding support to Bazel. Skipping step 2 silently makes every shard
run the whole suite. Scenarios are partitioned by a stable hash of
(feature file name, scenario line), and `@serial` still applies within
each shard, which is exactly the scope it protects — one process's
shared instance. This holds on every OS: profile-store isolation uses
the fork's `OMNISIM_SETTINGS_DIR` (release `v0.5.0-467.2`), which
re-roots OmniSim's profile store per instance on all platforms — the
default store is not redirectable on Windows or macOS, and a shared
store leaks persisted settings (e.g. the telescope site) between
concurrently running suites.

#### 5.6 Pebble: end-to-end ACME scenarios in doctor's suite

Doctor's `@pebble`-tagged scenarios run the real `instant-acme` order
flow against [Pebble](https://github.com/letsencrypt/pebble) (Let's
Encrypt's official ACME test server) plus its `pebble-challtestsrv` DNS
sidecar — the only coverage of the ACME network path that is not a mock
(see `docs/services/doctor.md` §Renewal). The harness lives in
`services/doctor/tests/bdd` (single consumer — not `bdd-infra`): each
scenario spawns a private Pebble on dynamic ports with a
`rusty_photon_tls::test_cert`-minted certificate for its HTTPS endpoint,
points Pebble's validating resolver at the sidecar, and drives the
doctor binary with `--directory-url`/`--acme-root`.

Two environment variables locate the binaries, mirroring
`OMNISIM_PATH`: **`PEBBLE_PATH`** and **`PEBBLE_CHALLTESTSRV_PATH`**
(both forwarded into Bazel test actions via `--test_env` in `.bazelrc`).
Unlike OmniSim they are **optional on a dev box**: when either is unset,
the suite skips the `@pebble` scenarios and prints a loud
`skipping N @pebble scenarios` notice — the rest of doctor's suite runs
normally, so a box without Pebble stays green while never *silently*
under-testing. Pebble backs six scenarios in one suite, so making it a
hard prerequisite for all 95 targets would cost more than it buys.

**In CI that skip is a failure.** `.bazelrc` sets
`test:ci --test_env=RUSTY_PHOTON_REQUIRE_PEBBLE=1`, and the suite's entry
point turns missing locators into a panic naming how many scenarios the
run would have dropped. Every workflow that runs doctor's suite provisions
the binaries today (`.github/actions/install-pebble`, pinned +
SHA-verified like install-omnisim), so the guard is inert — until an
install step breaks, a workflow forgets it, or a new runner can't fetch
the release, which is exactly when a skip notice in a green log would go
unread.

To run them locally: **`scripts/install-pebble.sh`** downloads and
SHA-verifies the same pinned release into `~/.local/opt/pebble`
(`--prefix` to choose elsewhere) and prints the two exports to add to your
shell profile. Bumping the version means updating the table in both the
script and the CI action — both fail closed on a mismatch.

#### 5.7 Never block in a step — the whole suite shares one poll loop

Cucumber drives every scenario as a `LocalBoxFuture` in a **single**
`FuturesUnordered`, polled by the one task `bdd_main!` hands to
`Runtime::block_on`. Up to 64 scenarios are in flight there (cucumber's
default), and they are concurrent, not parallel: whatever a step does
synchronously, the other 63 scenarios wait through.

That makes a blocking wait inside a step — most often waiting on a child
process — cost far more than the step it sits in. While it runs:

- no other scenario is polled, so their in-process test servers stop
  answering and their timers fire late;
- steps elsewhere fail with transport errors and timeouts that read like
  product bugs. `rusty-photon-tls`'s server bounds how long a connection
  may make no progress and drops it when the bound passes, so a client
  starved past that bound sees a bare `ConnectionAborted` with nothing
  wrong on either side;
- the suite loses its concurrency: wall time becomes the *sum* of every
  blocking wait. Doctor's suite spends most of its steps running the
  doctor binary — moving those waits off the poll loop took it from 20.6 s
  to 3.9 s locally, and from 160.8 s to 5.7 s with each spawn slowed to
  900 ms to emulate a contended 4-vCPU Windows runner.

So: **`bdd_infra::run_once_async` in a step, never `run_once`** (the
synchronous form is for plain `#[test]`s, which have no runtime to yield
to), and any other blocking wait belongs in `tokio::task::spawn_blocking`.
`tokio::task::block_in_place` is *not* a fix here — it hands a worker's
queue to a replacement thread, and the poll loop being starved is not on a
worker.

CPU-bound setup counts as blocking, not just waiting on a child. The TLS
fixture is the standing example: a CA keypair, a service keypair, a
signature and an Argon2id hash — which exists to be expensive, and which
`bazel test` builds unoptimised. Call
**`bdd_infra::tls_auth::shared_pki`, never `PkiFixture::generate`**: it
runs that work on the blocking pool *and* once per test process rather
than once per scenario, since a suite's scenarios all want the same
throwaway PKI. Reach for the same shape whenever setup is both expensive
and identical across scenarios.

The same reasoning applies to a helper a step calls: make it `async` all
the way down rather than blocking one level lower.

#### 5.8 What a BDD suite logs

`bdd_main!` installs a `tracing` subscriber (`bdd_infra::init_test_tracing`)
filtered to `warn,rusty_photon_tls=debug`, overridable with `RUST_LOG`.

Services under test are child processes whose stderr the suite already
forwards labelled (§5.1), so this subscriber only ever sees library code
running **in** the test process. It is deliberately near-silent, with one
exception: `rusty-photon-tls`'s server answers a connection it cannot
serve by dropping it and saying why at `debug!`, which is otherwise the
one failure the client cannot report — it just sees the socket die. Keep
new defaults to that bar: something a failing step cannot explain without.

#### 5.9 Steps that poll a service over HTTP must tolerate a failed fetch

[§5.7](#57-never-block-in-a-step--the-whole-suite-shares-one-poll-loop) is
the reason a loopback request from a step can stall for seconds: a
blocking step freezes the one poll loop every scenario shares, and an
in-flight response that had already delivered its headers sits unread
until the loop resumes. Concurrency compounds it — up to 64 scenarios,
most suites spawning a service process each, several suites at once via
`--local_test_jobs=HOST_CPUS*1.25`, on 4-vCPU Windows and macOS runners.

Fixing the blocking step is the cure; this section is the client side of
the same problem, and it holds regardless. Three rules for any step helper
that reads service state over HTTP (a `/debug/v1/*` introspection
endpoint, a health probe, a status poll):

1. **Retry the fetch, not just the assertion.** A helper that polls for
   a condition but panics on the first transport error is only half
   resilient — and the half it is missing is the one a starved poll loop
   exercises. Poll against a wall-clock deadline and treat a failed
   fetch as "not yet", keeping the last error so the final panic can
   report it. Distinguish *the condition never became true* from *the
   endpoint never answered*: they point at completely different bugs,
   and conflating them sends the next reader hunting for a product
   defect that isn't there.
2. **Share one client, and size its timeout for a starved runner.**
   `reqwest`'s `Client::builder().timeout(..)` is a whole-request
   deadline covering connect, headers *and* body, so a request that
   spends its budget before the body arrives surfaces as a body-read
   timeout even though nothing about the body was slow — which is how
   this failure disguises itself. Build the client once per suite (a
   `OnceLock`, or a field on the world) rather than per call: a fresh
   client per poll rebuilds a connection pool and pays a fresh TCP
   connect every iteration, which is exactly the cost that blows the
   deadline. `services/star-adventurer-gti/tests/bdd/world.rs`
   (`wait_for_command_log`) and `services/phd2-guider/tests/bdd/world.rs`
   (`http_client`) are the reference shapes.
3. **Make the budget a wall clock, not a poll count.** `for _ in 0..240 {
   … sleep(25ms) }` reads like a 6-second wall and is not one: each
   iteration also pays a full round trip, so the real budget is
   `240 × (25 ms + RTT)` — it *stretches* on a loaded runner, which
   sounds forgiving but means the number in the panic message is a lie
   and the real budget is unknowable from the source. Worse, the same
   shape used against work whose cost scales with the request gives
   every case the same nap count regardless: the three camera suites'
   old `wait_image_ready` allowed one 6.5-megapixel frame exactly what
   it allowed a 64×64 one. Take `Instant::now()` before the loop and
   compare `start.elapsed()` against a named budget each pass, then
   report *that measurement* in the panic alongside the budget — when
   the budget turns out to be the thing that was wrong, the elapsed
   time is the number you need, and reconstructing it from surrounding
   CI timestamps is guesswork. Size the budget for the slowest runner
   in the fleet, not the box you are typing on — CI's
   floor is a 3-core macOS runner carrying several BDD suites at once
   (`--local_test_jobs=HOST_CPUS*1.25`), where a wait that finishes in
   about a second locally has been measured at 7.5 s.
   `IMAGE_READY_BUDGET` in
   `services/{zwo,qhy,svbony}-camera/tests/bdd/world.rs` is the
   reference shape.

   Then check that budget against the target's own timeout: a per-wait
   deadline is only a diagnostic if the suite survives long enough to
   print it. A `rust_test` with `size = "small"` gets 60 s for *all* the
   scenarios it runs, so one scenario spending a 20 s budget can push
   the shard into an opaque Bazel `TIMEOUT` that names nothing. Size the
   target so the deadline fires first (the camera `bdd` targets are
   `size = "medium"` for this reason), and put the reasoning in a
   comment on the attribute — `services/rp/BUILD.bazel` and
   `services/session-runner/BUILD.bazel` are the precedent.
4. **A budget bounds a hang; it is not a timing assertion.** Sizing one
   as a small multiple of how long the wait *should* take reads like a
   contract and enforces nothing: no assertion fires when the wait comes
   in under it, so the only thing that number decides is how long the
   suite waits before reporting a hang. What it does buy is a
   per-scenario constant nobody can justify a month later, and the
   tightest of them going red first on the slowest leg.
   planetarium-bridge's three slew waits were 10 s, 3 s and 5 s over
   simulated slews of 1 s, 1 s and 2 s; the 3× one failed on the coverage
   leg while its 10× sibling — same 1 s slew, same poll, same run — never
   has (issue #1105). Prefer one named budget per suite, generous enough
   that the slowest leg never reaches it, with the target's own `size` as
   the outer bound. A generous budget cannot weaken an assertion it does
   not make; it only makes a genuine hang slower to report, and cucumber
   runs scenarios concurrently, so that cost is paid once by the suite
   rather than once per scenario. When a scenario really does need to pin
   how long something takes, assert *that*, and put the number in the
   feature file where
   [§2.5](#25-make-contract-constants-explicit-in-steps) can see it.

   One caveat that rule 2 already implies and this rule depends on: a
   wall-clock budget bounds a wait only if every call inside it can
   return. A client with no request deadline parks the whole wait inside
   one `await`, the elapsed check never runs, and the hang surfaces as
   the opaque target timeout the budget existed to prevent. Give the
   shared client a timeout, and the stalled call then lands in the
   wait's own last-error slot, which is where you want to read it.

#### 5.10 Assert the effect, not the timing, when a failure mode is "the OS killed it"

A process that is hard-killed dies *faster* than one that shuts down
gracefully. Any test that infers "we stopped it cleanly" from how long
the stop took therefore passes most loudly in exactly the case it was
meant to catch.

That is not hypothetical: `bdd-infra` stops services with the platform's
graceful signal, and on Windows `CTRL_BREAK_EVENT` went unregistered for
as long as the runner only handled Ctrl+C. Windows' default handler
terminated every service instead — promptly, with a normal-looking exit
and no error anywhere. The timing guard
(`test_stop_completes_via_graceful_signal`) was green throughout. The
only visible trace was an *absence*: the runner's shutdown log line
appeared 67 times in a local Linux run of one suite and 0 times across a
27k-line Windows CI job.

So when the thing under test is a shutdown, a cleanup, a flush, or a
teardown, assert on something the path itself produces — a marker file
the child writes from inside its stop handler, a persisted record, a
final log line — and prefer a fixture that goes through the **real**
production entry point. `bdd-infra`'s `test_service --graceful-probe`
runs `ServiceRunner` itself for that reason: a hand-rolled
`tokio::signal` await in the fixture would have kept passing no matter
which events the runner registered.

Corollary for cross-platform gaps: resist `#[cfg(unix)]` on a test whose
contract is cross-platform. Gating is right when the *fixture* needs a
Unix mechanism (see the `--epipe-probe` tests, which need a SIGTERM
handler to keep writing during shutdown); it is wrong when it merely
makes the suite green on the platform where the behaviour is broken.

---

### 6. Unit Test Rules

These rules apply to traditional `#[test]` and `#[tokio::test]` tests.

#### 6.1 One Test Function Per Behavior

Each test should verify exactly one behavior. Name the test `test_<component>_<behavior>`:

```rust
#[tokio::test]
async fn test_focuser_position_not_connected() { ... }

#[tokio::test]
async fn test_focuser_move_negative_position_rejected() { ... }
```

#### 6.2 Use `unwrap()` Over `assert!(result.is_ok())`

Per AGENTS.md Rule 7: prefer tests that fail with clear errors. `unwrap()` produces a message showing **what** the error was. `assert!(result.is_ok())` just says `false`.

**Good:**
```rust
let position = device.position().await.unwrap();
assert_eq!(position, 10000);
```

**Bad:**
```rust
let result = device.position().await;
assert!(result.is_ok());
```

#### 6.3 Test Both Success and Error Paths

For any operation that can fail, write separate tests for the success path and each meaningful failure mode. Assert the specific error code, not just that an error occurred.

```rust
#[tokio::test]
async fn test_focuser_position_connected() {
    // ... connect ...
    let position = device.position().await.unwrap();
    assert_eq!(position, 10000);
}

#[tokio::test]
async fn test_focuser_position_not_connected() {
    let err = device.position().await.unwrap_err();
    assert_eq!(err.code, ASCOMErrorCode::NOT_CONNECTED);
}
```

#### 6.4 Test File Organization

- `src/<module>.rs` `#[cfg(test)] mod tests` -- Unit and mock-based component tests (inline with source)
- `test_integration.rs` / `test_lib.rs` -- Server, run-loop, and CLI integration tests. Aim to keep these in one binary per service when practical so cargo links fewer integration targets; some services (phd2-guider, plate-solver) still split by concern where the categories don't share helpers.
- `conformu_integration.rs` -- ASCOM conformance (always `#[ignore]`)

#### 6.5 Mock Infrastructure Lives in Test Files

Hand-written mocks (`MockSerialReader`, `MockSerialWriter`, `MockSerialPortFactory`) are defined in the test files that use them. They are NOT feature-gated. The `#[cfg(feature = "mock")]` flag is reserved for the feature-gated `MockSerialPortFactory` in `src/` used by ConformU and server tests.

#### 6.6 Serialize Tests That Share Process-Global State

Tests that touch process-global state cannot run concurrently in the same
process. Guard each one with a shared static `Mutex<()>` held for the whole
test body. Two cases occur in this repo:

**Server tests that bind to ports** conflict with the discovery service:

```rust
static SERVER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn test_server_starts() {
    let _lock = SERVER_LOCK.lock().unwrap();
    // ... start server on port 0 ...
}
```

**mockall static-function / `#[automock]`-on-a-module mocks** (e.g. the FFI
mocks in `qhyccd-rs`, where `OpenQHYCCD_context()` and friends are generated
from `#[cfg_attr(test, automock)] mod libqhyccd_sys`). mockall stores every
`*_context()` expectation in **process-global** state, so two such tests
running on different threads in one process corrupt each other's
expectations — surfacing as `fragile` "destructor ran on wrong thread"
panics and `called 0 time(s) which is fewer than expected 1` failures that
abort the test binary (SIGABRT). Every such test must hold a shared guard as
its first line:

```rust
static MOCK_FFI_MTX: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Tolerate poisoning: these tests `assert!`/panic on failure, which would
// otherwise poison the mutex and cascade into spurious later-test failures.
fn mock_guard() -> std::sync::MutexGuard<'static, ()> {
    MOCK_FFI_MTX.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn open_succeeds() {
    let _mock = mock_guard(); // FIRST line, before any *_context() call
    // ... program OpenQHYCCD_context() etc. ...
}
```

`cargo nextest` (the project's standard Cargo test runner)
isolates each test in its own process and hides this — but plain
`cargo test` runs them as threads. That includes the nightly **safety**
sanitizer workflow and a developer running `cargo test -p qhyccd-rs`, both
of which abort without the guard. See issue #384 for the original failure.

#### 6.7 Mock Strategy: Hand-Written vs mockall vs axum stub

The project uses three mock strategies. The full rationale and the
empirical comparison that produced this rule live in
[ADR-004 — Testing Strategy for HTTP-Client Error Paths](../decisions/004-testing-strategy-for-http-client-error-paths.md).
The summary follows.

**Hand-written mocks** — Use for stateful device simulators that maintain internal
state across multiple calls. These mocks simulate hardware behavior: they process
commands, maintain device state (temperature, position, voltage), and return
responses from a queue. mockall cannot express this kind of stateful simulation.

Examples: `MockSerialPortFactory` in ppba-driver and qhy-focuser, which simulate
serial port communication with response queues and device state machines.

**mockall (`#[automock]`)** — Use for service-boundary traits where you need simple
"expect this call, return this value" behavior. These are thin abstractions over
external APIs (HTTP clients, DNS providers, ACME protocol) where the mock just
needs to return canned responses or verify call arguments. **This is the default
for HTTP-client error-path tests** — see ADR-004.

Examples: `HttpClient` trait in sentinel (mocks reqwest for `AlpacaSafetyMonitor`
and `PushoverNotifier`), `DnsProvider` trait in doctor's provisioning module
(mocks Cloudflare API), `AcmeClient` trait in the same module (mocks Let's
Encrypt ACME protocol).

**In-test axum stub server** — Use as the **escape hatch** when the production
code traverses a wide foreign trait surface that mockall would generate huge
boilerplate for. The qualitative threshold is roughly 10 methods on the trait
you'd need to mock: at the small end (3-method `HttpClient`), mockall is clean;
at the large end (76-method `ascom_alpaca::Camera`), it is not.

Spawn an axum server on `127.0.0.1:0` inside the test, configure routes that
return canned wire-format responses, point the production code at the bound
address. The existing `spawn_stub` helper plus extracted helpers with names
like `devices_body`, `ascom_body`, and `assert_disconnected` /
`assert_connected` (to be added when the pattern grows) keep per-test bodies
short.

Examples: `services/rp/src/equipment.rs::connect_*` (failure-branch tests
against the ascom-alpaca client); `services/sky-survey-camera/tests/bdd/world.rs`
(stateful behaviour switching for survey responses).

**How to use mockall with async traits:**

Place `#[automock]` before `#[async_trait]` on the trait definition, gated to
test builds only:

```rust
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait MyService: Send + Sync {
    async fn do_something(&self, input: String) -> Result<String>;
}
```

In tests, configure expectations on the generated `MockMyService`:

```rust
let mut mock = MockMyService::new();
mock.expect_do_something()
    .withf(|input| input == "expected")
    .returning(|_| Ok("result".to_string()));
```

**Decision rule for HTTP-client error paths:**

```
Stateful device simulator (multi-call state, command processing)?
└── Hand-written mock.

Otherwise: dependency surface ≤ ~10 methods?
├── Yes  → mockall + thin trait. Wrap only the methods used; gate
│         #[cfg_attr(test, mockall::automock)] on it; inject via Arc<dyn _>.
└── No   → axum stub server. Spawn axum on 127.0.0.1:0; extract helpers
          for repeated wire format; assert on the production-side outcome.
```

`httpmock` was evaluated and rejected — see ADR-004 for the empirical
comparison and the specific failure modes (silent `Content-Type`
omission, behaviour-switching regression, forced async propagation).

#### 6.8 Do Not Assert on Captured Tracing Events

**Never assert that a `warn!` / `info!` / `debug!` fired by installing a
thread-local subscriber (`tracing::subscriber::set_default` /
`with_default`) and counting captured events.** It is unsound in a parallel
test binary, and unlike §6.6 a mutex cannot fix it.

`tracing` caches each callsite's `Interest` in a **process-global** slot.
The first thread to reach a given `warn!` registers it, and — when one or
zero dispatchers are registered, the normal state in a test binary —
registration resolves the interest from *that thread's* default subscriber
(`tracing-core`'s `Rebuilder::JustOne` → `dispatcher::get_default`). If the
registering thread has no subscriber, `NoSubscriber::register_callsite`
returns `Interest::never()`, which is then stored **globally**. Every later
`warn!` at that callsite short-circuits on the cached `never` *before any
dispatcher is consulted* — including on a thread that does hold a live
`set_default` guard. The cache is only repaired by the next `Dispatch::new`.

So a test asserting "exactly one warn" fails with `left: 0` whenever some
*other* test in the same binary happens to touch the same callsite first
while carrying no subscriber. The failure is undercount-only,
platform-independent, load-sensitive, and hits just one of the asserting
tests per run. This was issue #949 (flake #946); the deterministic
reproduction is:

```rust
fn emit() { tracing::warn!("shared callsite"); }

let counter = /* a WARN-counting Layer */;
let _guard = tracing::subscriber::set_default(registry().with(counter.clone()));
std::thread::spawn(emit).join().unwrap();  // registers the callsite as `never`
emit();                                     // silenced, despite the guard
assert_eq!(counter.count(), 1);             // fails: left: 0
```

**Do this instead: assert the state machine, not the log.** Extract the
decision the log hangs off into a pure function, test it exhaustively, and
assert the observable state it drives. The log becomes an unasserted side
effect.

```rust
const fn is_limit_detect_rising_edge(previous: Option<bool>, current: bool) -> bool {
    match previous { None => current, Some(prev) => !prev && current }
}

if is_limit_detect_rising_edge(*last, status.limit_detect) {
    warn!("Falcon reported limit_detect after move toward {target:?}");
}
```

See `services/pa-falcon-rotator/src/manager.rs` (`edge_rule_tests` for the
rule, `read_status_records_each_limit_detect_observation` for the wiring).

Installing a process-global floor subscriber once per binary also suppresses
the flake, but it was **rejected**: it narrows the window rather than closing
it (`set_global_default` publishes `GLOBAL_INIT = INITIALIZED` only *after*
`Dispatch::new`'s interest rebuild, so a callsite first touched inside that
gap still caches `never`), it burns the binary's single `set_global_default`,
and leaving it in place would read as an endorsement of the capture pattern.

The same `enabled` gate has a second consequence that is not about
assertions at all — a computed field value never runs under test. See
§6.11.

#### 6.9 Testing a Negative: Watch a Window, Do Not Sample After a Nap

When the property under test is that something **does not happen** — a
decision *not* to act — there is no completion signal to wait on, because
nothing is produced. The tempting shape is to wait for the nearest related
signal, sleep a little to let the code under test run, and then assert once:

```rust
// WRONG: the nap is load-bearing for the test's ability to FAIL.
wait_for(some_related_signal).await;
tokio::time::sleep(Duration::from_millis(50)).await;
assert_eq!(device.camera_state().await.unwrap(), CameraState::Exposing);
```

The failure mode is asymmetric and therefore invisible. If the code under
test has not run yet when the sample is taken — a loaded runner, a slow
scheduler hop off the blocking pool — the assertion passes **whether or not
the bug is present**. A too-short nap never produces a failure; it produces a
green test that has silently stopped testing anything. Green stays green, so
nothing tells you the coverage is gone. And a loaded runner is exactly the
condition under which you want the regression test working.

Assert across a window instead, so the window itself is the detector:

```rust
// RIGHT: any violation at any point inside the window is a failure.
for _ in 0..100 {
    assert_eq!(
        device.camera_state().await.unwrap(),
        CameraState::Exposing,
        "the superseded capture's drain released the running exposure's slot"
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
}
```

A longer window can only make the bug easier to catch, never harder — the
opposite of a nap, which gets weaker as the machine gets slower. Give the
assertion a message naming the invariant, since the interesting failure is
about *which* state was observed, not about a bare equality.

Prove such a test earns its name: reintroduce the defect it targets and watch
it fail. For a nap-based test, also delay the code under test (a temporary
`sleep` right before it) to model a loaded runner — with the bug present and
its commit delayed 200 ms, the nap version of
`a_superseded_capture_does_not_release_the_new_exposures_slot`
(`services/zwo-camera/src/camera.rs`) passed, which is what prompted the
rewrite above.

Where the thing you want to wait on *can* be made observable instead, prefer
that over any timing: a simulation-only, read-only counter on the fake
backend is cheaper to reason about than a window, and turns the negative back
into a signal you can wait for.

#### 6.10 Reproducing a Nap Flake That Will Not Reproduce

A nap-based test reported flaky in CI will usually pass every time you run
it locally — hundreds of times, on the same commit. That is not evidence the
report was noise, and re-running is not a test of anything.

`#[tokio::test]` builds a **current-thread** runtime unless told otherwise.
The test task and the code under test share one thread and interleave only at
await points, so on an unloaded box the schedule is *deterministic*: the same
sample lands at the same place in the same cycle on every run. A flake of this
class is not a low-probability event you can wait for — it is a different
schedule you have to construct. Looping the test explores nothing, and loading
the machine is a blunt way to shift phase that mostly just adds noise.

Construct it instead, by injecting latency at the fake's I/O boundary — a
`sleep` in the test double's send/receive path, ideally behind an env var so
the knob is temporary and never ships:

```rust
async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
    // TEMPORARY: model a contended runner stretching each wire op.
    let delay_ms = match std::env::var("RP_WIRE_MS") {
        Ok(v) => v
            .parse::<u64>()
            .expect("RP_WIRE_MS must be an integer number of milliseconds"),
        Err(_) => 0,
    };
    if delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
    }
    self.state.lock().await.process_command(bytes);
    Ok(())
}
```

Note the `expect`. Unset means no delay, which is the normal path — but a
value that is *set and unparseable* must be loud, because the quiet
alternative is this section's own failure mode one level up: mistype
`RP_WIRE_MS=10ms`, get no latency injected, and watch a sweep come back clean
across every value. That reads as "the rewrite is load-independent" when it
actually means the experiment never ran.

This stretches the code under test *across* the sample point, which is the
condition a loaded runner produces and an idle one never does. Then sweep the
latency rather than picking one value — the interesting failures cluster in a
band, and a single guess usually misses it. On
`pause_background_polling_stops_wire_traffic_and_resumes_on_drop`
(`services/star-adventurer-gti/src/manager.rs`, issue #875) the sweep turned
one unreproducible CI sighting into a table: clean at 0 ms and 3 ms, then
failing at 5, 10 and 20 ms with the exact error CI had reported once, three
weeks earlier.

Keep the knob while you verify the rewrite, because it is what proves the new
test is load-independent rather than merely differently-timed: the window
version must stay green across the whole sweep *and* still fail across the
whole sweep once you reintroduce the defect. A rewrite verified only at 0 ms
has been checked in exactly the condition that never flaked.

Then revert it. The knob is scratch, not a feature: nothing in the tree ships
an env-var-driven sleep in a transport double, and a fake that can be slowed
from the environment is a fake that can be slowed *accidentally*, on CI, by a
stray variable. The sweep is finished before you push — check `git status`
and confirm the only files staged are the test you rewrote and its
documentation.

One trap when scripting the sweep: `cargo test`'s `--exact` matches the
**full** test path, so a bare function name matches nothing — and the binary
then prints `test result: ok. 0 passed`, which a `grep "test result: ok"`
reads as success. A sweep that silently ran zero tests looks exactly like a
sweep that passed.

Assert the count, not the word. Build the test binary once and drive it
directly — a sweep runs the test dozens of times, and going through
`cargo test` each iteration re-resolves the workspace for no benefit:

Placeholders are shell variables, not `<angle brackets>` — a bare `<crate>`
is input redirection, so a pasted snippet fails on the placeholder rather
than on anything you did:

```sh
CRATE=star-adventurer-gti                       # package name
CRATE_SNAKE=star_adventurer_gti                 # underscored: the binary's prefix
TEST=manager::tests::the_full_test_path         # full path, not a bare fn name

cargo test -p "$CRATE" --features mock --lib --no-run          # build once
BIN=$(ls -t "target/debug/deps/${CRATE_SNAKE}"-* | grep -v '\.d$' | head -1)

out=$("$BIN" "$TEST" --exact 2>&1)
echo "$out" | grep -q "test result: ok. 1 passed" || { echo "FAIL: $out"; exit 1; }
```

`ls -t | head -1` takes the most recently built binary, so build immediately
before the sweep — a stale binary from an earlier branch is another way to
sweep something that isn't the code under test.

`1 passed` cannot be satisfied by a filter that matched nothing, which is the
whole point — `ok` can.

#### 6.11 Compute Log Field Values Before the Macro, Not Inside It

**A value that a log macro's field expression computes normally runs in no
test at all.** Anything that does real work belongs in a `let` above the
macro; pass the binding as the field.

```rust
// WRONG: runs in no test, and first runs in production at 2 a.m.
debug!(elapsed_ms = start.elapsed().as_millis(), "frame arrived late");

// RIGHT
let elapsed_ms = start.elapsed().as_millis();
debug!(elapsed_ms, "frame arrived late");
```

A `tracing` event expands to `if enabled { ... valueset_all!(...) }` with the
field expressions *inside* the branch, and the workspace does not enable
`tracing`'s `log` feature, so the disabled arm expands to nothing at all.

`enabled` is false in a test binary for either of two reasons. A binary in
which no test installs a subscriber — the normal case, and what §6.8 asks for
— leaves `tracing`'s process-global max level at its initial `OFF`, so the
level check alone blocks every callsite before any interest is consulted. A
binary in which some test *does* install one raises that max level for the
whole process, so callsites at or above the installed level can run — but
which ones, in which tests, then turns on the registration order that makes
§6.8 unsound. Neither state is something to write code against: assume the
expression does not run.

Three consequences:

1. **Normally no test executes it.** A panic, a saturating conversion, a lock
   taken re-entrantly, an expensive call in a hot loop, or a `Display` impl
   that misbehaves stays invisible to the entire suite, and surfaces the
   first time an operator sets `RUST_LOG=debug` — which under
   [tenet 2](../workspace.md#project-tenets) is unattended, mid-session.
2. **Coverage counts the line as uncovered** — whatever the expression does,
   the zero-cost adapters below included. `codecov/patch` scores a
   percentage of the diff, so a couple of such lines in a large change are
   noise, while on a small one they can be the whole gap. Hoisting is a
   legitimate way to close that gap; a coverage exclusion never is (see
   [coverage.md](coverage.md)).
3. **The value is computed at a moment you did not choose** — after the
   subscriber's filtering work — which matters to anything reading global
   state. `std::io::Error::last_os_error()` is the clear case: read errno
   before the macro, or the subscriber runs between the failing syscall and
   the read (`send_sigterm` in `crates/bdd-infra/src/lib.rs`).

**Where the line is.** Hoist when the expression allocates, performs I/O or a
syscall, reads process-global state, or can fail or saturate. Leave it inline
when it is a borrow or a zero-cost `Display` adapter — `%path.display()`,
`%humantime::format_duration(d)`, `len = buf.len()` — where laziness is the
whole point and there is nothing that can go wrong. The trigger is the work
the expression does, not the coverage line it leaves: do not churn existing
adapter call sites to buy patch percentage.

---

### 7. Migration Strategy: From Integration Tests to BDD

When migrating existing integration tests to BDD:

1. **Group related tests into features.** Multiple `test_device_*` functions testing connection become scenarios in `connection_lifecycle.feature`.

2. **Extract the behavioral pattern.** A test like `test_focuser_position_not_connected` becomes:
   ```gherkin
   Scenario: Position read fails when disconnected
     Given a device that is not connected
     When I try to read the focuser position
     Then the operation should fail with a not-connected error
   ```

3. **Keep unit tests for protocol and serialization.** These have no behavioral story to tell -- they verify data encoding. BDD adds no value here.

4. **Keep property tests alongside BDD.** Property tests verify invariants across random inputs. BDD scenarios test specific, documented behaviors. They complement each other.

5. **Delete the migrated integration test** once the BDD equivalent is passing and covers the same behavior.

---

### 8. Naming Conventions Summary

| Element | Convention | Example |
|---|---|---|
| Feature file | `snake_case.feature`, named by concern | `safety_evaluation.feature` |
| Feature title | Title case, names the concern | `Feature: Safety evaluation rules` |
| Scenario title | Sentence case, states the outcome | `Scenario: Device starts disconnected` |
| Step definition file | `snake_case_steps.rs` | `connection_steps.rs` |
| Step function name | `snake_case`, describes the action | `fn monitoring_file_containing(...)` |
| Unit test function | `test_<component>_<behavior>` | `fn test_focuser_position_not_connected()` |
| Test data file | Descriptive name in test directory | `tests/config.json` |

---

### 9. Checklist: Adding a New Feature

When adding a new feature to a service:

1. Read the service's design document (`docs/services/<service>.md`)
2. Write feature file(s) describing the new behavior in Gherkin
3. Implement step definitions, reusing existing steps where possible
4. Add new fields to the World struct if needed
5. Write unit tests for any new protocol commands or serialization
6. Write property tests if the feature has invariants over arbitrary input
7. Run `cargo build --all --quiet --color never`, `cargo test --all --quiet --color never`, `cargo fmt`
8. Update the design document if behavior described there has changed

---

### 10. UI / Browser-Facing Service Testing (server-rendered HTMX apps)

**Applies to:** `ui-htmx` and any future server-rendered, HTMX-based
browser-facing service. The full design rationale (the Playwright evaluation,
the cross-cutting gotchas, the anticipatory-spike history) lives in the
archived [ui-htmx UI-testing plan](../plans/archive/ui-testing.md); this
section is the durable how-to.

#### 10.1 The model: three proof obligations, one BDD suite

A server-rendered HTMX app has no client state machine, virtual DOM, or
hand-written JavaScript — its interactivity is a thin, declarative contract:

```
browser-observable behavior = f( bytes the server sent , htmx.min.js , browser engine )
```

`htmx.min.js` is the same vendored file on every OS, and the browser engine is
the end user's device. So behavior can differ across *server* OSes only if the
*server's bytes* differ. That decomposes into three obligations, each with a
different tool — and **all three live inside the same cucumber-rs BDD
scenarios**, not a separate test type:

| # | Obligation | Question | Tool |
|---|---|---|---|
| P1 | Output correctness | Does the server emit the *right* markup + `hx-*` wiring? | `scraper` DOM assertions (§10.2) |
| P2 | Output OS-invariance | Is the output *identical* across OSes? | `insta` byte-equivalence snapshots (§10.3) |
| P3 | Output → behavior | Does real htmx *execute* it — swap lands, poll terminates, click works? | `thirtyfour` browser layer (§10.4) |

P3 establishes "this structure ⟹ this behavior" once; P2 establishes "every OS
emits that structure"; transitively, behavior is correct on every OS without a
browser on every OS. P1 independently proves the structure is *correct* (P2
alone would pass if every OS were identically wrong).

#### 10.2 Layer A — `scraper` DOM assertions (P1)

The everyday suite: runs on every OS leg via BDD, deterministically, no
browser. Replace `String::contains` substring checks with CSS-selector
assertions — a substring match is false-positive-prone and blind to attribute
order, boolean attributes, or malformed tags.

- **`!Send` discipline is load-bearing.** `scraper::Html` is `!Send` (it holds
  `Rc`s). Parse the borrowed HTML string, select, extract **owned** data, and
  drop the parsed tree — all inside a synchronous helper function — before
  returning to an `async` Then-step. Never store a parsed `Html`/`Selector` in
  a `Send` World struct or hold one across an `.await`. See
  `services/ui-htmx/tests/bdd/dom.rs` for the pattern: every helper takes
  `&str`, returns owned `String`/`bool`/a small owned struct, and is fully
  synchronous.
- **Thin DOM-driven request helpers, not an htmx simulator.** Build helpers on
  `reqwest` + `scraper` that *submit the rendered form* (read fields/hidden
  inputs and the actual `hx-post` URL from the HTML), *follow the rendered
  link* (its `hx-get` URL), and *poll* an endpoint discovered from the DOM —
  never hardcode a URL the server happens to use today. Send htmx's full
  request header set (`HX-Request`, `HX-Target`, `HX-Trigger`,
  `HX-Trigger-Name`, `HX-Current-URL`) so captured fragments match what a
  browser would actually send. Do **not** build a general model of
  `hx-swap`/`hx-target`/`HX-*` semantics — that's "testing your simulator";
  route real client-transformation behavior to Layer C instead.

#### 10.3 Layer B — `insta` cross-OS snapshots (P2)

Snapshot the **server response bytes** (full pages + `HX-Request` swap
fragments) captured by the existing non-browser BDD path — the
cross-OS-comparable artifact, since a browser DOM only runs on one OS and
reserializes what it received.

- **Explicit snapshot names.** `insta`'s auto-naming is murky inside a
  cucumber step; always name snapshots explicitly (see
  `services/ui-htmx/tests/bdd/snapshot.rs::assert_html`).
- **External `.snap` files**, not inline, under `tests/snapshots/` — readable
  diffs. Pin `*.snap text eol=lf` in `.gitattributes` (the CRLF hazard, not
  fixed by insta filters).
- **`add_filter` regexes scrub run-varying tokens** (ephemeral ports, temp
  dirs, correlation IDs) so one committed golden compares on every OS. Skip
  snapshotting output that is *inherently* OS-varying (e.g. an OS-specific
  `os error N` string in a connection-refused banner) — that's what Layer A's
  DOM check is for.
- **Runtime snapshot-path resolver, not a static BUILD.bazel string.**
  `INSTA_WORKSPACE_ROOT` can't be interpolated at Bazel analysis time. Resolve
  the snapshot directory at runtime from `TEST_SRCDIR`/`TEST_WORKSPACE`
  (Bazel runfiles) falling back to `$CARGO_MANIFEST_DIR/tests/snapshots`
  (Cargo) — see `snapshot_dir()` in `services/ui-htmx/tests/bdd/snapshot.rs`,
  which mirrors `services/ppba-driver/tests/translations.rs`'s
  `locate_i18n_dir()`.
- **Bazel wiring:** `data += glob(["tests/snapshots/**"])` on the `bdd`
  target so goldens reach the runfiles tree, and `env += {"INSTA_UPDATE":
  "no"}` — Bazel does not propagate `CI`, so compare-only must be forced
  explicitly (the sandbox is read-only anyway).
- **Updates are Cargo-local only.** `cargo insta review` / `accept`, then
  commit — never attempt to update a snapshot under Bazel/CI.

#### 10.4 Layer C — `thirtyfour` real-browser tests (P3)

A **small**, advisory set of scenarios for behavior only a real browser can
prove (a swap actually lands, a poller fires then terminates, streaming
teardown doesn't zero out coverage).

- **Gate with a cucumber tag + runtime env var, never a cargo feature.** Tag
  scenarios `@browser` and filter them out of the default suite unless
  `UI_BROWSER_TESTS=1` is set, in the same closure that filters `@wip` (see
  `docs/skills/testing.md` §2.7 and `services/ui-htmx/tests/bdd.rs`). A cargo
  feature would be flipped on by `--all-features` runs and drag browser flake
  into the required gate; `thirtyfour` itself stays an always-compiled
  dev-dep, which is harmless.
- **geckodriver is an external system tool**, exactly like OmniSim/ConformU:
  discover it via `GECKODRIVER_BINARY` (else `PATH`), spawn it on an
  **ephemeral** port, in **its own process group** (`process_group(0)` on
  unix) with `kill_on_drop`. The process group is what makes teardown
  tractable — geckodriver leads it, Firefox and its content processes inherit
  it, so the whole tree can be reaped with one `killpg` and an orphan check
  can be scoped to *that group* (it can never match a developer's own
  Firefox). See `services/ui-htmx/tests/bdd/browser.rs::BrowserSession`.
- **Teardown ordering is load-bearing:
  `driver.quit() → stop the service under test → stop its dependencies`.** A
  live browser session holds an open connection to the server; stopping the
  server first blocks its graceful shutdown, which costs the 5s SIGKILL grace
  and skips `atexit` — meaning **no `.profraw` is written and BDD coverage is
  silently lost** (the same hazard as §5.4, but the browser removes the
  in-process escape hatch: `driver.quit()` is the only lever). This is
  especially sharp for SSE/streaming endpoints, which never close on a
  shutdown signal (axum issue #2673) — an open SSE connection can block
  graceful shutdown for seconds where a plain request blocks for ~0.1s.
- **Poll, never snapshot-once.** Set the WebDriver implicit wait to zero and
  poll explicitly (bounded retries, no wall-clock `sleep` beyond the poll
  interval) for every assertion that depends on an async htmx swap or SSE
  push — a single point-in-time read races the swap. See `wait_present` /
  `wait_enabled` / `wait_text_contains` in `browser.rs`.
- **No async `Drop`.** `thirtyfour`'s `WebDriver` has none — always
  `driver.quit().await` explicitly; relying on `Drop` can deadlock the async
  executor mid-teardown.
- **Capture failure artifacts (screenshot + page source) before any
  reap/quit**, to an **absolute** path (Bazel's `TEST_UNDECLARED_OUTPUTS_DIR`
  when set, else the OS temp dir) — a path relative to the original cwd
  breaks under Bazel's package-dir chdir.
- **Single browser, single OS, by design — not a gap.** Firefox headless on
  Linux is the only environment exercised; cross-OS coverage rides Layer B
  (identical server bytes ⇒ identical behavior), and the process-group
  reaper/orphan scan is unix-only. Don't propose extending this to
  macOS/Windows or another engine without a concrete new behavioral gap to
  justify it.

#### 10.5 Gating & CI

- **Default/required suite** (Cargo + Bazel, every OS leg + the Pi): Layers A
  + B only. Deterministic, no browser, full server-byte coverage.
- **`@browser` stays advisory**, run on a dedicated nightly recording job
  (`schedule` + `workflow_dispatch`, modeled on `scheduled.yml`'s Miri job)
  with a `notify-on-failure` step that opens-or-updates a tracking issue
  rather than reopening a closed one.
- Promote a *specific* `@browser` behavior to required only once it's the
  **sole** proof for something Layers A/B structurally cannot see (OOB swaps,
  response-header retargeting, SSE) and it's been stable for a defined
  sustained-green window — don't promote the whole layer wholesale.

#### 10.6 What not to build

- **A hand-rolled htmx simulator** modeling `hx-swap`/`hx-target`/`HX-*`
  semantics — you'd be testing your simulator, with a re-validate-on-every-
  htmx-bump tax. Keep Layer A's request helpers thin; route real
  client-transformation behavior to Layer C.
- **In-process unit-test snapshots** of rendered pages — a second test
  re-rendering the page duplicates the behavioral contract and cuts against
  BDD-as-source-of-truth. Snapshots ride the BDD scenarios (§10.3).
- **Playwright** (or any browser runner requiring a Node.js toolchain) in this
  Rust/Bazel/`crate_universe`-single-source-of-truth workspace — re-evaluate
  only if an official, Node-free, v1.0+ Rust binding lands.
- **A browser on every OS**, or a cross-engine matrix, absent a concrete
  behavioral gap the single Linux/Firefox environment doesn't cover — see
  §10.4's last point.

---

## References

- [AGENTS.md](../AGENTS.md) -- Rule 7 (prefer `unwrap()`) and Rule 8 (test smallest functionality)
- [Pre-push skill](pre-push.md) -- Running the full CI quality-gate suite before pushing
- [ASCOM Alpaca reference](../references/ascom-alpaca.md) -- Protocol details for ASCOM device tests
- [ui-htmx UI-testing plan (archived)](../plans/archive/ui-testing.md) -- Full design rationale behind Section 10: the three-obligation model, the Playwright evaluation, the anticipatory-spike history
