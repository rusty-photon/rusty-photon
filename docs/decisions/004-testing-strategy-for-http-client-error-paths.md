# ADR-004: Testing Strategy for HTTP-Client Error Paths

## Status

Accepted

## Context

Across the workspace, multiple services and crates contain async functions
that construct an external HTTP client and walk through several fallible
steps before returning a result. The canonical example is
`services/rp/src/equipment.rs::connect_focuser` (and its three siblings
`connect_camera`, `connect_filter_wheel`, `connect_cover_calibrator`):

```rust
async fn connect_focuser(config: &FocuserConfig) -> FocuserEntry {
    let client = match build_alpaca_client(...) { ... Err => return disconnected ... };
    let devices = match tokio::time::timeout(5s, client.get_devices()).await {
        Ok(Ok(devices)) => devices,
        Ok(Err(_)) => return disconnected,   // network / parse failure
        Err(_) => return disconnected,        // timeout
    };
    let device = match find_at_index(...) { ... None => return disconnected ... };
    match device.set_connected(true).await { ... Err => return disconnected ... }
}
```

Each function has five failure branches (build client, list devices,
timeout, find at index, set connected). Multiplied by four `connect_*`
functions, that's ~20 failure branches in `equipment.rs` alone. The same
shape recurs in `services/sky-survey-camera/src/survey.rs::SkyViewClient`,
`services/calibrator-flats/src/mcp_client.rs`, `services/rp/src/session.rs`,
and elsewhere.

BDD scenarios exercise *some* of these branches by pointing services at
unreachable URLs (`http://localhost:1`), but injecting more interesting
failures (timeouts, malformed bodies, mid-stream errors, ASCOM-level
rejections) is awkward in BDD because Gherkin steps don't have natural
hooks for surgical wire-level manipulation. Unit tests are the right
tool for these branches.

The question this ADR answers: **what testing pattern do we standardize
on, workspace-wide, for HTTP-client error paths?**

