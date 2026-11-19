# Imaging Workflow DSL + `session-runner` Orchestrator Plugin

## Status

**Status: COMPLETE (archived 2026-07-10).** Delivered across Phases A–F:
design + schema PR #424; expression layer PRs #428 (spike) and #433;
engine core + `calibrator_flats.json` port PRs #439, #442, #445, #447,
#449; triggers/events/resume PRs #451, #453, #458 (+ rp safety recovery
PR #459); `deep_sky.json` PR #461 (planner gaps closed by rp PRs #466,
#468, #471; multi-instance OmniSim #470); Phase F (the
`workflow-documents.md` authoring guide, the `rp.md` § Orchestration
rewrite, `sky_flat.json`) together with rp-side startup recovery in the
archiving PR #486. Deliberately deferred, recorded below and in
`session-runner.md` § Limits of format version 1: Luau `script` nodes, container-scoped
triggers, parallel containers, sub-workflow imports, the `ui-htmx`
editor, typed array-element declarations, and retiring the Rust
`calibrator-flats` service (a separate decision after the port has
mileage); `deep_sky.json`'s guide/dither adoption tracks rp issue #464.

Direction decided 2026-07-01 after a research pass over the
astrophotography-automation ecosystem and the Rust workflow/DSL solution
space; Phases A–F all delivered (A/B design + expression layer, C engine
core + flats port, D triggers/events/resume, E deep-sky document, F
polish/adoption — the
[`workflow-documents.md`](../../references/workflow-documents.md) authoring
guide, the `rp.md` § Orchestration rewrite, and the `sky_flat.json` third
document). The rp-side *startup* recovery Phase D left deferred is also
implemented (`rp.md` § Session Persistence). The service design doc is
[`docs/services/session-runner.md`](../../services/session-runner.md); this plan
is the decision record and phase breakdown behind it, per the design → BDD →
implementation flow in
[`docs/skills/development-workflow.md`](../../skills/development-workflow.md).

## Motivation

`rp` deliberately contains no workflow logic (design tenet: orchestration is
a plugin concern). The docs envision four orchestrators — `deep-sky`,
`planetary`, `calibrator-flats`, `sky-flat` — of which only `calibrator-flats`
exists, as a hand-written Rust plugin. Before building the second one, decide
how workflow logic should be *expressed*, because the answer changes what we
build:

- **Hand-written Rust per session type** scales to a handful of first-party
  workflows but makes every new session shape (and every user adaptation —
  "same as deep-sky but refocus only on narrowband filters") a Rust PR.
- A **workflow definition format** lets one generic engine execute many
  workflows, lets power users author and share them as files, gives a future
  `ui-htmx` editor something to edit, and gives an LLM a validatable target
  format to generate.

## Research summary (2026-07-01)

Two research passes inform the decisions below.

### What the astro ecosystem converged on

Surveyed: N.I.N.A. (Advanced Sequencer + Target Scheduler plugin), Voyager
(DragScript + RoboTarget), ACP Expert (text plans + dispatch Scheduler),
KStars/Ekos (Scheduler + `.esq` capture queues), SGPro, RTS2 (professional).

1. **Every mature system is a three-layer hybrid**: an imperative *session
   procedure* (startup/shutdown/recovery choreography), a *cross-cutting
   reactive overlay* (meridian flip, refocus-on-HFR, park-on-unsafe — always
   triggers/conditions, never sequence steps), and a *declarative dispatch
   core* for target choice (targets + constraints + progress in a database, a
   planner asked "what now?" one step at a time). Every sequential-first
   product grew a dispatch layer (NINA → Target Scheduler, Voyager →
   RoboTarget); SGPro never did and lost its user base. Professional
   observatories (RTS2) are dispatch-only.
2. **Resume-after-crash is a property of the data model, not the runtime.**
   Ekos counts frames on disk; Target Scheduler/RoboTarget keep acquisition
   state in a DB. Nobody snapshots a running sequence.
3. **Every declarative system grew a scripting escape hatch in the dark**
   (NINA's Sequencer Powerups plugin is a programming language grafted onto
   the GUI; Home Assistant YAML and GitHub Actions `${{ }}` are the same
   cautionary tale from adjacent ecosystems). Design the expression story up
   front.
4. **Astronomers demonstrably author and share JSON instruction trees** —
   NINA's Advanced Sequencer (a JSON tree of instructions + triggers + loop
   conditions with community-shared templates) is its single most-cited
   feature.
5. **Workflow-level parallelism is marginal** (NINA's parallel container
   forfeits conditions/triggers; nobody else bothered). Concurrency in this
   domain is device-level (guiding while exposing), which `rp` tools already
   encapsulate.

Key sources: NINA Advanced Sequencer
(<https://nighttime-imaging.eu/docs/master/site/sequencer/advanced/advanced/>),
Target Scheduler (<https://tcpalmer.github.io/nina-scheduler/>), Voyager
DragScript (<https://wiki.starkeeper.it/index.php/DragScript>), ACP plan
format (<http://web.archive.org/web/20260210084339/http://solo.dc3.com/ar/RefDocs/HelpFiles/ACP81Help/planfmt.html>,
dead as of 2026-07 — linked via Wayback Machine),
Ekos Scheduler
(<https://kstars-docs.kde.org/en/user_manual/ekos-scheduler.html>), RTS2
dispatch scheduling (<https://arxiv.org/pdf/1005.1014>).

### What the Rust solution space says

Evaluated: plain Rust orchestrators; embedded scripting (Rhai, mlua/Luau,
Rune, Steel, Starlark, PyO3, rquickjs/Boa, Koto); declarative formats (Amazon
States Language-style state machines, statecharts, behavior trees, HTN/GOAP);
durable-execution engines (Temporal, Restate, LittleHorse, Obelisk/WASM, DIY
event-sourced replay); LLM-as-orchestrator.

1. **No embedded interpreter can snapshot a suspended script** (Rhai issue
   #769 open; Lua's Eris is dead). Crash-resume comes from either
   *event-sourced replay* (Temporal-style deterministic re-execution against
   a result log) or *re-deriving position from persisted domain state*
   (behavior-tree style re-tick against device state + a progress
   blackboard).
2. **A declarative reactive-tree DSL is the strongest primary format** for
   this project: instruction leaves map 1:1 onto the JSON-schema'd MCP tools
   (validation for free), triggers give reactivity, interpreter state is
   trivially persistable, it tests against the existing OmniSim + `rp` BDD
   stack, and the interpreter is a few thousand lines of owned,
   dependency-light Rust.
3. **If scripting is ever needed, Luau via `mlua` is the standout**: real
   async (a script awaiting a tool call is a coroutine yield the host can
   `select!` against safety events), industry-grade sandboxing, ~1–2 MB,
   very active. Rune has the best async syntax but a bus-factor risk; Python
   cannot be sandboxed (PyO3 says so explicitly) — and Python users already
   have an escape hatch, because orchestrators are separate processes: they
   can write an external MCP client in any language.
4. **Heavyweight engines don't fit a Pi 5 appliance**: Temporal's server is a
   multi-service Go deployment; Restate is a plausible single binary but
   another always-on stateful service that still doesn't solve
   user-authorability. The replay *pattern* is buildable in-process if ever
   needed.
5. **LLM-as-orchestrator is the wrong executor for unattended nights**
   (compounding per-step error over hundreds of steps) but a natural
   *author* of workflow documents — which a validatable declarative format
   enables safely.

Key sources: mlua (<https://github.com/mlua-rs/mlua>), Rhai checkpoint issue
(<https://github.com/rhaiscript/rhai/issues/769>), CEL spec + Rust
interpreter (<https://crates.io/crates/cel-interpreter>), Amazon States
Language (<https://states-language.net/>), BehaviorTree.CPP / Nav2
(<https://www.behaviortree.dev/>,
<https://docs.nav2.org/behavior_trees/overview/detailed_behavior_tree_walkthrough.html>),
Temporal Rust SDK status (<https://crates.io/crates/temporalio-sdk>), Restate
(<https://www.restate.dev/>), Obelisk (<https://obeli.sk/>), PyO3 sandboxing
statement (<https://docs.rs/pyo3/latest/pyo3/>), Home Assistant YAML
complexity backlash
(<https://homecore.tech/en/home-assistant-yaml-or-node-red/>), GitHub Actions
expression-language critique
(<https://www.iankduncan.com/engineering/2026-02-05-github-actions-killing-your-team/>).

## Decisions

| # | Decision | Rationale | Rejected alternatives |
|---|----------|-----------|----------------------|
| D1 | Workflows are **declarative reactive-tree documents in JSON**, executed by one generic **`session-runner`** orchestrator plugin (port 11171). | Matches the ecosystem-converged shape (procedure tree + trigger overlay + dispatch via `rp`'s planner); leaves validate against MCP tool schemas; authorable by power users now, a `ui-htmx` editor and LLMs later. | Embedded scripting as the *primary* format (no UI story, users must program); plain Rust per session type (every variation is a PR); YAML/KDL/TOML/custom text surface (weaker schema + round-trip tooling; JSON chosen deliberately). |
| D2 | **Rust orchestrators remain first-class.** The plugin protocol is unchanged and language-agnostic; `calibrator-flats` keeps shipping as Rust until its DSL port is proven equivalent, and future workflows may still be Rust when that fits better. | The DSL is an addition, not a replacement; the process boundary is also the escape hatch for users who prefer real languages (external MCP clients). | Mandating the DSL for all workflows. |
| D3 | **Escape hatch designed up front**: CEL-style **bounded expressions** in `when` / `until` / `set` / `$expr` positions in v1 — pure, total, no I/O, reading only the document's namespaces plus a single sanctioned clock read (`seconds_until()`, for dawn/flip math — tenet 4 in the design doc). The document schema **reserves a `script` node type** for sandboxed Luau handlers later, so scripting can land without a format break. | Avoids the accreted-expression-language failure mode (HA, GH Actions, NINA Powerups) while keeping v1 small; expressions stay pure so resume and UI round-tripping stay sound. | Pure-declarative v1 (vocabulary explosion, retrofitted expressions later); full Luau in v1 (second authoring surface before the first has users). |
| D4 | **Resume = re-derive from state.** On (re-)invocation the engine re-executes the document from the root against the persisted blackboard + live device state; documents must be written re-entrant (progress guards, `rp` planner progress counters, `once` markers). No replay log, no interpreter snapshots. | Matches how Ekos / Target Scheduler resume; rides `rp`'s existing session-persistence design; smallest machinery. Replay becomes necessary only if long-running scripts ever drive sessions — and the Luau-handler design (stateless per-event, state in blackboard) is chosen precisely so they don't. | Event-sourced replay (more machinery than v1 needs); interpreter snapshots (not implementable). |
| D5 | **Safety stays exclusively in `rp`.** A workflow document cannot express, delay, or override park-on-unsafe; the engine just gets its MCP session cancelled and is re-invoked with recovery context later. | Already an `rp` tenet; reinforced by the Home Assistant lesson (safety logic lives in the simplest, most reliable layer). | Safety triggers in documents. |
| D6 | **Dispatch is delegated to `rp`'s planner tools** (`get_next_target`, `record_exposure`, …). The DSL expresses *procedure* and *reaction*; target/filter *choice* stays a pure function in `rp`. | The ecosystem's dispatch cores are DBs + planners, not workflow syntax; `rp` already owns this. | Target-selection logic in documents. |
| D7 | **v1 proves the engine on two workflows**: the deep-sky session (new value) *and* a port of `calibrator-flats` (generalization proof, diffed against its existing BDD suite). | A single workflow can't show the format generalizes; the flats port has a known-good behavioral oracle. | Engine + deep-sky only; port-flats-first only. |

## Phases

Phase boundaries follow design → BDD → implementation per
[`development-workflow.md`](../../skills/development-workflow.md). Each phase is
committable on its own.

### Phase A — Design + document schema *(this PR)*

- [x] Research passes (ecosystem + Rust solution space) and decisions above.
- [x] Service design doc
      [`docs/services/session-runner.md`](../../services/session-runner.md):
      document format, instruction/trigger vocabulary, expression semantics,
      blackboard + re-entrancy contract, validation, configuration, MVP
      boundary.
- [x] Publish the workflow-document **JSON Schema**
      (`services/session-runner/schema/workflow-v1.schema.json`) — the
      contract for authors, the future UI, and LLM generation. Written by
      hand (the schema *is* the format spec); `schemars` is not the source
      of truth here because the schema outlives any one Rust representation.
      Validated (draft 2020-12) against both shipped golden documents and a
      battery of negative cases derived from the spec's own stated rules.

### Phase B — Expression layer

- [x] **Spike (time-boxed)**: pick the parser. An implementation-research
      pass (2026-07-02, verified against crates.io/GitHub) found that **no
      off-the-shelf interpreter implements the fixed semantics** — every
      live candidate deviates on f64-only numbers, strict
      null/division-by-zero, or grammar subsettability — so the evaluator
      is hand-written in every arm and the spike compares parser
      strategies:
      1. **Hand-rolled** lexer + Pratt parser (~800 LOC, zero deps;
         `miette` optional for dual human/structured diagnostics;
         `proptest` round-trip + `cargo-fuzz` as the test harness).
      2. **The `cel` crate as parser only** (the `cel-interpreter` crate
         was renamed `cel` at 0.11 — actively maintained, MIT, pure Rust,
         multi-maintainer): parse-only API with per-node spans and a
         public AST, but a deny-by-default AST walk must reject
         comprehension macros (expanded at parse time, no off switch) and
         rewrite int literals to f64, and its stock evaluator is unusable
         as-is (error-absorbing `&&`/`||`, IEEE-infinity float division,
         raise-on-missing-key).
      3. **`oxc_parser` JS-expression subset**: the specced grammar is a
         JavaScript expression subset and JS numbers are f64 — parse with
         oxc, allowlist AST node kinds plus lexical checks (comments, hex
         and `_`-separator literals share node kinds), evaluate strictly
         by hand. Cost: oxc's rapid 0.x release churn (pin + adapter).
      Evaluate: exactness of grammar subsetting (tenet 5 — anything the
      parser accepts becomes de-facto format), span quality mapped to
      JSON-Pointer `/validate` errors (incl. structured/serializable error
      types), error-message quality at 2 a.m., footprint, maintenance.
      Rejected up front by the same research: Rhai expression-mode (no
      `?:`, irremovable i64, silent cross-type comparisons), `evalexpr`
      (AGPL-3.0-only since v12), `zen-expression` (Decimal arithmetic,
      division by zero yields null), JEXL (no function-call grammar,
      silent Infinity), and the JSON-tree family (JsonLogic/jq/JMESPath/
      FEEL — lenient-null semantics and/or unreadable authoring contradict
      tenets 4–5).

      **Spike outcome (2026-07-03): hand-rolled lexer + Pratt parser.**
      All three arms were built against a shared target AST and evaluated
      with a 178-case conformance corpus derived from the design doc's
      § Expressions (including every golden expression from the shipped
      document examples), a cross-arm canonical-form agreement check, an
      error-message battery, a 30 000-input mutation robustness run, and
      footprint measurement. Results:

      | Criterion | hand-rolled | `cel` 0.14 parser | `oxc_parser` 0.138 subset |
      |---|---|---|---|
      | Conformance | 178/178 | 165/178 | 178/178 |
      | Transitive deps | 0 | 41 (ANTLR runtime, chrono, regex, nom, …) | 73 |
      | Clean release build | 0.4 s | ~6 s | ~12 s |
      | Panics under mutation | 0 | 0 | 0 |

      The `cel` arm is **disqualified on correctness**: its parser
      collapses runs of identical unary operators to a single application
      (`- -x` → `-x`, a silent sign flip; `!!x` → `!x`), invisible to any
      AST walk. Its 13 corpus deviations are structural, not fixable from
      the AST: no parenthesis nodes (so the non-chaining comparison rule is
      unenforceable and CEL's single-level relation grammar mis-groups
      `a == b < c`), `SourceInfo` spans attached only to errors (subset
      rejections cannot point at the offending token), lexical over-
      acceptance (`0x1F`, `007`, `.5`, `//` comments, `r'raw'`)
      indistinguishable in the AST, and ANTLR error text that actively
      suggests forbidden syntax (`expecting {'[', '{', … NUM_UINT, BYTES,
      'in'}`).

      The `oxc` arm matched the hand parser 178/178 — but only after four
      classes of wrapper lexical checks (string-aware comment pre-scan,
      trailing-input check because `parse_expression()` does not verify it
      consumed the source, `raw`-text re-validation of every number and
      string literal, source-slice sniffing for legal-JS trailing commas).
      That wrapper is ~450 LOC of exactly the kind of lexical code the
      hand parser consists of, on top of 73 dependencies and oxc's 0.x
      churn; operator-precise spans would need further work (node spans
      cover whole expressions).

      The hand-rolled arm is exact by construction, dependency-free, and
      produced the best diagnostics (span-exact, with fix hints like
      "`===` is not an operator here — use `==`"; "`=` … did you mean
      `==`? (expressions cannot assign; use a `set` instruction)") in
      766 spike LOC. Grammar details pinned during the spike are recorded
      in the design doc's § Expressions (JSON-syntax number literals,
      single non-chaining comparison level, the exact escape set, ASCII
      identifiers, reserved-word field names, no comments / unary runs /
      trailing commas). The spike corpus transfers as the seed conformance
      suite for the implementation step below.
- [x] Implement the expression module with exhaustive unit tests (every
      operator, every namespace, every error case), property tests
      (parse ↔ pretty-print round-trip), and a fuzz target (the parser
      must never panic on operator-authored input).

      **Done (2026-07-04):** `services/session-runner/src/expr/` — the
      spike's hand parser lifted and made panic-free, plus the hand-written
      strict evaluator over `serde_json::Value` with the engine clock
      injected (`EvalContext.now`). The spike's 178-case corpus transferred
      verbatim as the conformance suite; ~90 unit tests cover every
      operator/function/namespace/error class; proptest properties cover
      the print↔parse round-trip and no-panic totality; the cargo-fuzz
      target lives in `services/session-runner/fuzz/` (standalone
      workspace, `cargo +nightly fuzz run expr_parse`). Fuzzing immediately
      found unbounded parser recursion (deep `((((…` → stack overflow) that
      the property tests could not — fixed with a 64-level nesting cap and
      regression tests; a follow-up 5.9M-run session found nothing else.
      The evaluation pins this step fixed (runtime overflow raises at the
      producing operation, ordered comparisons are numbers-only, no
      truthiness, total path traversal) are recorded in the design doc's
      § Expressions.

### Phase C — Engine core + `calibrator-flats` port (the equivalence proof)

- [x] BDD scaffold for `session-runner` (rp-harness: OmniSim + `rp` +
      `session-runner`), `@wip`-tagged per testing.md §2.7 until green.

      **Done (2026-07-05):** `services/session-runner/tests/` — the same
      three-process shape as `calibrator-flats`' suite (`bdd_main!`,
      OmniSim device reset per scenario, `resources:omnisim:1` under
      Bazel). The runner carries the `@wip` filter for Phase D scenarios;
      the Phase C scenarios landed green in the same PR, so nothing ships
      tagged.
- [x] Engine core: document loading + schema validation, static tool-call
      validation against `tools/list`, `sequence` / `tool` / `set` / `if` /
      `repeat` / `try`-`catch`-`finally` / `fail` / `wait` / `log`
      execution, blackboard with
      atomic persistence, MCP client, `/invoke` + `/health` + `/validate`
      routes, completion posting.

      **Done in three slices (all 2026-07-05):** the document layer
      (typed model, schema-layer validation, parameter binding,
      workflow-name resolution — PR #439); the interpreter + blackboard
      (tree execution against the `ToolClient`/`Clock` seams, atomic
      persistence, `once` bookkeeping, the terminated-session path —
      PR #442); and the service wiring (rmcp MCP client, layer-2 catalog
      validation, `/invoke` + `/validate` + `/health`, completion
      posting, `main.rs` per service-lifecycle). Until Phase D, declared
      triggers do not fire (warned at run start) and `wait.until_event`
      raises a workflow error. Packaging (deb/rpm metadata + `pkg/`
      scripts per the service-packaging plan) is a follow-up once the
      first-party documents ship.
- [x] `rp`: forward the orchestrator registration's `config` object
      verbatim in the `/invoke` POST (the protocol addition documented in
      `rp.md` § Orchestrator Invocation Protocol). The invoke body now
      always carries a `config` key — the registered object verbatim, or
      `null` when the registration has none — pinned by the
      session-lifecycle BDD scenarios.
- [x] `workflows/calibrator_flats.json` shipped as a first-party document.

      **Done (2026-07-05):** `services/session-runner/workflows/`. Shipping
      it settled the filter-plan question the design left open: `rp`
      invokes the orchestrator exactly once per session, so the plan is an
      **`array` parameter** — a minimal format addition (`array` joins the
      parameter type enum; elements stay opaque, typed element
      declarations remain deferred) — iterated with a blackboard index and
      a `while has(params.filters[session.filter_index])` gate. The
      golden-document unit suite walks `workflows/` (validation walk +
      published schema), and the validation corpus embeds the shipped file
      so the engine's exec tests execute the real artifact.
- [x] Port `calibrator-flats`' BDD scenarios to run against `session-runner`
      executing that document; behavior must match the Rust plugin (same
      events, same frame counts, same cleanup-on-failure). The Rust
      `calibrator-flats` service is **not** retired in this plan — retirement
      is a separate decision after the port has real-world mileage.

      **Done (2026-07-05):** both scenarios ported
      (`services/session-runner/tests/features/flat_calibration.feature`)
      and green: the session completes back to `idle` and a 2 × 2 plan
      emits ≥ 4 `exposure_complete` events — the oracle suite's exact
      assertions. The call-sequence equivalence BDD cannot assert
      (per-filter exposure reset, no rescale once converged,
      cleanup-on-failure ordering) is pinned by the engine's exec tests
      running the shipped document against `rp`-faithful mock results.

### Phase D — Triggers, events, resume

- [x] SSE event client (`/api/events/subscribe`, `Last-Event-ID` = the
      envelope's `event_seq`).

      **Done (2026-07-05):** `services/session-runner/src/events.rs` — the
      workspace's house SSE-client pattern (chunked `reqwest` + hand
      parser, as in sentinel's watchdog and bdd-infra), reconnecting every
      1 s with the cursor, `stream_gap` logged-and-continued. The intake
      opens per session run before the first instruction and closes with
      it; the engine gained the `EventIntake`/`EngineEvent` seam and a
      monotonic `Clock` reading, and `wait.until_event` is implemented
      against it (buffered-since-run-start matching, final check at
      expiry, sleep-measured budget — pins recorded in the design doc's
      § `wait` / § Event Subscription). BDD: `events.feature` runs
      purpose-built fixture documents (`tests/fixtures/workflows/`,
      merged into the spawned service's `workflows_dir`) proving the
      buffered-event wait and the timeout-ends-the-session path
      end-to-end.
- [x] Trigger engine: `event`, `poll`, and the synthetic
      `correction_requested` sources; `when` gating and `while`
      phase-gates; safe-point interleaving; `once` / `cooldown`.

      **Done (2026-07-05):** the safe-point pump in
      `services/session-runner/src/engine/exec.rs` — evaluation after
      every instruction and continuously during waits, queued firings in
      document order, `when` at queue time / `while` at fire time,
      `once`/`cooldown` bookkeeping under `session._triggers.<id>` (recorded
      on successful action completion), poll sources due one interval
      after run start with gated-skip and debug-skip-on-failure, and
      synthetic `correction_requested` from `aborted` /
      `blocked_by_correction` / `pending_correction` tool results. Because
      the pump now consumes events, `until_event` matching moved to
      per-name unconsumed-occurrence counters (pins in the design doc's
      § Triggers). Landing the BDD suite (`triggers.feature`: interleaving
      by SSE seq order, `once`, cooldown, poll) also surfaced and fixed a
      Phase D1 latent race: `events::subscribe` now completes its initial
      connect inline, before `/invoke` acknowledges — previously the
      first instruction could outrun the subscription and lose its
      events.
- [x] Resume: recovery invocation → blackboard reload → re-execution;
      BDD scenarios for kill-mid-session → re-invoke → session continues
      without repeating completed work (frame counts prove it).

      **Done (2026-07-06):** the engine machinery already existed
      (recovery `/invoke` → `Blackboard::load`, eager replace for fresh
      sessions, `params._recovery.*`, Terminated-keeps-blackboard); this
      slice proves it end-to-end. `recovery.feature` runs a purpose-built
      re-entrant fixture (`recovery_capture_loop`: once-marked filter
      setup + a capture loop counting frames in the blackboard): SIGKILL
      the engine mid-loop → restart → re-invoke with recovery → exposure
      totals stay ≤ plan + 1 and the once marker is not re-run; and an
      rp outage mid-loop → the request-level MCP failure terminates the
      run (service healthy, blackboard kept) → re-invoke against the
      restarted rp captures exactly the remaining frames. The scenarios
      POST `/invoke` directly because rp's recovery re-invocation
      machinery did not exist at the time.

      **Follow-up (2026-07-07):** rp's safety-recovery machinery is now
      implemented (rp.md § Safety): SafetyMonitor polling, unsafe →
      MCP-session termination + `/mcp` 503 gate + session `interrupted`,
      safe → re-invocation with `recovery.reason = "safety_interruption"`,
      and a failed `/invoke` now returns the session to idle
      (`session_stopped` / `orchestrator_invoke_failed`) instead of
      wedging it active. `recovery.feature` gained the end-to-end safety
      interruption + resume scenario through rp's real machinery; rp's own
      `safety.feature` pins the rp-side contract.

      **Follow-up (2026-07-10):** rp-side *startup* recovery is now
      implemented too (with Phase F, same PR): the session registry +
      planner counters persist to `session.session_state_file`
      (atomically, on every transition and after every
      `record_exposure`), and on startup rp restores them and
      re-invokes the orchestrator with `recovery.reason = "rp_restart"`
      (rp.md § Session Persistence). Pinned by rp's
      `startup_recovery.feature` and, end-to-end against a real engine,
      by the rp-restart scenario in `session-runner`'s
      `recovery.feature`.

### Phase E — Deep-sky workflow

- [x] `workflows/deep_sky.json`: startup → dispatch loop
      (`get_next_target` → slew → `center_on_target` → `auto_focus` →
      guide → capture loop with dither/refocus/meridian-flip triggers) →
      shutdown, per the flow sketched in `rp.md` § Orchestration.

      **Done (2026-07-07):** shipped as the second first-party document,
      written against `rp`'s *implemented* tool surface — no
      guide/dither steps (no guider tools exist), the exposure plan as
      parameters (`get_next_target` v1 returns no filter/duration), and
      blackboard-side frame counting (no `record_exposure`). Triggers:
      `refocus-after-frames` (every-N, parameter-disabled by default),
      `refocus-on-hfr-degradation` (measure_basic + factor, 15 m
      cooldown), `flip-when-due` (30 s `get_meridian_status` poll). The
      dispatch loop is re-entrant with zero `once` markers, and a
      recovery invocation nulls `session.target_name` so the resumed
      run re-acquires (re-slew/center/focus). Design details in
      `session-runner.md` § `deep_sky.json`.
- [x] BDD scenarios against OmniSim for the full night-cycle happy path,
      target switch, refocus trigger, and safety interruption + resume.

      **Done (2026-07-07):** `deep_sky.feature` — five scenarios,
      including a bonus meridian-flip one (a due flip re-slews between
      exposures, never during one). The planner gates on real ephemeris
      at wall-clock now, so the harness computes the sky to fit the
      clock (`bdd_infra::rp_harness::NightSky`): an equatorial site at
      the anti-solar longitude is always in deep astronomical night,
      and dec-0 targets placed by hour angle sink at a constant
      0.25°/min — making "target drops below its floor in N seconds"
      exact (the target-switch lever). The harness also teaches OmniSim's
      telescope the computed site (rp hard-errors on a mount/config site
      mismatch) and syncs it onto the first target so document slews
      stay sub-degree.
- [x] Any `rp` planner v1 gaps this exposes (documented in `rp.md` § Dynamic
      Planner) get issues filed — closing them is `rp` work, not
      `session-runner` work.

      **Done (2026-07-07, filed 2026-07-08):** four gaps the document
      had to design around, each an `rp` issue: `get_next_target`
      returns no exposure plan (#462 — since closed: the planner now
      returns the first `exposures[]` entry and the document falls
      back to its parameters); no `record_exposure` /
      `get_session_progress` (#463 — since closed: rp tracks
      per-target/per-filter progress, rotates plans, balances tied
      targets, ends the session when every goal is met, and the
      document records each light frame and follows the rotation with
      a per-pass filter-change step); no guider tools (`start_guiding` /
      `stop_guiding` / `dither`) (#464); `end_of_session` unreachable
      (#465 — since closed: the planner reads the Sun's trend to tell
      dawn from dusk and the document dropped its frames-captured dawn
      heuristic). All are
      also recorded in `session-runner.md` § `deep_sky.json`
      ("v1 adaptations") with what the document does meanwhile and
      what changes when each lands.

### Phase F — Polish and adoption

- [x] Authoring documentation (`docs/references/workflow-documents.md`):
      the format, the expression grammar, the re-entrancy contract, worked
      examples.

      **Done (2026-07-10):** the author-facing reference — how a
      document runs, the instruction vocabulary, trigger contract, the
      expression rules that bite (null loudness, no truthiness,
      non-chaining comparisons, the mod-24 idiom), the re-entrancy
      idioms (guarded init, totals-traversal, index-marker reset,
      recovery branching), `/validate` as the authoring loop, and the
      three shipped documents dissected as worked examples, closing
      with an authoring checklist. Indexed in `workspace.md`
      § References.
- [x] Update `rp.md` § Orchestration to describe `session-runner` as the
      home of the deep-sky and sky-flat workflow documents (the Phase A
      edit only cross-references the plan there).

      **Done (2026-07-10):** the orchestrator table now says what each
      workflow *ships as* (deep-sky and sky-flat as documents,
      calibrator-flats as both the Rust service and its document port,
      planetary unbuilt), and the "how orchestrators are built" prose
      names `session-runner` as the home of the first-party documents,
      linking the authoring guide. The lifecycle diagram gained the
      rp-restart recovery stanza alongside the safety one.
- [x] Example documents beyond the two v1 workflows (sky-flat is the natural
      third: it exercises the expression layer's convergence-loop ceiling).

      **Done (2026-07-10):** `workflows/sky_flat.json` — twilight sky
      flats at the zenith (RA from `get_local_sidereal_time`, dec from a
      `site_latitude_degrees` parameter), per-frame rescale-always
      adaptation with in-band accept/discard, operator exposure bounds
      intersected with the camera's, dusk/dawn twilight-window closure
      at the clamp rails, and graceful partial completion
      (`report.window_over`). The adaptation fits the bounded
      expressions — the sky-flat ceiling test passed without needing
      `script` nodes. Design in `session-runner.md` § `sky_flat.json`;
      pinned by seven engine exec tests against scripted medians
      (rescale-always, discard, both window closures, budget fallback,
      index-marker resume) plus `sky_flat.feature` end-to-end against
      OmniSim (zenith slew from live LST, exact frame counts, park),
      and embedded in the validation corpus like its siblings.

### Deferred (explicitly out of scope for v1)

- **Luau script nodes** (`{"script": …}` is reserved in the schema; handlers
  will be stateless per-event with state in the blackboard, so D4 resume
  survives their arrival).
- **`ui-htmx` workflow editor** (the JSON Schema is designed to round-trip;
  building the editor is a `ui-htmx` plan).
- **LLM-authored documents** (nothing to build here beyond the schema — an
  LLM generates a document, `/validate` checks it; operational guidance
  belongs in authoring docs).
- **Parallel containers** (research: marginal value, real complexity;
  device-level concurrency already lives inside `rp` tools).
- **Sub-workflow imports / template composition** (wait for real duplication
  pressure across shipped documents).
- **Event-sourced replay** (only if long-running scripts ever land).

## Open questions

- Whether the document/engine layer should later extract into an `rp-*`
  crate for reuse (e.g. by a future `sky-flat`-specialized runner). Per the
  workspace convention, it stays service-local until a second consumer
  exists.
- Exact expression function set beyond the v1 list in the design doc —
  grows with worked examples, gated by the purity rules.
- ~~Numeric edge semantics to pin during Phase B~~ — resolved by the
  Phase B implementation step (2026-07-04): runtime f64 overflow raises at
  the **producing operation** (so every number in the system stays finite
  and `set` can always persist), and string ordered comparison is a **type
  error** (`'a' < 'b'` raises; `==`/`!=` are the string comparisons). Both
  recorded as evaluation pins in the design doc's § Expressions.
- Document versioning policy beyond `"version": 1` (pre-1.0 stance: breaking
  changes to the format are acceptable; the field exists so the engine can
  reject documents it doesn't understand with a clear error).