A workspace audit (issue #111) catalogued every site:

- **Already covered with a small trait + `mockall`** (workspace convention
  per [`docs/skills/testing.md`](../skills/testing.md) §6.7):
  `sentinel::HttpClient` (3 methods, mocks both `AlpacaSafetyMonitor`
  and `PushoverNotifier`); `rp-tls::DnsProvider`/`CloudflareApi`
  (4 methods); `rp-tls::AcmeClient`; the `SerialPortFactory` traits in
  `ppba-driver`/`qhy-focuser`; `phd2-guider::ConnectionFactory`.
- **Already covered with an in-test `axum` stub server**: PR #109 added
  one to `services/rp/src/equipment.rs::connect_focuser`;
  `services/sky-survey-camera/tests/bdd/world.rs` uses one with
  mid-test behaviour switching for survey responses.
- **BDD-only with untested error branches**: `rp::session::start`
  (orchestrator POST), `sky-survey-camera::SkyViewClient::*`,
  `calibrator-flats::McpClient::*`, the spawned task in
  `sky-survey-camera::SkySurveyCamera::start_exposure`.

Three candidate patterns surfaced during PR #109's review and were
evaluated empirically by porting two real call sites between them:

1. **`mockall` + a small trait** wrapping only the methods used.
2. **In-test `axum` stub server** with hand-written wire-format
   responses.
3. **`httpmock` dev-dep** with a DSL for declaring mocks.

A fourth option — *no unit tests, rely on BDD* — is the de-facto current
state for the BDD-only sites and is what this ADR replaces.

## Options Considered

The empirical comparison was driven by two ports kept on spike branches
during evaluation (since deleted; their findings are reproduced below):

- `equipment.rs::connect_focuser` tests ported from axum to httpmock
  (stateless, multi-route).
- `sky-survey-camera`'s BDD `world.rs` survey stub ported from axum to
  httpmock (stateful, behaviour-switching mid-scenario).

### Option 1: `mockall` + thin trait

A small async trait wraps only the methods the production code calls,
and `#[cfg_attr(test, mockall::automock)]` generates a mock that tests
configure with `expect_method().returning(...)`.

**Example: `sentinel::HttpClient` (`services/sentinel/src/io.rs`):**

```rust
#[async_trait]
#[cfg_attr(test, mockall::automock)]
pub trait HttpClient: Send + Sync {
    async fn get(&self, url: &str) -> Result<HttpResponse>;
    async fn put_form(&self, url: &str, params: &[(&str, &str)]) -> Result<HttpResponse>;
    async fn post_form(&self, url: &str, params: &[(&str, &str)]) -> Result<HttpResponse>;
}

// In a test:
let mut mock = MockHttpClient::new();
mock.expect_get()
    .withf(|url| url.ends_with("/issafe"))
    .returning(|_| Err(SentinelError::Http("connection refused".into())));
let monitor = AlpacaSafetyMonitor::new(Arc::new(mock), config);
let result = monitor.poll().await;
// Assert that the http error mapped to "unsafe"
```

**Pros:**
- Most expressive failure-injection in the workspace. `.times(N)`,
  `.never()`, `.withf(|x| ...)` predicates, and sequence matchers cover
  almost every shape of test you'd want to write.
- Already a workspace dev-dep (`mockall = "0.14.0"`) used by several
  crates in the workspace.
- The trait itself is documentation: it pins down exactly which methods
  the production code depends on, making API-surface drift visible.
- No wire-format work — tests deal in domain types
  (`HttpResponse { status, body }`) rather than JSON envelopes.
- Compiles fast — `mockall`'s proc-macro expansion is cheap when the
  trait surface is small.

**Cons:**
- Requires the production code to be *parameterized* over the trait
  (e.g. `HttpClient: Arc<dyn HttpClient>`). This is a small refactor at
  each call site.
- Doesn't scale to wide foreign trait surfaces. The
  `ascom_alpaca::Camera` trait has 76 methods plus `Device`'s 12;
  generating a `MockCamera` that only ever calls `.set_connected(true)`
  is technically possible but proc-macro-heavy and produces a wall of
  irrelevant boilerplate. (See decision rule below.)
- The trait is a wrapper layer between production code and the real
  client; small one-time complexity.

### Option 2: In-test `axum` stub server

Spawn a real HTTP server on `127.0.0.1:0` inside the test. Tests
configure routes that return canned wire-format responses.

**Example (the pattern in `services/rp/src/equipment.rs` after PR #109):**

```rust
let app = Router::new()
    .route("/management/v1/configureddevices", get(|| async {
        Json(serde_json::json!({
            "Value": [{
                "DeviceName": "Focuser 0",
                "DeviceType": "Focuser",
                "DeviceNumber": 0,
                "UniqueID": "test-uid"
            }],
            "ErrorNumber": 0,
            "ErrorMessage": ""
        }))
    }))
    .route("/api/v1/focuser/0/connected", put(|| async {
        Json(serde_json::json!({ "ErrorNumber": 1024, "ErrorMessage": "..." }))
    }));
let stub = spawn_stub(app).await;
let entry = connect_focuser(&focuser_config_for(&stub.url())).await;
assert!(!entry.connected);
```

**Pros:**
- Same mental model as production code — handlers are plain
  `async fn` returning `Json(...)`. Reviewers don't context-switch.
- `axum = "0.8"` is already a workspace dependency for production code,
  so dev-time cost is zero.
- Full handler power: mid-test state mutation via `Arc<RwLock<State>>`
  is natural. `services/sky-survey-camera/tests/bdd/world.rs` uses this
  to flip the survey stub between `Ok` / `Status500` / `Hold` /
  `ServingFits(bytes)` / `Malformed` mid-scenario.
- No DSL ceiling — anything you can write in axum, you can write here.
- `Json(_)` extractor sets `Content-Type: application/json` correctly,
  matching ASCOM-Alpaca client expectations out of the box.
- Easy to debug — set a breakpoint inside the handler, get a normal
  stack trace.
- Composes with `tokio::test(start_paused = true)` for timeout tests:
  a handler that calls `std::future::pending().await` lets virtual time
  advance to the production-side `tokio::time::timeout`.

**Cons:**
- Verbose at low scenario count. ~30 LOC of harness boilerplate per
  test file (`AlpacaStub` struct, listener, `oneshot` shutdown, `Drop`
  impl), and each route is 5–10 LOC even after the harness exists.
- Wire format hand-written. Every Alpaca envelope
  (`Value`/`ErrorNumber`/`ErrorMessage`) is repeated in JSON literals.
  Mitigated by extracting helper functions (`devices_body`, `ascom_body`)
  but those are extra infrastructure.
- No built-in request assertions. "Was `set_connected` called once with
  `Connected=true`?" requires hand-rolling an `Arc<Mutex<Vec<…>>>`.
- No built-in call-count tracking. Same hand-rolling requirement.

### Option 3: `httpmock` dev-dep

A library that spawns its own mock server and exposes a builder DSL for
configuring routes and matching requests.

**Example (the equivalent of the axum stub above):**

```rust
let server = MockServer::start_async().await;
server.mock_async(|when, then| {
    when.method(GET).path("/management/v1/configureddevices");
    then.status(200)
        .header("content-type", "application/json")
        .json_body(serde_json::json!({
            "Value": [{
                "DeviceName": "Focuser 0", "DeviceType": "Focuser",
                "DeviceNumber": 0, "UniqueID": "test-uid"
            }],
            "ErrorNumber": 0, "ErrorMessage": ""
        }));
}).await;
server.mock_async(|when, then| {
    when.method(PUT).path("/api/v1/focuser/0/connected");
    then.status(200)
        .header("content-type", "application/json")
        .json_body(serde_json::json!({ "ErrorNumber": 1024, "ErrorMessage": "..." }));
}).await;
let entry = connect_focuser(&focuser_config_for(&server.base_url())).await;
assert!(!entry.connected);
```

**Pros:**
- Concise DSL for stateless multi-route tests. Each route is 3–5 LOC
  vs. 8–15 LOC for the equivalent axum.
- Built-in request assertions: `mock.assert_hits(1)`, automatic
  teardown verification.
- Built-in matchers for query strings, headers, JSON bodies, regex on
  paths. No hand-rolling.
- `MockServer` is reusable — one server can hold many mocks across a
  test binary.
- Sequenced responses (fail twice then succeed) without manually
  threading an `AtomicUsize`.

**Cons (discovered while porting):**
- **Silent `Content-Type` foot-gun.** `httpmock 0.8.3`'s
  `then.json_body(...)` does *not* set `Content-Type:
  application/json`, despite the method name. The `ascom-alpaca`
  client requires `Content-Type` (`Missing Content-Type header` error)
  and rejects responses without it — the mock is hit but the test
  fails with a misleading symptom. Workaround: explicitly call
  `.header("content-type", "application/json")` on every Alpaca-shaped
  mock, or extract a `json_alpaca(then, body)` helper. This is tribal
  knowledge that every Alpaca consumer would have to learn the hard
  way.
- **Stateful behaviour switching is awkward.** `httpmock` registers
  mocks once and matches first-match-wins. To change behaviour
  mid-test, you call `server.reset_async()` and re-register *all*
  mocks (including the unchanged HEAD mock). This produced a 10% LOC
  *increase* (498 → 548 LOC) when porting
  `sky-survey-camera/tests/bdd/world.rs` from axum, even though the
  axum version was the smaller, more direct expression of the same
  semantics.
- **Forced async propagation.** `set_stub_behavior()` was a sync
  `RwLock::write`; the httpmock equivalent has to `await`
  `reset_async` and `mock_async`, forcing four sync `#[given]` step
  functions in `tests/bdd/steps/exposure_survey_steps.rs` to become
  `async fn`.
- **Self-referential lifetime trap.** `Mock<'a>` borrows
  `&MockServer`, so storing both in the same struct (e.g. on the
  cucumber `World`) requires capturing `mock.id: usize` *before*
  moving the server. Trivial once you know it; a real friction the
  first time.
- **`MockServer` doesn't implement `Debug`.** Cucumber's `World`
  derive needs `Debug`, forcing a hand-written `impl Debug`.
- **Doesn't actually solve wire-format work.** The Alpaca JSON envelope
  is still hand-written inside `then.json_body(...)`. The library
  doesn't know about Alpaca, so the same `Value`/`ErrorNumber`/
  `ErrorMessage` boilerplate appears either way.
- **New dev-dep with non-trivial transitive cost.** Adding `httpmock`
  pulls in a second `reqwest` major version (0.12 alongside the
  workspace's 0.13) and ~140 lines of new `Cargo.lock` entries. Not
  blocking, but real.
- **Two HTTP mental models in tests.** Production uses `axum`;
  switching to `httpmock` for some tests means contributors and
  reviewers context-shift between two stub idioms.
- **DSL ceiling.** Anything stateful or computed-from-prior-request
  falls back to `delete()`-and-recreate. The escape hatch is uglier
  than just writing an axum handler with shared state.

### Option 4: BDD only, no unit tests

The de-facto status for the four BDD-only sites listed above
(`rp::session`, `sky-survey-camera::SkyViewClient`,
`calibrator-flats::McpClient`, `sky-survey-camera::start_exposure`).

**Pros:**
- Zero per-site cost.

**Cons:**
- BDD doesn't have a natural way to inject mid-call failures. You can
  point a service at `http://localhost:1` to test connection-refused,
  but you can't easily test "the server returns 200 with malformed
  JSON" or "the server times out after 4.9 seconds with a partial
  body." Those branches stay uncovered.
- Failure-branch coverage on BDD-only sites is patchy and hard to
  audit. Adding a new failure branch in production code lands without
  a test.

## Decision

We adopt a **two-tier convention** for HTTP-client error paths:

### Tier 1 (default): trait + `mockall`

When the production code's dependency on the external client is narrow
(roughly **≤ 10 methods**), define a small trait wrapping only those
methods, gate `#[cfg_attr(test, mockall::automock)]` on it, and inject
the trait via `Arc<dyn YourTrait>` into the production code.

**Use this for:** `rp::session::start` (single `reqwest::post`),
`sky-survey-camera::SkyViewClient::{health_check,fetch}` (3 methods),
`calibrator-flats::McpClient::{new,call_tool}` (rmcp `Peer` — wrap only
the methods used), and any new HTTP-boundary code.

**Promote `sentinel::HttpClient` to a workspace crate.** Today it lives
in `services/sentinel/src/io.rs`. The same trait shape (`get`,
`put_form`, `post_form` over `reqwest`) fits `sky-survey-camera` and
`rp::session::start` directly. Lifting it to (e.g.)
`crates/http-client-mockable` lets new sites adopt it with a
single `use` statement.

### Tier 2 (escape hatch): `axum` stub server

When the production code traverses a wide foreign trait surface that
makes Tier 1 impractical — currently only the
`ascom_alpaca::Client::get_devices() -> Vec<TypedDevice>` path with its
fat `Camera`/`Focuser`/`FilterWheel`/`CoverCalibrator` device traits
(76, 13, 7, and 12 methods respectively, plus `Device`'s 12) — fall
back to spawning an in-test axum server.

**Use this for:** `services/rp/src/equipment.rs::connect_*`
(canonical), and `services/sky-survey-camera/tests/bdd/world.rs`
behaviour-switching survey stubs (already in place).

**Document the pattern with helpers.** The repeating boilerplate is
small but real; canonical helpers belong alongside the first use site:

- `spawn_stub(router) -> AlpacaStub` — listener bind + shutdown
  wiring (~30 LOC, write once).
- `devices_body(device_type, n) -> serde_json::Value` — Alpaca
  configured-devices envelope.
- `ascom_body(error_number) -> serde_json::Value` — generic
  `{ErrorNumber, ErrorMessage}` shape.
- `assert_disconnected(&entry)` / `assert_connected(&entry)` — pin the
  expected outcome.

These helpers cut per-test bodies from ~25 LOC to ~10 LOC without
introducing a macro layer. A scenario macro on top is optional and
warranted only when scenario count grows past ~6 tests in a single
file (see Consequences).

### What we explicitly do *not* adopt: `httpmock`

The empirical comparison showed that `httpmock`'s narrow conciseness
advantage in stateless multi-route tests is approximately matched by
extracting shared helpers in axum, and is *more than offset* by:

1. The silent `Content-Type` failure mode that every Alpaca consumer
   would re-discover.
2. A measurable LOC *regression* in stateful behaviour-switching tests
   (the `sky-survey-camera` shape).
3. Forced async propagation that the axum equivalent doesn't require.
4. A second HTTP testing idiom in a codebase that already has axum in
   production.

`httpmock` would be a sensible default in a project without an existing
axum dependency or with mostly stateless single-route mocks; neither
condition holds for this workspace.

## Implementation

### Action items (follow-up issues)

This ADR is a doc-only change. The implementation work is tracked
separately:

1. **Lift `sentinel::HttpClient` into a workspace crate** (proposed
   `crates/http-client-mockable` or extension of `crates/rp-tls`).
   Update `sentinel` to import from the new crate. No behavioural
   change.
2. **Apply Tier 1 to the four BDD-only sites**, in roughly increasing
   order of effort:
   - `services/rp/src/session.rs::start` — single `reqwest::post`,
     trivial trait wrap.
   - `services/sky-survey-camera/src/survey.rs::SkyViewClient` —
     reuse promoted `HttpClient` trait.
   - `services/calibrator-flats/src/mcp_client.rs::McpClient` — wrap
     the rmcp `Peer` methods actually used (`call_tool`, `initialize`).
   - `services/sky-survey-camera/src/camera.rs::SkySurveyCamera::start_exposure` —
     spawned task; mock the `SurveyClient` trait it already depends on.
3. **Document the axum stub pattern in
   [`docs/skills/testing.md`](../skills/testing.md) §6.7**, including
   the helper extraction (`spawn_stub`, `devices_body`, `ascom_body`,
   `assert_*`). This ADR provides the rationale; §6.7 provides the
   how-to.
4. **No new dependencies.** Both `mockall` and `axum` are already in
   the workspace. The `httpmock` evaluation is closed.

### Decision rule (what to use when)

```
Is the production code's dependency narrow (≤ ~10 methods)?
├── Yes  → Tier 1: trait + mockall.
│         Define a small trait wrapping only the methods used;
│         #[cfg_attr(test, mockall::automock)] it; inject via Arc<dyn _>.
│
└── No   → Tier 2: axum stub server.
          Spawn axum on 127.0.0.1:0; write helpers (devices_body,
          ascom_body, spawn_stub, assert_*); compose tests from them.
          Add a scenario macro only when scenario count > ~6.
```

The only current Tier 2 site is the `ascom_alpaca::Client::get_devices`
walk in `equipment.rs`. Every other HTTP boundary in the workspace has
a narrow surface and belongs in Tier 1.

### What "narrow" means concretely

The trait surface includes only the methods the production code
actually calls — not the full external API. Examples from the existing
codebase:

| Trait | Methods | Wraps |
|---|---:|---|
| `sentinel::HttpClient` | 3 | `reqwest::Client::{get, put.form, post.form}` |
| `rp-tls::CloudflareApi` | 4 | the `cloudflare` crate (zone listing, TXT record CRUD) |
| `rp-tls::AcmeClient` | 2 | `instant_acme::{Account, Order}` |
| `phd2-guider::ConnectionFactory` | 1 | `tokio::net::TcpStream::connect_timeout` |

The `ascom_alpaca::Camera` trait, by contrast, exposes 76 methods. Even
if production code only calls `.set_connected(true)`, mockall generates
the full 76-method mock. That is the qualitative line where Tier 2
becomes preferable.

## Consequences

### What changes

- New ADR (this document) referenced from
  [`docs/skills/testing.md`](../skills/testing.md) §6.7.
- The four BDD-only sites identified above gain unit-test coverage for
  their failure branches via Tier 1 (`mockall` + trait), in a
  follow-up implementation issue.
- `sentinel::HttpClient` is lifted into a workspace crate to allow
  reuse by `sky-survey-camera` and `rp::session`.
- `services/rp/src/equipment.rs` may, in a separate refactor, extract
  its current axum stub helpers (`spawn_stub`, plus new
  `devices_body`/`ascom_body`/`assert_*`) and optionally introduce a
  scenario macro if the connect-test count grows past ~6 per device
  family. This is a quality-of-life refactor, not a correctness
  change.

### What doesn't change

- Existing `mockall`-based tests
  (`sentinel::AlpacaSafetyMonitor`, `PushoverNotifier`,
  `rp-tls::CloudflareDnsProvider`/`AcmeClient`, the serial-port factory
  traits) — they are already Tier 1 and stay as-is.
- The existing axum stub in `services/rp/src/equipment.rs` and the
  axum-based survey stub in
  `services/sky-survey-camera/tests/bdd/world.rs` — they are correctly
  Tier 2 and stay as-is.
- BDD remains the primary test type for service behaviour
  ([`docs/skills/testing.md`](../skills/testing.md) §1.1). Unit tests
  per this ADR cover *failure branches that BDD cannot inject
  surgically*; they complement BDD, they don't replace it.
- No new workspace dependencies. `mockall` and `axum` are already
  present.

### Risks

- **Tier 1 requires a small refactor at each call site** (introduce
  trait, parameterize production code over `Arc<dyn _>`). For the four
  identified sites this is straightforward; for any future site with
  more entanglement it might be larger. The risk is mitigated by the
  fact that Tier 2 remains an explicit escape hatch — there is no
  pressure to force-fit a wide foreign trait through Tier 1.
- **The two-tier rule depends on judgment** ("≤ ~10 methods"). The
  "Camera at 76 methods" anchor and the existing-precedent table above
  give the rule concrete grounding, but new external libraries may
  surface borderline cases. Reviewers should ask: "is the trait we'd
  generate readable, or is it a wall of irrelevant boilerplate?" If
  the latter, that's the signal to reach for Tier 2.

### Spike artefacts

The two empirical ports referenced in this ADR were performed on
spike branches that are not retained — only this ADR survives. The
findings (LOC deltas, the `Content-Type` foot-gun, the behaviour-
switching regression, the async-propagation cost) are reproduced
inline above so the rationale is self-contained.

## References

- Issue [#111 — Workspace-wide analysis: testing strategy for
  HTTP-client error paths](https://github.com/rusty-photon/rusty-photon/issues/111)
- PR [#109 — Phase 6a focuser primitives](https://github.com/rusty-photon/rusty-photon/pull/109)
  (introduced the in-test axum stub for `connect_focuser`)
- [`docs/skills/testing.md`](../skills/testing.md) §6.7 — Mock Strategy:
  Hand-Written vs `mockall` vs axum stub
- `services/sentinel/src/io.rs` — `HttpClient` trait, the workspace
  precedent for Tier 1
- `services/rp/src/equipment.rs` — Tier 2 precedent (in-test axum stub)
- `services/sky-survey-camera/tests/bdd/world.rs` — Tier 2 precedent
  (stateful axum stub with mid-test behaviour switching)
- [`mockall` crate](https://crates.io/crates/mockall) — workspace dep,
  `mockall = "0.14.0"`
- [`axum` crate](https://crates.io/crates/axum) — workspace dep,
  `axum = "0.8"`
