# session-runner — Generic Workflow Orchestrator Design

## Overview

`session-runner` is an orchestrator plugin that executes **workflow
documents** — declarative JSON descriptions of an imaging session — against
`rp`'s MCP tool catalog. One generic engine replaces the need for a
hand-written Rust orchestrator per session type: the deep-sky night, the
flat-calibration run, and the twilight sky-flat session become documents,
not binaries. Rust orchestrators (such as today's `calibrator-flats`)
remain first-class citizens of the unchanged plugin protocol; the DSL is an
addition, not a replacement.

Decision record and phase plan:
[`docs/plans/archive/workflow-dsl.md`](../plans/archive/workflow-dsl.md).

### Tenets

1. **Documents describe procedure and reaction, never choice or safety.**
   Target/filter selection is delegated to `rp`'s planner tools; safety
   enforcement belongs exclusively to `rp` and cannot be expressed, delayed,
   or overridden by a document.
2. **Position is derived, not stored.** The engine never persists "where it
   is" in the tree. Resume re-executes the document from the root against
   the persisted blackboard and live device state; documents are written
   re-entrant (see [Re-entrancy Contract](#re-entrancy-contract)).
3. **Everything validates before anything moves.** A document is checked
   against the workflow schema, and every tool call in it is checked against
   `rp`'s live tool catalog, before the first instruction runs. Authoring
   errors surface at load, at the `/validate` endpoint, or at session start
   — never in the middle of the night.
4. **Expressions are pure — with one sanctioned exception.** The bounded
   expression language reads document state and computes values. It cannot
   perform I/O, call tools, or loop. The single sanctioned observation of
   the outside world is `seconds_until()`, which reads the engine clock at
   evaluation time — dawn/flip math is impossible without it (see
   [Expressions](#expressions)). Everything else effectful is an
   instruction.
5. **The document format is the API.** The published JSON Schema is the
   contract for hand-authors, the future `ui-htmx` editor, and LLM
   generation alike. The engine's internals may change; the format versions
   deliberately.

## Architecture

`session-runner` is a standalone HTTP service following the orchestrator
plugin protocol defined in [`rp.md`](rp.md): `rp` POSTs `/invoke` when a
session starts, the plugin connects back to `rp`'s MCP server and drives the
session with tool calls, and posts a completion when it finishes. In
addition, `session-runner` subscribes to `rp`'s SSE event stream to feed
trigger evaluation.

```
  rp (equipment gateway)              session-runner (generic orchestrator)
  ┌─────────────────────┐             ┌──────────────────────────────┐
  │                     │ POST /invoke│  load document               │
  │  session start ─────┼────────────►│  validate vs schema + tools  │
  │                     │             │  load/restore blackboard     │
  │  MCP server    ◄────┼─────────────┤  execute procedure tree      │
  │  /mcp               │  tool calls │   ├─ instructions            │
  │                     │             │   └─ trigger actions         │
  │  SSE            ────┼────────────►│  evaluate triggers           │
  │  /api/events/       │   events    │                              │
  │  subscribe          │             │                              │
  │                     │             │                              │
  │  REST API      ◄────┼─────────────┤  post completion             │
  │  /api/plugins/      │  completion │                              │
  │  {wf_id}/complete   │             │                              │
  └─────────────────────┘             └──────────────────────────────┘
```

### Port

11171 (configurable) — in the orchestrator-plugin range next to
`calibrator-flats` (11170).

## Workflow Documents

A workflow document is a single JSON file. Top-level structure:

```jsonc
{
  "version": 1,
  "name": "deep-sky",
  "description": "Classic multi-target deep-sky imaging session",
  "parameters": {
    "camera_id":   { "type": "string", "required": true },
    "focuser_id":  { "type": "string", "required": true },
    "dither_every": { "type": "integer", "default": 3 }
  },
  "estimated_duration": "8h",
  "max_duration": "12h",
  "triggers": [ /* trigger objects — see Triggers */ ],
  "root": { "sequence": [ /* instructions — see Instructions */ ] }
}
```

| Field | Meaning |
|-------|---------|
| `version` | Format version. The engine rejects documents whose version it does not implement, naming the supported version(s) in the error. |
| `name` / `description` | Identification for logs, events, and the completion payload. |
| `parameters` | Declared invocation parameters. Each has a `type` (`string`, `integer`, `number`, `boolean`, `duration`, `array`), and either `required: true` or a `default`. Values supplied at invocation are type-checked against the declaration; missing required parameters fail validation before anything runs. Available to expressions as `params.*`. Names beginning with `_` are reserved for the engine — declaring one is a validation error. An `array` parameter is an opaque JSON array in v1 — no element shape is declared, so element errors surface as loud expression errors at run time (typed element declarations are deferred; see [MVP Scope](#mvp-scope)). Durations stay humantime strings — expressions read them via `seconds()`. |
| `estimated_duration` / `max_duration` | The acknowledgment durations returned to `rp` from `/invoke` (humantime strings). Optional; engine defaults apply when absent (see [Invocation](#invocation)). |
| `triggers` | Document-global reactive rules, evaluated alongside the procedure tree. |
| `root` | The procedure tree — conventionally a `sequence` container, though any instruction is structurally valid as the root. |

### Instructions

The v1 instruction vocabulary. Every instruction is a JSON object with
exactly one *discriminant* key (`tool`, `sequence`, `repeat`, `if`, `set`,
`try`, `fail`, `wait`, `log` — plus `script`, reserved but rejected in v1)
plus the optional common keys `id` (a string used in
logs and error messages) and `once` (see
[Re-entrancy Contract](#re-entrancy-contract)). Unknown keys are a
validation error — misspellings must not silently no-op.

#### `tool` — call an MCP tool

```jsonc
{ "tool": "capture",
  "args": { "camera_id": { "$expr": "params.camera_id" },
            "duration": "300s" },
  "retry": { "max_attempts": 3, "backoff": "10s" } }
```

- `args` values are **literal JSON by default**. A value that must be
  computed is wrapped as `{ "$expr": "<expression>" }`. This keeps literal
  strings (humantime durations, filter names) unambiguous and lets static
  validation type-check every literal argument against the tool's JSON
  Schema. `$expr` is recognized only as a **direct** argument value; a
  `$expr` key nested anywhere inside a literal value is a validation
  error — letting it pass as data would silently send the wrapper object
  to the tool, exactly the no-op misspelling the format forbids.
  A `$expr` result that is a whole number within f64's exact-integer
  range is serialized as a JSON **integer**: expression arithmetic
  always produces f64, while tool parameters are commonly
  integer-typed (a panel brightness, a camera gain), and a `127.0`
  would fail their deserialization where `127` succeeds. Fractional
  and out-of-range results pass through unchanged.
- The tool's structured result becomes `result` for the instructions that
  follow (see [`result` scoping](#result-scoping)).
- Optional `retry`: on tool error, retry up to `max_attempts` total attempts
  with a fixed `backoff` delay between attempts. Default: no retry.
- A tool error (after retries) raises a workflow error that propagates
  outward through enclosing `try` instructions (see `try`), ultimately
  failing the workflow.
- If the tool result carries a **correction** (`rp` returns
  `pending_correction` on natural completion or `status: "aborted"` +
  `correction` on an immediate correction, per `rp.md` § Corrections), the
  engine fires the synthetic `correction_requested` trigger source (see
  [Triggers](#triggers)) with the correction as the event payload, then
  continues. An `aborted` tool result is **not** a workflow error — the
  document decides how to react via a trigger.

#### `sequence` — ordered container

```jsonc
{ "sequence": [ /* instructions, executed in order */ ] }
```

There is deliberately no parallel container in v1 (research finding:
marginal value; device-level concurrency already lives inside `rp` tools
such as `capture`-while-guiding).

#### `repeat` — loop

```jsonc
{ "repeat": { "until": "abs(result.median_adu - session.target_adu) / session.target_adu <= 0.05",
              "max_iterations": 10 },
  "body": [ /* instructions */ ] }
```

- Exactly one of `until` (expression, checked **after** each pass), `while`
  (expression, checked **before** each pass), or `count` (integer or
  `$expr`) is required.
- `max_iterations` (integer or `$expr` — evaluated once at loop entry, and
  a workflow error unless it yields a positive integer) is **required**
  alongside `until`/`while` — unbounded loops are a validation error. When
  `max_iterations` is exhausted without the condition being met, the loop
  still *completes*, with `result.converged = false` (see
  [`result` scoping](#result-scoping)); the document
  decides whether that is fatal (an `if` + `fail` pattern) or a warning
  (`log`). This mirrors `calibrator-flats`' non-converged-exposure warning
  behavior.
- Bound evaluation pins: a `$expr` bound must yield an integer-valued
  number (`2.0` from a tool result counts; `2.5` or a string is a workflow
  error at loop entry); `count` may be zero (zero passes), `max_iterations`
  must be ≥ 1. A `while` condition is also evaluated once *after* the
  final permitted pass, so a condition that turns false exactly at the
  budget still completes with `result.converged = true` — `converged =
  false` means the budget ran out while the condition still requested
  another pass. On a `count` loop, `max_iterations` is a guard against a
  runaway `$expr` count: if the evaluated `count` exceeds it, the loop
  fails loudly at entry rather than silently truncating the pass count.

#### `if` — conditional

```jsonc
{ "if": "event.hfr > session.last_focus_hfr * 1.2",
  "then": [ /* instructions */ ],
  "else": [ /* optional */ ] }
```

#### `set` — write the blackboard

```jsonc
{ "set": { "session.last_focus_hfr": "result.best_hfr",
           "session.target_adu": "result.max_adu * params.target_fraction" } }
```

- Keys must be `session.*` paths; values are always expressions.
- All values are evaluated before any key is written — a `set` cannot read
  its own writes. Because of this, keys within one `set` must not
  **overlap** (no key may be a path prefix of another, e.g. `session.a`
  alongside `session.a.b`) — the write order would be ambiguous;
  validation rejects the overlap.
- Writing a nested key creates missing (or `null` — the same thing, in
  `has()`'s view) intermediate objects; an intermediate that exists as a
  **non-object** (`session.a.b` when `session.a` is a number) is a
  workflow error — silently discarding the scalar would hide a document
  bug.
- `set` is the **only** way state crosses instruction boundaries or survives
  a crash. `result` is transient by design — anything worth keeping is
  copied to the blackboard explicitly, which makes the resume semantics
  visible in the document itself.
- Each `set` persists the blackboard atomically before the next instruction
  runs (see [Blackboard](#blackboard-and-persistence)).

#### `try` — cleanup and error handling

```jsonc
{ "try": [ /* body */ ],
  "catch": [ /* optional: runs on body error; error.* in scope */ ],
  "finally": [ /* optional: always runs */ ] }
```

- Semantics follow the `calibrator-flats` cleanup guard: `finally` runs
  whether the body succeeded, failed, or was cancelled by safety — with the
  caveat that after a safety cancellation `rp` has already secured the
  equipment and torn down the MCP session, so `finally` instructions that
  call tools will themselves fail; the engine runs them best-effort, logs
  each failure, and does not let a `finally` failure mask the original
  error.
- `catch` handles the error (the workflow continues after the `try`) unless
  it re-raises via `fail`. In `catch` and `finally` (on the error path),
  expressions can read `error.message`, `error.instruction_id`, and
  `error.tool` (null when the error was not a tool error).
  `error.instruction_id` is the raising instruction's **own** `id` (null
  when it declares none), not a nearest-ancestor id.
- `error.*` names the error the nearest enclosing error path is handling:
  a `catch` (or an error-path `finally`) binds it for its block and the
  enclosing scope's value is restored afterwards; a success-path `finally`
  leaves the enclosing scope's value visible (so a `finally` nested inside
  an outer `catch` still reads the outer error). Where no error is being
  handled, `error.*` reads as `null` — `has(error.message)` is the guard.
- `finally` failure semantics: on the success path a `finally` failure is
  a real workflow error; on the error path it is logged and the original
  error propagates (never masked); a safety termination during `finally`
  supersedes everything. A safety termination also skips `catch` entirely
  — by then `rp` has secured the equipment and torn down the MCP session,
  so there is nothing left to handle; only `finally` runs (best-effort).

#### `fail` — raise a workflow error

```jsonc
{ "fail": { "message": "'exposure never converged'" } }
```

Accepted anywhere an instruction is (`catch`, `then`, `else`, a `repeat`
body, …) and raises a workflow error deliberately; inside `catch` it
re-raises, propagating the failure outward. `message` is an expression —
quote it (as above) for a fixed string. A non-string message value is
rendered as compact JSON (an error message is terminal output, not data).

#### `wait` — pause at a safe point

```jsonc
{ "wait": { "duration": "30s" } }
{ "wait": { "until_event": "guide_settled", "timeout": "5m" } }
{ "wait": { "until": "seconds_until(session.flip_at) <= 0", "poll_interval": "10s", "timeout": "2h" } }
```

- Exactly one of `duration`, `until_event` (an `rp` event name), or `until`
  (expression re-evaluated every `poll_interval`, default `"10s"`).
- `until_event` and `until` require a `timeout`; expiry raises a workflow
  error. Triggers keep firing during a `wait` — a `wait` is one long safe
  point.
- An `until` condition is evaluated on entry, after each `poll_interval`,
  and one final time exactly when the timeout expires (the last sleep is
  clamped to the remaining budget) — only then does expiry raise. The
  timeout budget is measured by accumulated sleep time (monotonic), so a
  wall-clock adjustment (NTP step) can neither fire a timeout early nor
  extend a wait; the wall clock feeds only `seconds_until()`, where
  calendar time is the point.
- An `until_event` wait matches against every event received since the
  **run** started, not just those arriving after the wait began: the
  engine's event intake opens before the first instruction and buffers
  while instructions run, so an event emitted during an earlier
  instruction (say, the `exposure_complete` of a capture the document
  just made) still satisfies a later wait. Buffered events are checked
  once more exactly when the timeout expires — only then does expiry
  raise (the same final-evaluation rule as `until`). Its budget is
  measured like `until`'s: monotonic time spent waiting, so trigger
  actions running during the wait do not count against it. Every
  consumed event — the matching one included — feeds trigger
  evaluation; the wait neither extends nor aborts for non-matching
  ones. If the event stream is down, the
  wait simply runs to its timeout — a missing stream is never an
  instruction error ([Event Subscription](#event-subscription)).

#### `log` — operator-visible message

```jsonc
{ "log": { "level": "info",
           "message": "exposure converged",
           "values": { "duration": "session.duration" } } }
```

`level` is `debug` (default) or `info`, matching the workspace logging rule
(`info` only where the operator derives clear value). `values` entries are
expressions, rendered into the structured log record.

#### Reserved: `script`

`{ "script": … }` is **reserved** for a future sandboxed Luau handler node
(decision D3 in the plan). v1 validation rejects it with an explicit
"reserved for a future format version" error rather than "unknown key", so
documents written against a future version fail comprehensibly on an old
engine.

### `result` scoping

`result` is always the structured result of the most recently completed
result-producing instruction on the current execution path. Concretely:

- `tool` calls produce their structured result. A completed `repeat`
  produces a loop summary — `result.iterations`, plus `result.converged`
  for `until`/`while` loops (`true` when the condition was met, `false`
  when `max_iterations` ran out).
- `set`, `log`, and `wait` produce no result and leave `result` unchanged;
  containers (`sequence`, `if`, `try`) leave whatever the last instruction
  executed inside them left. In particular, the first instruction of a
  `then`/`else`/`catch`/`finally` block sees the `result` that was in
  scope when the branch was taken — an `if` condition reads `result`
  without consuming it.
- A `repeat`'s `until` expression is evaluated with the `result` left by
  the pass that just completed; `while` is evaluated with the `result` in
  scope before the upcoming pass.
- `result` is `null` at the start of a session and at the start of a
  trigger `do` block.

### Triggers

Triggers are the reactive overlay: cross-cutting rules that fire while the
procedure tree runs. They are declared at the document top level.

```jsonc
{
  "id": "refocus-on-hfr-degradation",
  "on": { "event": "exposure_complete" },
  "when": "session.last_focus_hfr != null",
  "while": "session.imaging == true",
  "cooldown": "15m",
  "do": [
    { "tool": "measure_basic", "args": { "document_id": { "$expr": "event.document_id" } } },
    { "if": "result.hfr != null && result.hfr > session.last_focus_hfr * 1.2",
      "then": [
        { "tool": "auto_focus", "args": { /* … */ } },
        { "set": { "session.last_focus_hfr": "result.best_hfr" } } ] }
  ]
}
```

(`exposure_complete`'s payload carries `document_id` / `file_path` only —
`rp.md` § Events — so the trigger measures HFR itself before deciding; the
`when` gate keeps it idle until the first `auto_focus` has seeded
`session.last_focus_hfr`, and `cooldown` bounds how often the measurement
itself runs.)

| Field | Meaning |
|-------|---------|
| `id` | Required, unique within the document. Names the trigger in logs and in the `session._triggers.*` bookkeeping state. |
| `on` | The source. `{ "event": "<rp event name>" }` — an envelope from the SSE stream; the envelope's `payload` becomes `event.*`. `{ "poll": { "tool": "<name>", "args": { … }, "interval": "30s" } }` — the engine calls the tool on the interval and the result becomes `event.*`. `{ "event": "correction_requested" }` — the synthetic source fired when a tool result carries a correction (payload: `event.action`, `event.reason`, `event.source`, plus engine-synthesized `event.delivery` — `"immediate"` when the tool result was `aborted`, `"after_current"` for `pending_correction`). |
| `when` | Optional expression over `event.*` + the usual namespaces. The trigger fires only when it evaluates to `true`. Absent = always fire. |
| `while` | Optional expression gate evaluated at fire time; lets a document scope a trigger to a phase via a blackboard flag (e.g. set `session.imaging = true` when the capture loop starts). This is the v1 substitute for container-scoped triggers, which are deferred. |
| `cooldown` | Optional minimum interval between firings (humantime). Last-fired timestamps live in the blackboard, so cooldowns survive resume. |
| `once` | Optional boolean: fire at most once per session (recorded in the blackboard). |
| `do` | Instructions, same vocabulary as the tree (nested triggers excepted). |

**Interleaving contract (safe points).** Trigger `do` blocks never preempt
an in-flight instruction. When a trigger fires, its action is queued; queued
actions run at the next **safe point** — after the current instruction
completes, or continuously during a `wait`. Multiple queued triggers run in
document order. A trigger whose action is queued or running does not queue
again. Only `rp` (safety) and Sentinel (watchdog) ever abort an in-flight
operation; a document that wants "stop the current exposure when X" reacts
to the *correction* `rp` delivers rather than aborting on its own.

**Poll lifecycle.** Poll triggers start when the workflow starts and stop
when it completes or is cancelled. A poll whose tool call fails logs at
`debug!` and skips that cycle — a flaky poll must not kill the session.

Implementation pins (`src/engine/`, Phase D2):

- **The pump.** Trigger evaluation is single-threaded and runs only at
  safe points: drain buffered events (synthetic first, then the SSE
  intake), match and gate them against each trigger, call due poll
  sources, then run queued actions in document order. An event that
  arrives mid-instruction waits in the intake until the instruction
  completes. The run's last safe point follows its last instruction: an
  event still in flight on the stream when the tree completes does not
  fire its trigger — a document that must react to its final operation's
  events ends with a short `wait`. Trigger evaluation never re-enters:
  inside a trigger action
  the pump only moves intake events into the engine's pending buffer
  (an `until_event` inside a `do` block matches against that buffer
  without feeding evaluation), and anything a trigger action itself
  provokes — new events, a synthetic correction from one of its tool
  calls — is evaluated at the next safe point on the procedure tree.
- **Occurrence counters.** Because the pump consumes events for trigger
  evaluation, the engine keeps a per-event-name count of *unconsumed
  occurrences* (names and counts only — still no event history). An
  `until_event` wait is satisfied by, and decrements, one unconsumed
  occurrence of its event name — that is how "matches every event since
  the run started" (§ `wait`) survives trigger evaluation running
  first. A wait consumes only its own event name's occurrences; unlike
  Phase D1's buffer draining, a wait no longer discards other names'
  events (they remain for later waits — and have already fed
  evaluation).
- **Gate timing.** `when` (together with the `once`, `cooldown`, and
  already-queued checks) is evaluated when the event is drained — a
  passing trigger queues its firing with the event payload. `while` is
  evaluated at fire time, immediately before the action runs, so an
  earlier trigger's action in the same batch can retract a later one by
  flipping the phase flag. An expression error or non-boolean value in
  either gate is a workflow error: a gate that cannot be evaluated is an
  authoring bug, and silently never-firing would hide it (write robust
  gates with `has()` and `??`).
- **Errors in actions.** An uncaught workflow error inside a `do` block
  fails the session, its message prefixed with the trigger id; a trigger
  that must not take the session down wraps its body in `try`. A safety
  termination during an action behaves as § Safety Behavior.
- **Bookkeeping.** `session._triggers.<id>.last_fired` (RFC 3339 wall
  time) and `.fired_once` are recorded when the action **completes
  successfully** — mirroring instruction `once` semantics, an action
  interrupted by a crash or termination does not count as fired, and
  instruction-level `once` markers inside the `do` block are the tool
  for non-repeatable steps within it. Cooldowns compare wall-clock time
  (they must survive resume, so they cannot use the monotonic reading
  that measures `wait` budgets); a backwards clock step extends a
  cooldown rather than firing early.
- **Polls.** The first cycle of a poll source is due one `interval`
  after the run starts, and each handled due reschedules the next at
  now + `interval` (missed cycles collapse — no burst after a long
  instruction). The schedule is in-memory: on a recovery invocation
  polls restart with a fresh first-due. A due poll whose trigger cannot
  fire anyway (`once` spent, cooldown open, firing already queued)
  skips the tool call entirely. An argument-expression failure counts
  as a call failure (`debug!`, skip the cycle) — poll args commonly
  read session state a later phase sets, and a poll must not kill the
  session before that phase arrives. During waits, sleep segments are
  clamped to the next poll due so polls stay on schedule; between
  instructions a due poll runs at the next safe point. A zero
  `interval` is a validation error.
- **Synthetic corrections.** A tool result with `status: "aborted"` or
  `status: "blocked_by_correction"` carrying a `correction` object
  synthesizes `correction_requested` with `event.delivery =
  "immediate"`; a result carrying `pending_correction` synthesizes it
  with `"after_current"` (`rp.md` § Corrections). The synthetic event is
  evaluated at the safe point immediately after the carrying tool call,
  ahead of that safe point's intake drain. A correction value that is
  not a JSON object is logged at `debug!` and ignored.
- **Action scoping.** A `do` block starts with `result = null` and
  `error.*` null and sees the firing's payload as `event.*`; all three
  are restored when the action ends (§ `result` scoping).
- **Waits.** A wait is one long safe point: the pump runs continuously
  between sleep segments, and time spent in trigger actions never
  counts against a wait's duration or timeout budget (budgets measure
  monotonic time around the sleeps alone). The awaited event of an
  `until_event` also feeds trigger evaluation — a trigger on the same
  event still fires, during the wait's own safe point, before the wait
  returns. An `until` condition may be re-evaluated more often than
  `poll_interval` while trigger actions run; `poll_interval` is the
  guaranteed maximum gap between evaluations only while idle.

### Expressions

Expressions are strings in a small, pure, CEL-style language. They appear in
`when` / `while` / `until` / `if` / `set` values / `$expr` wrappers /
`fail.message` / `log.values`.

**Namespaces** (read-only except via `set`):

| Namespace | Contents | Lifetime |
|-----------|----------|----------|
| `params.*` | Invocation parameters after defaulting/type-check; on recovery invocations the engine also injects `params._recovery.*` ([Re-entrancy Contract](#re-entrancy-contract)). | Session (immutable). |
| `session.*` | The blackboard. | Session (persisted, survives crash/resume). |
| `result.*` | Structured result of the most recent result-producing instruction ([`result` scoping](#result-scoping)). | Until the next result-producing instruction. |
| `event.*` | The triggering envelope payload (in trigger `when`/`do`) or poll result. | One trigger firing. |
| `error.*` | `message`, `instruction_id`, `tool` inside `catch`/`finally`. | One error scope. |

`event.*` is only in scope in a trigger's `when` / `while` / `do`;
`error.*` only inside `catch` / `finally` blocks. Referencing either
anywhere else is a **load-time validation error** (the engine knows
statically the value could never be non-null there), pointing at the
offending namespace root within the expression.

**Semantics:**

- Types: `null`, boolean, number (f64), string, plus JSON arrays/objects
  from tool results (member access and indexing only).
- Operators: `== != < <= > >=`, `+ - * / %`, `&& || !`, `?:` (conditional),
  parentheses.
- Functions (v1): `abs`, `min`, `max`, `clamp(x, lo, hi)`, `floor`, `ceil`,
  `round`, `seconds("1m30s")` (humantime string → f64 seconds),
  `humantime(secs)` (f64 seconds → humantime string, for building tool
  args), `has(session.x)` (path exists and is non-null),
  `seconds_until("<RFC3339>")` (evaluated against the engine's clock at
  evaluation time — the one sanctioned exception to tenet 4's purity rule,
  needed for dawn/flip math).
- **No** loops, user function definitions, assignment, tool calls, string
  interpolation, or regular expressions. Anything effectful is an
  instruction; anything algorithmic beyond this belongs in a built-in `rp`
  tool or (future) a `script` node.
- Accessing a missing path yields `null`; `null` in arithmetic or comparison
  (other than `==`/`!=`) raises an expression error → workflow error at
  that instruction. Authors guard with `has()` / `!= null` (as the trigger
  example above does). This is deliberate: silent `null` propagation in a
  system that moves telescopes is worse than a loud 2 a.m. error.
- Division or remainder by zero raises. There is no implicit type
  coercion.

**Grammar pins** (fixed by the Phase B parser spike, 2026-07-03):

- **Number literals use JSON number syntax, unsigned** (`-` is the unary
  operator): a leading digit is required (`0.5`, not `.5`), digits must
  follow the decimal point (`5.0`, not `5.`), exponents are allowed
  (`1.5e-3`), and leading zeros, hex/octal/binary forms, `_` separators,
  and bigint suffixes are rejected. A literal that overflows f64 is a
  parse error.
- **Strings** are `'…'` or `"…"` (single quotes are the ergonomic choice
  inside JSON documents) with exactly the escapes
  `\\ \' \" \n \r \t \uXXXX`; raw newlines and any other escape are
  errors.
- **Identifiers** (namespace roots and `.`-fields) are ASCII
  `[A-Za-z_][A-Za-z0-9_]*`. `null` / `true` / `false` are reserved words
  and cannot be field names — use `['null']` indexing for such keys.
  Other keys (unicode, `$`, spaces) are reachable the same way.
- **Comparison and equality operators form one non-chaining precedence
  level**: `a < b < c` and `a == b < c` are parse errors ("comparison
  operators cannot be chained"); parenthesize explicitly. Rationale: CEL
  groups `a == b < c` as `(a == b) < c` while JavaScript groups it as
  `a == (b < c)` — the format refuses the ambiguity instead of inheriting
  either convention.
- **Precedence** (tightest first): postfix (`.` `[]` call) → unary
  (`!`, `-`) → `* / %` → `+ -` → comparisons (non-chaining) → `&&` →
  `||` → `?:` (right-associative).
- **No comments**, no unary `+`, no `--`/`++` (write `-(-x)` for double
  negation), no trailing commas in argument lists, and calls only on bare
  built-in function names (no method-call syntax).
- `min` / `max` take two or more arguments; every other function's arity
  is fixed at its signature.
- **Nesting depth is capped at 64 levels** (parentheses, unary runs,
  argument lists, ternary branches). No legitimate expression comes near
  this; without the cap, adversarially deep input overflows the parser
  stack (found by the fuzz target).

**Evaluation pins** (fixed by the Phase B implementation, 2026-07-04):

- **Path traversal is total** — it never raises. Member/index access
  through `null`, a missing key, an out-of-range / negative / non-integer
  array index, a non-string object key, or a value of the wrong shape
  yields `null`. `has(path)` is true iff the path resolves to non-null
  (an explicit JSON `null` value counts as absent). The loudness comes
  when the `null` reaches an operator or function, per the null rule
  above.
- **Arithmetic and ordered comparisons are numbers-only.** `+` does not
  concatenate strings; `< <= > >=` on strings is a type error (string
  ordering is deliberately undefined — `==` / `!=` are the string
  comparisons).
- **Runtime overflow raises at the producing operation**: any `+ - * / %`
  result outside the finite f64 range is an evaluation error there (not
  at `set` persistence). Together with finite literals, JSON-sourced
  values, and the division/remainder-by-zero rule this makes ±Infinity
  and NaN unrepresentable — every number in the system is finite.
- **No truthiness.** `&&` / `||` / `!` and the `?:` condition require
  booleans. `&&` / `||` short-circuit left to right and `?:` evaluates
  only the taken branch — this is what makes
  `has(session.x) && session.x > 0` a sound guard.
- **Equality is deep and total.** `==` / `!=` accept any two values:
  structural for arrays/objects, numbers by numeric value regardless of
  JSON representation (a tool result's integer `5` equals the literal
  `5`), cross-type comparison is `false`, never an error.
- `%` is the f64 remainder (sign follows the dividend); `round()` rounds
  half away from zero; `clamp(x, lo, hi)` raises if `lo > hi`;
  `humantime(n)` requires a non-negative in-range number;
  `seconds_until(s)` requires an RFC 3339 string and is measured against
  the engine clock injected into the evaluation context (never the wall
  clock directly), so evaluation stays deterministic and testable.

The implementation (`src/expr/`) is a **hand-rolled lexer + Pratt parser
with a hand-written evaluator** — the parser is dependency-free; the
evaluator sits on the workspace's `serde_json` (values), `humantime`
(`seconds`/`humantime`), and `chrono` (`seconds_until`). The Phase B
spike compared this against reusing the `cel` crate's parser and an
`oxc_parser` JS-expression subset on a 178-case conformance corpus: the
cel parser silently collapses unary-operator runs (`- -x` → `-x`) and
cannot enforce the pins above from its AST; the oxc subset can, but only
with wrapper lexical checks approximating the hand lexer on top of 73
dependencies. See the plan's Phase B spike outcome for the full
evidence. The corpus ships as the module's conformance suite, alongside
proptest round-trip/no-panic properties and a cargo-fuzz target
(`services/session-runner/fuzz/`, standalone workspace, run with
`cargo +nightly fuzz run expr_parse`). Parse-time errors (lexing,
parsing, static checks: namespace roots, known functions and arities,
`has()` path arguments) and evaluation errors share one serializable
error type carrying a byte span into the expression source, for mapping
to JSON-Pointer locations in `/validate` responses.

## Blackboard and Persistence

The blackboard (`session.*`) is the workflow's only mutable state. It is a
JSON object persisted to `<state_dir>/<session_id>.json` with the workspace
atomic-write pattern (sibling temp file, fsync, rename, fsync parent
directory — same as `rp`'s exposure documents).

Writes happen after **every** mutation: each `set`, each `once` completion
marker, each trigger bookkeeping update (cooldown timestamps, once flags).
Mutations are small and infrequent (human-scale session cadence), so write
amplification is a non-issue; the invariant "the file always reflects every
completed `set`" is what makes tenet 2 sound.

Engine bookkeeping lives under reserved keys the schema forbids documents
from setting directly: `session._once.*` (completed once-markers),
`session._triggers.<id>.*` (`last_fired` RFC 3339 timestamp, `fired_once`
flag — § Triggers implementation pins).

The file is deleted when a session completes and the completion has been
acknowledged by `rp`; a leftover file at `/invoke` time for a **new**
session (no `recovery` context) is deleted **eagerly**, before the run
starts — lazy replacement on first persist would not be enough, because a
safety termination before the first write must not leave a stale file
(stale `_once` markers included) to be mistaken for this session's state
on the recovery invocation.

## Re-entrancy Contract

Resume is re-execution: on a recovery invocation, the engine reloads the
blackboard and runs the document from the root. For this to continue the
session rather than repeat it, documents must be **re-entrant**:

> Running the document from the top with the persisted blackboard and the
> current device state must converge to *continuing* the session, not
> redoing completed work.

The format provides three tools for this, in preference order:

1. **Dispatch-driven loops.** A capture loop that asks `get_next_target`
   and records progress with `record_exposure` is naturally re-entrant —
   the frames `capture` already wrote *are* the resume state, since
   `rp` derives progress from them on every read (rp.md § Progress
   derivation). This is the ecosystem lesson (Ekos counts frames on
   disk; Target Scheduler keeps a DB) applied through `rp`'s planner.
2. **Idempotent procedure.** Startup steps that are safe to repeat (cool
   the camera to a setpoint, unpark, connect) simply re-run.
3. **`once` markers** for steps that are *not* safe or sensible to repeat:

   ```jsonc
   { "tool": "calibrator_on", "args": { "calibrator_id": "flat-panel" },
     "once": "panel-on" }
   ```

   When the instruction completes **successfully**, `session._once["panel-on"]`
   is recorded (a failed instruction re-runs on resume); on re-execution
   the instruction is skipped. A skipped instruction produces nothing and
   leaves `result` unchanged — a following instruction that reads `result`
   must not assume the marked instruction just ran (that assumption is
   itself a re-entrancy bug). `once` keys must be unique
   within a document (validated). Use sparingly — a document that needs many
   `once` markers is usually missing a dispatch loop.

Resume behavior at `/invoke` with a non-null `recovery`:

1. Reload the blackboard for `session_id`. A missing blackboard file is not
   an error — the document starts with an empty `session.*` (first-run
   equivalent), because a crash can predate the first `set`.
2. Re-validate the document against the live tool catalog (equipment may
   have changed across the outage).
3. Log the recovery context (`reason`, interruption time) at `info!` and
   expose it as `params._recovery.*` so a document *may* branch on it
   (e.g. re-run `center_on_target` after any interruption) — but a correct
   document does not need to.
4. Execute from the root.

## Safety Behavior

On an unsafe transition `rp` — not `session-runner` — cancels the
plugin's in-flight tool call (a gated call such as `slew` aborts its
motion, a `capture` aborts its exposure, and either answers the tool
error `cancelled: safety`), stops guiding, parks the mount, and refuses
the plugin's further calls while conditions stay unsafe (per `rp.md`
§ Safety). From the engine's perspective: the in-flight tool call fails
with a terminated-session error. (MCP client pin: a call that *returns*
with the MCP `is_error` flag is a tool failure — retryable and
catchable — with one exception, the exact text `cancelled: safety`,
which is the terminated-session error; **any request-level failure** —
transport loss *or* a JSON-RPC protocol error — is the terminated-session
error too, never retried, never caught. `rp` reports ordinary tool
failures via `is_error` results, so a protocol error means `rp` itself
is unhealthy, and the engine's response — persist, exit without
completion, await re-invocation — is the safest generic recovery. Tool
results arrive as one JSON text content block: no content
is a `null` result; anything else — non-JSON text, a non-text block, or
multiple blocks — is a loud tool failure rather than a silently dropped
or stringified result.) The engine
then:

1. Stops trigger evaluation and abandons queued trigger actions.
2. Runs any enclosing `finally` blocks best-effort (their tool calls will
   fail; failures are logged, not raised).
3. Persists the blackboard (already current, by the write-on-mutation
   invariant).
4. Exits the run **without** posting a completion — the session is not
   over; `rp` re-invokes with recovery context on the safe transition, and
   the [re-entrancy contract](#re-entrancy-contract) takes it from there.

A document cannot subscribe to `safety_changed` to *countermand* any of
this; it may subscribe to it (e.g. to `log`), but by the time the trigger
would run, `rp` is refusing the run's calls. Safety-reaction logic in
documents is a smell the authoring docs will warn about.

## Event Subscription

The engine consumes `rp`'s SSE stream (`/api/events/subscribe`) for trigger
sources and `until_event` waits. The SSE `id` is the envelope's `event_seq`;
on reconnect the engine
sends `Last-Event-ID`, and replay is exact within `rp`'s retention window
(the most recent 512 envelopes — `rp.md` § Real-Time Stream). If the engine
was gone long enough that its cursor was evicted, the stream leads with a
`stream_gap` event instead: the engine logs the gap at `info!` and simply
continues — poll triggers re-observe current state on their next cycle, and
the re-entrancy contract already assumes events can be missed across an
outage. Events that arrive while no trigger matches
are discarded — the engine keeps no event history, only the per-name
unconsumed-occurrence counters that serve `until_event` waits
(§ Triggers implementation pins). The stream URL is derived
from the invocation's `mcp_server_url` origin unless overridden in
configuration.

Implementation pins (`src/events.rs`, Phase D):

- The subscription is **per session run**: the initial connect completes
  inline on the `/invoke` path, before the invocation is acknowledged and
  the first instruction executes — an event emitted milliseconds into the
  first tool call is already being captured (the `until_event` buffering
  and trigger evaluation depend on this). A failed or timed-out first
  attempt does not block the session; it just falls through to the retry
  loop. Every subscribe attempt — initial and reconnect alike — is
  capped at 5 s end to end (TCP, TLS, response headers), so an endpoint
  that accepts the connection but never answers cannot hang the
  invocation or wedge the reconnect loop; reads of the established
  stream stay uncapped (an idle stream is healthy — `rp` keep-alives
  every 15 s). The subscription closes when the run
  ends. Reconnects after a dropped stream or refused connect retry every
  1 s, indefinitely, carrying the cursor; a session with a dead stream
  keeps running — tool calls, not events, decide whether `rp` is alive
  (§ Safety Behavior).
- What the engine sees of an envelope is its event-type name plus its
  `payload` (the `event.*` value). Stream-control frames (`stream_gap`,
  `stream_error`) and malformed frames never reach the engine.
- The intake buffers up to 256 events (matching `rp`'s own broadcast
  buffer) while an instruction runs; if the engine falls further behind,
  backpressure reaches the socket and `rp`'s slow-consumer cutoff +
  reconnect replay take over — the designed loss path, ending in a
  logged `stream_gap` at worst.
- The transport is the workspace's house SSE-client pattern (`sentinel`'s
  watchdog, `bdd-infra`'s harness client): a chunked `reqwest` `GET` and
  a hand-rolled `id:`/`event:`/`data:` parser.

Webhook delivery is not used: `session-runner` registers no
`subscribes_to`/`barrier_gates` and never blocks `rp`'s tool pipeline. It is
purely a *consumer* of the stream plus an MCP *client*.

## Invocation

`rp` POSTs `/invoke` per the orchestrator protocol:

```jsonc
{
  "workflow_id": "wf-550e8400-e29b-41d4",
  "session_id": "session-2026-07-01",
  "mcp_server_url": "http://localhost:11115/mcp",
  "recovery": null,
  "config": {
    "workflow": "deep_sky",
    "parameters": { "camera_id": "main-cam", "focuser_id": "main-foc" }
  }
}
```

- `config` is this plugin's registered `config` object, forwarded verbatim
  by `rp` (`rp.md` § Orchestrator Invocation Protocol).
- `config.workflow` names a document: `<name>.json` resolved in the
  configured `workflows_dir` (the `.json` suffix may be spelled out; it is
  appended when absent), or an absolute path. Resolution outside
  `workflows_dir` for relative names is rejected.
- `config.parameters` is validated against the document's `parameters`
  declarations (unknown parameter names are errors; missing required
  parameters are errors; types must match).
- The acknowledgment returns the document's `estimated_duration` /
  `max_duration` (engine defaults `"1h"` / `"14h"` when the document omits
  them — `max_duration` must comfortably exceed a full night because `rp`
  treats its expiry as plugin timeout).
- Any validation failure (unknown document, schema violation, unknown tool,
  bad parameters) is returned as the `/invoke` error response — the session
  fails to start loudly, before any hardware moves.
- Completion is posted to `POST /api/plugins/{workflow_id}/complete` with
  `status` (`"complete"`, or `"error"` for a failed workflow) and a result
  payload: `{ "workflow": "<name>", "outcome":
  "complete" | "failed", "error": "<message when failed>" }` plus any
  values the document placed under `session.report.*` (the conventional
  place for a document to accumulate its summary — e.g. frames per filter;
  the fixed `workflow`/`outcome`/`error` keys win on a name collision).
  Both outcomes end the session: once `rp` acknowledges the completion
  (2xx), the blackboard file is deleted. A safety termination posts
  nothing and keeps the blackboard (see [Safety
  Behavior](#safety-behavior)); an unacknowledged completion also keeps
  it, and is logged loudly (the post carries a 30 s timeout — a stalled
  `rp` counts as unacknowledged rather than wedging the session task).

## Validation

Three layers, all sharing one implementation:

1. **Schema validation** — the document against the rules published in
   `schema/workflow-v1.schema.json`: structure, discriminant keys, unknown
   keys, reserved names (`_`-prefixed parameters, `session._*` writes),
   unique trigger `id`s / `once` keys, loop-bound requirements,
   expression fields parse-checked (including the namespace-scope rule
   above), `$expr` placement, non-overlapping `set` keys, and duration
   fields checked against the published surface form **and** humantime
   (humantime alone is looser — it accepts `1day` / `1 h`, which the
   published pattern rejects; the document format is their intersection)
   **and** capped at 24 hours — a session is a single night, so a longer
   poll interval, timeout, backoff, or cooldown is an authoring error,
   and the cap demotes overflow in the engine's deadline adds from an
   input-reachable panic to a `checked_add` defense-in-depth arm (the
   monotonic clock reading itself stays outside the cap's reach).
   Implementation note: layer 1 is a hand-rolled validation walk
   (`src/document/validate.rs`) that doubles as the typed-model builder
   (parse-don't-validate) and reports **all** findings in one pass with
   exact JSON-Pointer locations and targeted messages (raw JSON-Schema
   `oneOf` output cannot name a misspelled key or produce the `script`
   reservation error). The published schema remains the external
   contract: an agreement suite enforces that everything the walk accepts
   passes the schema — the walk is only ever *stronger*, where JSON
   Schema cannot express a rule.
2. **Catalog validation** (requires `rp`) — every `tool` node's name exists
   in `tools/list`; literal args type-check against the tool's parameter
   schema; required tool parameters are present (as literal or `$expr`);
   `$expr` argument types are checked at runtime when the call is made.
   Poll-trigger tools validate the same way. When a tool's schema pins
   `additionalProperties: false`, every argument **name** (literal or
   `$expr`) must be a declared property — a misspelled argument must not
   silently travel to the tool. A top-level `oneOf` whose branches are
   **presence-only** (each object carrying nothing but a `required` name
   list) is the addressing-alternatives contract rp's train-addressable
   tools publish (`camera_id` or `train_id`, …): exactly one branch must
   be fully present among the call's argument names — literal or `$expr`
   — and the combinator is excluded from the literal-value check, which
   could not see `$expr` names. Value-constraining `oneOf`s are not
   touched. Implementation
   (`src/document/catalog.rs`): literal values are checked with a real
   JSON-Schema validator against the tool's input schema (top-level
   `required` / `additionalProperties` stripped — those two are enforced
   separately so they see `$expr` arguments too); nested constraints
   inside a literal value (types, nested `required`, ranges) apply in
   full, and issue pointers extend into the literal
   (`…/args/target/ra_hours`).
3. **Parameter validation** — invocation `parameters` against the
   document's declarations.

`POST /validate` with `{ "document": { … } }` (or `{ "workflow": "<name>" }`
— exactly one) runs layers 1–2 and returns `200` with a report — the hook
for CI on shared workflow repositories, the future UI, and LLM authoring
loops:

```jsonc
{
  "valid": false,
  "errors": [ { "pointer": "/root/args/gain", "message": "…" } ],
  "catalog_validation": "checked"   // or "skipped: <reason>"
}
```

Each error is
`{ "pointer": "<RFC 6901 JSON Pointer>", "message": "…" }`, plus
`"expr_span": { "start": …, "end": … }` (byte offsets into the expression
string at that location) when the finding is inside an expression. Standalone `/validate` reaches `rp` through the
configured `mcp_server_url`; when that is unset or `rp` is unreachable, it
runs layer 1 only and says so in `catalog_validation` (`"skipped: no
mcp_server_url configured"` / `"skipped: rp unreachable (…)"`; schema
failures and workflows that cannot be loaded also skip the catalog check,
each under its own label). `4xx` is reserved for a malformed
*request* (neither/both input forms, invalid JSON).
`/invoke`, which always carries a live `mcp_server_url`, runs all three
layers before executing: validation failures return `400` with
`{ "error": "…", "issues": [ … ] }`, an unreachable `rp` returns `502`,
and only a fully validated invocation is acknowledged. A `session_id`
that could traverse outside `state_dir` (path separators, `..`) is
rejected — it names the blackboard file.

## Configuration

`session-runner`'s own config file (via `rusty-photon-config` conventions):

```jsonc
{
  "server": {
    "port": 11171,
    "bind_address": "0.0.0.0",
    "tls": null,
    "auth": null
  },
  "workflows_dir": "/var/lib/rusty-photon/workflows",
  "state_dir": "/var/lib/rusty-photon/session-runner",
  "mcp_server_url": null,       // rp MCP endpoint for standalone /validate only
  "events_url": null,           // override; default derives from mcp_server_url origin
  "service_auth": null,         // optional { "username", "password" } presented to rp
  "ca_cert": null               // optional PEM CA path for a TLS-enabled rp
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `server` | object | `{ "port": 11171 }` | The HTTP server for `/invoke`, `/validate`, `/health` |
| `workflows_dir` | path | required | Directory of workflow documents; first-party documents ship in the package |
| `state_dir` | path | required | Blackboard persistence directory |
| `mcp_server_url` | string or null | null | `rp` MCP endpoint used only by standalone `/validate` catalog validation; invocations always use the URL delivered in the `/invoke` payload |
| `events_url` | string or null | null | Explicit SSE endpoint; null derives `<mcp origin>/api/events/subscribe` |
| `service_auth` | object or null | null | HTTP Basic credentials presented to `rp` — MCP calls and the event stream alike. The D6 observatory credential; doctor `--fix` wires it (see [doctor.md](doctor.md) §Provisioning) |
| `ca_cert` | string or null | null | PEM CA path used to trust a TLS-enabled `rp` for the same connections |

The `server` block is the shared `ServerConfig` from
`crates/rusty-photon-server-config` (see ADR-016): `port`, `bind_address`
(default `0.0.0.0`), and optional `tls`/`auth`. Absent `tls`/`auth` means
plain, unauthenticated HTTP.

`service_auth` / `ca_cert` apply to session-runner **as a client of rp**
— every MCP connection (the configured `mcp_server_url` and per-invoke
URLs equally), the derived events subscription, and the completion POST
(`/api/plugins/{id}/complete`). The MCP client is
built through the shared `rp-mcp-client` crate
([ADR-017](../decisions/017-standard-mcp-client-construction.md)), which
enforces the credentials-only-over-verified-HTTPS policy: `service_auth`
without `ca_cert` (or on a non-HTTPS URL) is **not sent** — the client
connects unauthenticated and logs a loud warning, so plaintext
credentials never travel over cleartext.

Unknown configuration keys are rejected at load — a misspelled field must
not silently fall back to a default. CLI: `--config <path>` (default: the
platform config directory, e.g.
`~/.config/rusty-photon/session-runner.json` on Linux,
`%PROGRAMDATA%\rusty-photon\session-runner.json` on Windows), `--port` (overrides the
file's `server.port`; `--port 0` binds an ephemeral port, printed at startup),
`--bind-address` (overrides the file's `server.bind_address`), `--log-level`.

The config file is **operator-provided, never self-created**: `workflows_dir`
and `state_dir` are required with no usable defaults, so — unlike the services
that bootstrap via `rusty_photon_config::resolve_and_init` — session-runner
deliberately writes no default scaffold on first start, and refuses to start
until the file exists.

## Example Documents

Shipped first-party documents live in `services/session-runner/workflows/`.

### `calibrator_flats.json` (the generalization proof)

The port of the existing Rust orchestrator's algorithm
([`calibrator-flats.md`](calibrator-flats.md)). The shipped file is
canonical; its BDD scenarios — the Rust orchestrator's suite re-run
against this document through the same OmniSim + `rp` + `session-runner`
topology — are the behavioral oracle, and the engine's unit suite
executes the same file against `rp`-faithful mock results to pin the
exact call sequence to the Rust loop's (per-filter exposure reset, no
rescale once converged, cleanup on failure).

The filter plan is an `array` parameter (`[ { "name": "L", "count": 20 },
… ]`) iterated with the total-traversal idiom: a blackboard index and a
`while` gate of `has(params.filters[session.filter_index])` — one past
the end reads `null`, so `has()` turns false and the loop completes.
Abridged to the load-bearing shape:

```jsonc
{
  "version": 1,
  "name": "calibrator-flats",
  "parameters": {
    "camera_id": { "type": "string", "required": true },
    // "" (the default) = no filter wheel — an OSC rig; set_filter is skipped.
    // The parameter grammar has no optional-without-default, so the empty
    // string is the sentinel (house style — deep_sky's pass_filter does the same).
    "filter_wheel_id": { "type": "string", "default": "" },
    "calibrator_id": { "type": "string", "required": true },
    "filters": { "type": "array", "required": true },   // [ { "name", "count" }, … ]
    "target_adu_fraction": { "type": "number", "default": 0.5 },
    "tolerance": { "type": "number", "default": 0.05 },
    "max_iterations": { "type": "integer", "default": 10 },
    "initial_duration": { "type": "duration", "default": "1s" }
  },
  "triggers": [],
  "root": { "sequence": [
    { "tool": "get_camera_info", "args": { "camera_id": { "$expr": "params.camera_id" } } },
    { "set": { "session.target_adu": "result.max_adu * params.target_adu_fraction",
               // exposure limits arrive as humantime strings — convert once,
               // do arithmetic on numbers, humantime() back at the tool call
               "session.exp_min": "seconds(result.exposure_min)",
               "session.exp_max": "seconds(result.exposure_max)",
               // has() guard: resume continues at the current filter
               "session.filter_index": "has(session.filter_index) ? session.filter_index : 0" } },
    // fail fast on a nonsensical target — before the try, so the cover
    // never closes (the Rust oracle catches this mid-search; the document
    // raises before any hardware moves)
    { "if": "session.target_adu <= 0",
      "then": [ { "fail": { "message": "'target_adu is not positive (max_adu * target_adu_fraction) — check get_camera_info and target_adu_fraction'" } } ] },
    // record the cover's starting state (read-only) so the finally can
    // restore it; the has() guard keeps the original across a resume
    { "tool": "get_cover_state", "args": { "calibrator_id": { "$expr": "params.calibrator_id" } } },
    { "set": { "session.initial_cover_state": "has(session.initial_cover_state) ? session.initial_cover_state : result.cover_state" } },
    { "try": [
        { "tool": "close_cover", "args": { "calibrator_id": { "$expr": "params.calibrator_id" } } },
        { "tool": "calibrator_on", "args": { "calibrator_id": { "$expr": "params.calibrator_id" } } },
        // the applied brightness (device max when unset) seeds the
        // brightness ladder below
        { "set": { "session.brightness": "has(session.brightness) ? session.brightness : result.brightness" } },
        { "id": "filter-plan",
          "repeat": { "while": "has(params.filters[session.filter_index])", "max_iterations": 64 },
          "body": [
            { "if": "params.filter_wheel_id != ''",
              "then": [
                { "tool": "set_filter", "args": { "filter_wheel_id": { "$expr": "params.filter_wheel_id" },
                                                  "filter_name": { "$expr": "params.filters[session.filter_index].name" } } } ] },
            { "set": { "session.duration": "seconds(params.initial_duration)",  // reset per filter
                       "session.group_converged": "false",
                       "session.ladder_done": "false" } },
            // brightness ladder: re-run the search at half brightness while
            // it ends pinned OVER the target (a saturated sensor gives the
            // proportional step no gradient); an under-target miss is final
            { "id": "brightness-ladder",
              "repeat": { "until": "session.group_converged == true || session.ladder_done == true",
                          "max_iterations": 32 },
              "body": [
                { "id": "find-exposure",
                  "repeat": { "until": "abs(session.median_adu - session.target_adu) / session.target_adu <= params.tolerance",
                              "max_iterations": { "$expr": "params.max_iterations" } },
                  "body": [
                    { "tool": "capture", "args": { "camera_id": { "$expr": "params.camera_id" },
                                                   "duration": { "$expr": "humantime(session.duration)" } } },
                    { "tool": "compute_image_stats", "args": { "document_id": { "$expr": "result.document_id" } } },
                    { "set": { "session.median_adu": "result.median_adu" } },
                    // rescale only when another pass is coming, so the duration
                    // that converged is the one the flats reuse (the Rust loop's
                    // exact behavior)
                    { "if": "abs(session.median_adu - session.target_adu) / session.target_adu > params.tolerance",
                      "then": [ { "set": { "session.duration": "clamp(session.median_adu == 0 ? session.duration * 2 : session.duration * (session.target_adu / session.median_adu), session.exp_min, session.exp_max)" } } ] } ] },
                { "set": { "session.group_converged": "result.converged" } },
                { "if": "session.group_converged == false && session.median_adu > session.target_adu && floor(session.brightness / 2) >= 1",
                  "then": [
                    { "set": { "session.brightness": "floor(session.brightness / 2)" } },
                    // integral expression results are serialized as JSON
                    // integers in tool args — rp's brightness is a u32
                    { "tool": "calibrator_on", "args": { "calibrator_id": { "$expr": "params.calibrator_id" },
                                                         "brightness": { "$expr": "session.brightness" } } } ],
                  "else": [ { "set": { "session.ladder_done": "true" } } ] } ] },
            { "if": "session.group_converged == false",
              "then": [ { "log": { "level": "info", "message": "exposure did not converge, using best duration",
                                   "values": { "filter": "params.filters[session.filter_index].name" } } } ] },
            { "repeat": { "count": { "$expr": "params.filters[session.filter_index].count" } },
              "body": [
                { "tool": "capture", "args": { "camera_id": { "$expr": "params.camera_id" },
                                               "duration": { "$expr": "humantime(session.duration)" } } } ] },
            { "set": { "session.report.total_frames": "session.report.total_frames + params.filters[session.filter_index].count",
                       "session.filter_index": "session.filter_index + 1" } } ] },
        // a while loop that exhausts its budget completes with
        // result.converged == false — for this document that means an
        // absurd plan, and silently skipping filters is worse than failing
        { "if": "result.converged == false",
          "then": [ { "fail": { "message": "'the filter plan exceeds the 64-filter loop budget'" } } ] }
      ],
      "finally": [
        { "tool": "calibrator_off", "args": { "calibrator_id": { "$expr": "params.calibrator_id" } } },
        // restore, don't blindly open: a cover that started Closed (or
        // read Moving/Unknown/Error) stays closed, protecting the optics
        { "if": "session.initial_cover_state == 'Open'",
          "then": [ { "tool": "open_cover", "args": { "calibrator_id": { "$expr": "params.calibrator_id" } } } ] } ] }
  ] }
}
```

### `deep_sky.json` (the night-cycle document)

The classic deep-sky session as a shipped first-party document: startup
(unpark, tracking) → a dispatch loop (`get_next_target` → `set_filter`
whenever the recommended filter differs from the wheel's current one →
acquire on target change: stop guiding if active, slew → optional
`center_on_target` → optional `move_rotator` to the pass's effective
position angle → optional `auto_focus` → optional `start_guiding` →
one `capture` per pass (carrying the pass's `target` and
`frame_type: Light`, so the frame lands in the target's own directory
and counts toward its goals — rp.md § Progress derivation) →
`record_exposure` → optional `dither` on the
`dither_every` cadence, re-asking the planner after every frame) →
shutdown (stop guiding, optional `park`).

The document is **train-addressed**: it takes a single required
`train_id` (the imaging train) and threads it to `capture`,
`set_filter`, `center_on_target`, and `auto_focus` — rp resolves the
train's camera, sole filter wheel, and terminal focuser
(rp.md § Optical Trains). Sweep geometry comes from the train's
`auto_focus` config block, so the document carries no step
sizes or sweep exposures; the former `camera_id` / `focuser_id` /
`filter_wheel_id` / `focus_exposure` / `focus_step_size` /
`focus_half_width` parameters are retired (a pre-1.0 hard cutover of
the document's parameter contract — rp's tools keep device-id
addressing for direct callers). `focus_min_area` / `focus_max_area`
remain as parameters because they are **measurement policy**, not
sweep geometry: the HFR-degradation trigger's `measure_basic` call
requires them (rp.md § `measure_basic` Contract — callers own that
policy).

Rotation is adopted behind the `rotate` parameter (default `false`,
for rigs with a rotator in the imaging train): the document passes its
`train_id` to `get_next_target`, which resolves the effective position
angle (target value → the train's `default_position_angle_degrees` →
`0.0` north-up; rp.md § Target Store → Position angle) and returns it
as `position_angle_degrees`. The dispatch loop threads that value
through the blackboard (`session.pass_position_angle`, committed to
`session.target_position_angle` on acquisition), and when `rotate` is
enabled, acquisition — and the meridian-flip re-acquisition — issues a
train-addressed `move_rotator` to that sky angle after
slew/centering, before focus and guiding start (rotation changes
every train's field, so it precedes anything that depends on the
frame). A failed rotator move is logged, not fatal — framing degrades,
the night continues (tenet: robustness); with `rotate` off nothing
moves and the angle is display/config reality only (rotator-less
rigs).

Guiding is adopted behind the `guide` parameter (default `false`):
when enabled, acquisition stops any active guiding before the slew
and starts guiding (3 attempts, 30 s backoff) after centering and
focus — a guided session that cannot guide fails loudly instead of
silently capturing trailed frames all night. `dither_every` (default
`0` = off) dithers after every N recorded light frames; the dither
amount is rig geometry and comes from rp's `guiding.dither_pixels`
config, not a parameter, and a failed dither is logged, not fatal. Shutdown and the meridian-flip trigger
stop and restart guiding around their mount motion the same way.
Failed stops are logged, not fatal — and `session.guiding` clears only
on a **successful** stop, so the blackboard never claims a stopped
loop the guider still runs and the next handshake point retries the
stop.
The full document lives in `workflows/deep_sky.json`; the shape:

```jsonc
{
  "root": { "sequence": [
    /* init: counter defaults; on a recovery invocation, null
       session.target_name and clear session.guiding so the loop
       re-acquires (re-slew, re-center, re-focus, re-start guiding)
       before the next frame */
    { "if": "has(params._recovery.reason)",
      "then": [ { "set": { "session.target_name": "null",
                           "session.imaging": "false",
                           "session.guiding": "false" } } ] },
    /* unpark, set_tracking — both idempotent */
    { "repeat": { "while": "session.session_over != true", "max_iterations": 20000 },
      "body": [
        { "tool": "get_next_target",
          "args": { "train_id": { "$expr": "params.train_id" } } },
        /* end_of_session → done; target == null → 5m
           wait-and-re-ask; otherwise: derive this pass's
           filter/duration from the plan's nested exposure object
           (result.exposure.filter / result.exposure.duration_secs,
           falling back to params.filter / params.exposure_duration when
           result.exposure is null) and its position angle from
           result.position_angle_degrees, set_filter
           (train-addressed) when that differs from the wheel's
           current filter (the planner rotates the plan as goals
           complete, so this fires mid-target too); on target change
           stop_guiding if active, slew, center, move_rotator to the
           pass angle when params.rotate, focus, start_guiding
           when params.guide, commit session.target_* and
           session.imaging = true; then one capture at the pass
           duration, record_exposure, counter updates, and a dither
           every params.dither_every recorded frames, ending when
           params.max_frames (> 0) is reached */ ] },
    /* shutdown: session.imaging = false, stop_guiding if active,
       optional park */ ] }
}
```

**v1 adaptations.** The document is written against `rp`'s implemented
tool surface, and its shape reflects the documented planner v1 gaps
(`rp.md` § Dynamic Planner) — each is `rp` work, tracked as an issue, and
the document simplifies when it lands:

- **The planner supplies the exposure plan; parameters are the
  fallback.** `get_next_target` returns a nested `exposure` object
  (`exposure.filter` / `exposure.duration_secs`) from the recommended
  target's first *incomplete* plan entry (rp issues #462 and #463,
  closed). Every dispatch pass derives its own filter and duration from
  that object — converting `exposure.duration_secs` to a humantime
  string with the `humantime()` builtin — and falls back to the
  `exposure_duration` / `filter` parameters only when the target has
  **no plan** (`exposure` null). A plan is authoritative for the
  whole exposure spec: an explicitly unfiltered entry images
  unfiltered rather than merging with the `filter` parameter — and
  "unfiltered" means the wheel is left untouched, so a plan that
  wants glass-free frames through a filter wheel names its clear
  slot (rp.md § Target Definition), which also keeps the
  `{filter}` token in each frame's filename truthful — and with it the
  progress the scan derives. The
  filter change is its own step *before* acquisition, gated on "does
  this pass's filter differ from the wheel's current one"
  (`session.current_filter`) — so it fires on a target switch **and**
  when the planner rotates within a plan (Red after the Luminance
  goal completes), and centering/focus always run through the filter
  the frame will use. A pass whose filter names a wheel the train
  does not contain fails the session loudly — the train-addressed
  `set_filter` errors before the slew, since the target cannot be
  imaged as planned. `max_frames` (`0` = unbounded) remains an
  invocation parameter.
- **Progress lives in `rp`.** After every light frame the document
  calls `record_exposure` (rp issue #463, closed) with the pass's
  target and filter, feeding the planner's per-target/per-filter
  counters — that is what rotates plans, balances tied targets, and
  makes `end_of_session` reachable mid-night once every goal is met.
  The call is wrapped in `try`/`catch`-log: losing one count degrades
  planning, ending the night over bookkeeping would be worse (tenet:
  robustness). The blackboard keeps only the document's own budget
  (`session.total_frames`, mirrored to `session.report.total_frames`
  for the completion payload) — `max_frames` is a session cap, not an
  integration goal. rp's counters survive a safety interrupt/resume
  (same `rp` process), so a resumed dispatch continues where the
  night left off; they reset when a fresh session starts.
- **Dawn belongs to the planner.** `get_next_target` distinguishes dusk
  from dawn by the Sun's trend (rp issue #465, closed): `end_of_session`
  when the sky is bright and the Sun is rising, `wait_for_twilight` when
  it is not yet dark and the Sun is descending. The document trusts the
  reason alone — `end_of_session` ends the session, any other
  target-less reason waits 5 minutes and re-asks. (An earlier revision
  disambiguated dawn by frames-captured progress in the document; the
  heuristic is gone.)

**Triggers.** Five reactive rules. The three imaging-loop rules are
gated `while session.imaging == true` so they stay silent during
acquisition and shutdown; the two guide-watch rules are gated only on
`when params.guide == true && event.train_id != null` — rp emits their
events exclusively while guiding is active, the metric sweep re-checks
that precondition at the tool, and an `imaging` gate would race the
acquisition's commit `set` (a firing evaluated in that gap would be
dropped for the rest of the watch episode, since the degraded event
fires once per episode). The `train_id` half keeps them silent when
the watch runs without a guiding train and legally emits
`train_id: null` — a null-addressed sweep would only spam the
catch-log:

- `refocus-after-frames` — on `exposure_complete`, when
  `session.frames_since_focus` reaches `refocus_every` (a parameter;
  `0` disables): re-run `auto_focus` on the imaging train. The frame
  counter only counts light frames (the capture loop increments it),
  so a sweep's own exposures cannot re-trigger it.
- `refocus-on-hfr-degradation` — on `exposure_complete`, when
  `auto_focus` has seeded `session.last_focus_hfr` and
  `refocus_hfr_factor` (> 0) is set: `measure_basic` the finished
  frame and re-focus when its HFR exceeds `last_focus_hfr × factor`;
  `cooldown: "15m"` bounds how often the measurement itself runs.
- `flip-when-due` — poll `get_meridian_status` every 30 s; when
  `time_to_flip_seconds` drops under `meridian_margin` (default 300 s):
  stop guiding if active, re-slew to the current target (the
  post-meridian slew is what flips a GEM), re-center, and restart
  guiding if it was stopped. Self-limiting: after the flip the next
  crossing is ~24 h out, so the gate goes false without
  `once`/`cooldown` bookkeeping.
- `guide-af-on-degraded` — on `guide_focus_degraded` (rp's
  [Guide Focus Watch](rp.md), emitted only while the watch is
  configured and guiding): run the guide-only metric `auto_focus` on
  `event.train_id` — the guiding train named by the event, so the
  document needs no guide-train parameter. Sweep geometry comes from
  that train's `auto_focus` block; re-fire pacing is the watch's own
  `cooldown`.
- `refocus-on-escalation` — on `guide_focus_escalation` (same gate):
  run the full `refocus_train` on `event.train_id` — shared focusers
  first, then the guide differential (rp.md § `refocus_train`
  Contract) — and reset `session.frames_since_focus`.

In all four focus triggers the sweep call is wrapped in `try` with
a logging `catch`: a failed focus sweep degrades the night, but ending
the session over it would be worse (tenet: robustness). The skeleton's
`handle-correction` trigger from earlier drafts is **not** shipped: no
first-party plugin emits corrections yet, so it would be untestable
speculation — the synthetic `correction_requested` source remains
engine-tested and the trigger joins the document alongside the first
correction-emitting plugin.

**Re-entrancy.** The dispatch loop re-derives everything from the
blackboard + the planner: after a crash or safety interruption the same
loop continues from the persisted counters with **zero** `once` markers.
Startup is idempotent (unpark on an unparked mount is a no-op, tracking
and filter re-assert their state), and a recovery invocation nulls
`session.target_name` and clears `session.guiding` (rp already stopped
guiding, or the crash did) so the first loop pass re-acquires —
re-slew, re-center, re-focus, re-start guiding — before capturing
again, regardless of what the interruption did to the mount. A stale
`session.guiding` flag costs at most one `stop_guiding` call, which is
idempotent by rp's contract.

### `sky_flat.json` (the twilight-adaptation document)

Twilight sky flats: point the mount at the zenith, and per filter capture
flats while re-scaling the exposure after **every** frame against the
changing sky. This is the expression layer's stress test (the plan's
"convergence-loop ceiling"): unlike `calibrator_flats.json`'s
find-then-capture shape — a panel that holds still, so the search loop
runs once and the converged duration is reused — the sky brightens or
dims continuously, so there is no separate search: every pass is
capture → measure → keep-if-in-band → rescale regardless. The full
document lives in `workflows/sky_flat.json`; the load-bearing decisions:

- **Pointing is computed, not configured.** Zenith right ascension is
  the local sidereal time (`get_local_sidereal_time`, plus an optional
  `ra_offset_hours`, normalized with the
  `((x % 24) + 24) % 24` idiom because `%` follows the dividend's
  sign); zenith declination is the site latitude — which no `rp` tool
  exposes, so it is a required parameter (`site_latitude_degrees`).
  `slew` requires tracking, so the order is unpark → tracking on →
  slew → tracking **off** (stars trail through untracked flats and
  median-combine away in the stack).
- **The exposure window belongs to the operator.** `min_exposure_duration` /
  `max_exposure_duration` parameters are intersected with the camera's own
  limits from `get_camera_info` (`max(...)` / `min(...)`, with a
  fail-fast guard when the intersection is empty) — real flats have
  tighter floors (shutter and banding artifacts) and ceilings (star
  trails, window length) than the sensor does.
- **Every frame rescales; in-band frames count.** A frame whose median
  lands within `tolerance` of `target_adu` increments the filter's
  count (`session.flat_count`) and the report total; an out-of-band
  frame is logged and discarded (the file stays on disk — the stacking
  software selects by the report, not the engine). Either way the next
  duration is `clamp(duration × target/median, exp_min, exp_max)` —
  with the `median == 0 ? duration × 2` guard from the flats port.
- **The twilight window closes; the document notices when a frame
  captured *at* a rail is still out of band.** The window checks run
  **before** the rescale, so `session.duration` still holds the
  duration the measured frame was actually captured at — a rail must
  be tested by a real frame before it can close the window (checking
  after the rescale would end the run a frame early: a mid-range frame
  whose rescale merely *clamps* to the ceiling says nothing about what
  a ceiling-length frame would read). At dusk (`dawn: false`, the
  default) a frame at the floor that is still too bright means the
  window hasn't opened yet — `wait` 30 s and re-test; a frame at the
  ceiling that is still too dark means the window is over — set
  `session.window_over`, which ends the frame loop *and* gates the
  filter loop (later filters would only be darker). At dawn
  (`dawn: true`) the two reactions swap. Exhausting the frame loop's
  budget (`count + max_extra_attempts` passes, a `$expr` bound) without
  reaching the count is treated the same as a closed window — logged at
  `info!` and ended with a partial report rather than failed: partial
  flats are usable, and the session-level report
  (`session.report.total_frames`, `session.report.window_over`) says
  what happened.
- **Re-entrancy without a recovery branch.** The pointing steps are
  idempotent and simply re-run on resume (a fresh LST, a fresh slew —
  wherever the safety park left the mount); `session.filter_index` is
  the totals-traversal cursor; the per-filter frame counter resets via
  an **index marker** (`session.counting_index`) instead of
  unconditionally, so a resumed run continues the current filter's
  count rather than recapturing it; and `session.duration` carries
  across filters and across resume (the previous filter's converged
  duration is a better starting point than `initial_duration`). The
  frame loop is a `while` (checked before each pass), so a resume that
  lands after a filter's count is already met captures nothing extra.

The adaptation math cannot be pinned against OmniSim — the simulator's
image content does not track exposure duration — so, as with the flats
port, the engine's exec tests run the shipped document against scripted
medians (convergence, discard-and-rescale, both window closures, the
budget fallback), and the BDD scenario pins the plumbing end-to-end
(zenith slew from live LST, per-filter captures, park).

## Error Handling Summary

| Failure | Behavior |
|---------|----------|
| Document fails schema/catalog/parameter validation | `/invoke` returns the error; nothing executes. |
| Tool call errors (after `retry`) | Workflow error → nearest `catch`, `finally` blocks run; uncaught → workflow fails, completion posted with `outcome: "failed"`. |
| Tool result carries a correction | Synthetic `correction_requested` trigger; not an error. |
| Expression error (null arithmetic, division by zero) | Workflow error at that instruction, same propagation as tool errors. |
| `wait` timeout | Workflow error. |
| Trigger `when`/`while` gate errors or yields a non-boolean | Workflow error — a gate that cannot be evaluated is an authoring bug; silently never-firing would hide it. |
| Uncaught error in a trigger `do` block | Fails the session, message prefixed with the trigger id; wrap the body in `try` for a resilient trigger. |
| Loop `max_iterations` exhausted (`until`/`while`) | Loop completes with `result.converged = false`; not an error. |
| SSE stream drops | Reconnect with `Last-Event-ID`; exact replay within `rp`'s 512-event retention; on `stream_gap`, log and continue (§ Event Subscription). |
| Poll-trigger tool call fails | `debug!` log, skip cycle. |
| MCP session terminated by `rp` (safety) | Best-effort `finally`, persist blackboard, exit without completion; await re-invocation. |
| Engine crash / power failure | Blackboard reflects every completed `set`; recovery invocation re-executes per the re-entrancy contract. |
| Blackboard write fails | Workflow error (fail loud — continuing with unpersistable state would silently break resume). |

## MVP Scope

**In scope (v1):** the instruction vocabulary above; expressions per the
semantics above; `event` / `poll` / `correction_requested` triggers with
`when`/`while`/`once`/`cooldown`; blackboard persistence + re-derive resume;
schema + catalog + parameter validation and `/validate`; SSE consumption
with replay; the three shipped documents (`calibrator_flats.json`,
`deep_sky.json`, `sky_flat.json`).

**Deferred:** Luau `script` nodes (schema key reserved); container-scoped
triggers (use `while` gates); parallel containers; sub-workflow
imports/templates; a `ui-htmx` document editor; typed array-element
declarations (v1 `array` parameters are opaque JSON arrays — the flats
port needs no more, and element-shape mistakes still fail loudly, as
run-time expression errors instead of load-time findings); retirement of
the Rust `calibrator-flats` service (separate decision after the port has
mileage).

## Module Structure

```
services/session-runner/
  schema/workflow-v1.schema.json   The published document schema
  workflows/                       First-party documents (installed with the service)
  src/
    main.rs            CLI entry point (rusty-photon-service-lifecycle)
    lib.rs             ServerBuilder (two-phase: build → start)
    config.rs          Service configuration
    error.rs           SessionRunnerError (thiserror)
    document/          Document model, schema-layer validation, parameter
                       binding, workflow-name resolution (catalog
                       validation joins once the MCP client exists)
    expr/              Expression parsing + evaluation (Phase B)
    blackboard.rs      session.* state + atomic persistence
    engine/            Tree execution, safe points, trigger queue, resume
    events.rs          SSE client (Last-Event-ID replay)
    mcp_client.rs      rp-mcp-client (ADR-017) wrapper to rp's /mcp
    routes.rs          Axum router: POST /invoke, POST /validate, GET /health
```

## Testing Strategy

Testing follows [`docs/skills/testing.md`](../skills/testing.md).

### Unit tests

- Document parsing and validation: every instruction type, every schema
  error (unknown key, missing loop bound, duplicate trigger id, reserved
  names, `script` reservation message).
- Expression evaluation: every operator/function, every namespace, null
  handling, division by zero — table-driven, exhaustive.
- Engine semantics against a mock MCP-client trait: sequencing, `result`
  scoping, `set` persistence ordering, `try`/`catch`/`finally` paths
  (including finally-does-not-mask), `retry`, loop bounds and
  `result.converged`, trigger safe-point interleaving, `once`/`cooldown`
  bookkeeping.
- Blackboard: atomic write, reload, reserved-key protection.

### BDD tests (Cucumber, rp-harness)

Full three-process topology (OmniSim + `rp` + `session-runner`) via
`bdd_infra::rp_harness`, mirroring `calibrator-flats`' suite:

| Design section | Feature file | Representative scenarios |
|----------------|--------------|--------------------------|
| Invocation + validation | `invocation.feature` | invalid document rejected at `/invoke`; unknown tool named in error; parameter type mismatch |
| Flats port equivalence | `flat_calibration.feature` | the scenarios from `calibrator-flats`' suite, run against the document — same events, frame counts, cleanup-on-failure |
| Event subscription | `events.feature` | an `until_event` wait satisfied by an event emitted during an earlier instruction (pins subscription-from-run-start); a wait whose event never arrives fails the session at its timeout rather than hanging |
| Triggers | `triggers.feature` | a trigger action lands between exposures, never during one (proved by SSE seq order); `once` fires exactly once across three captures; cooldown suppresses firings inside its window; a poll trigger fires through its `when` gate |
| Resume | `recovery.feature` | SIGKILL the engine mid-capture-loop → restart → re-invoke with recovery → progress continues without repeated frames (exposure totals prove it); `once` marker not re-run (`filter_switch` count proves it); an rp outage terminates the run (service stays healthy, blackboard kept) and the session resumes against the restarted rp; an rp restart with a pinned `session_state_file` re-invokes the engine **by itself** (`recovery.reason = "rp_restart"` — rp startup recovery) and the session completes with no repeated frames |
| Safety | `recovery.feature` | a SafetyMonitor unsafe reading interrupts the session end-to-end through rp's own machinery (rp terminates the MCP session, the run terminates keeping its blackboard) and the safe transition re-invokes the engine with `recovery.reason = "safety_interruption"` — the resumed run captures exactly the remaining frames, the once marker is not re-run, and the completion deletes the blackboard. rp-side specifics (session `interrupted` status, `/mcp` 503 gate, `safety_changed` events) are pinned in rp's own `safety.feature` |
| Deep-sky document | `deep_sky.feature` | the shipped `deep_sky.json` against a computed night sky (site + planner targets placed so a candidate is viable at test time): the full cycle completes (unpark → slew → center → capture ×N → park); the planner's exposure plan drives the capture duration (a 2 s plan finishes a session the 300 s parameter default could not); a target whose plan carries a `count` ends the session through `record_exposure` → exhaustion → `end_of_session` with exactly the goal's frame count and no `max_frames` budget; a session started after dawn (a computed morning site — Sun risen and climbing) ends on the planner's `end_of_session` with zero slews and zero frames; a target sinking below its per-target altitude floor switches the dispatch loop to the second target (a second slew, frames on both sides of it); `refocus_every` fires `auto_focus` from the trigger overlay (`focus_started` count proves it); a due meridian flip re-slews between exposures, never during one; a safety interruption resumes with re-acquisition (two `centering_complete`); a guided session (`guide: true` against the harness guider stub) starts guiding after acquisition, dithers on the `dither_every` cadence, and stops guiding before the park (`guide_settled` / `dither_settled` / `guide_stopped` counts prove it); rp's Guide Focus Watch escalating over a degrading stub HFD script fires the document's `refocus-on-escalation` trigger end-to-end (`refocus_started` proves the wiring — sweep success is not asserted, per the OmniSim flat-HFR rule). The full guided call cadence, the `guide-af-on-degraded` wiring, the start-guiding retry-then-fail posture, and the `rotate` cadence (train-addressed `get_next_target`, `move_rotator` at the recommendation's angle between slew and capture, off by default, failure non-fatal) are pinned by the engine exec tests against scripted tool results. Mid-plan filter rotation is pinned by `rp`'s own planner BDD plus the engine golden tests (no simulated filter wheel or rotator in the deep-sky harness) |
| Sky-flat document | `sky_flat.feature` | the shipped `sky_flat.json` end-to-end against OmniSim: a computed night site with the mount taught the site and synced near the zenith → the session slews to the zenith from live LST, captures exactly the plan's flats through both filters, and parks (a 0.5 target fraction with 1.0 tolerance makes every OmniSim frame in-band, so the counts are deterministic — the simulator's image content does not track exposure). The adaptation math (rescale-always, discard-and-recapture, both window closures, the budget fallback) is pinned by engine exec tests running the shipped document against scripted medians |

The safety scenario exercises rp's real recovery re-invocation, and the
rp-restart scenario exercises rp's *startup* recovery (rp persists its
session registry and re-invokes on restart with
`recovery.reason = "rp_restart"` — `rp.md` § Recovery Behavior; the
harness pins rp's `session_state_file` across the restart). The
engine-kill and rp-outage scenarios still POST `/invoke` directly —
same ids, the registration's forwarded `config`, a non-null `recovery`
object — because they pin the *engine's* recovery contract in
isolation: what the engine does with a recovery invocation, independent
of who sends it.

Scenarios that need a document the shipped set doesn't provide (a
targeted `until_event` wait, resume fixtures) execute purpose-built
documents from `tests/fixtures/workflows/`; the spawned service's
`workflows_dir` is a per-scenario merge of `workflows/` and that fixtures
directory.

A separate TLS + auth smoke scenario (`auth.feature`) spawns only
session-runner itself with `server.tls` and `server.auth` configured and
proves `/health` requires HTTP Basic Auth over HTTPS.

### Golden documents

The shipped `workflows/*.json` are validated in CI against both the
validation walk and the published schema (a unit test walks the
directory), so a format change that breaks a first-party document fails
the build. The validation corpus additionally embeds
`calibrator_flats.json` verbatim, and the engine's exec tests execute it
against `rp`-faithful mock results — the shipped artifact, not a copy, is
what the unit suites pin.

## Future Considerations

- **Luau script handlers** (`script` nodes): stateless per-event handlers
  with blackboard-only state, preserving the re-derive resume model; the
  coroutine-yield boundary is where deterministic replay would attach if
  ever needed.
- **Document editor in `ui-htmx`** driven by the JSON Schema, including an
  expression condition-builder.
- **Sub-workflow composition** (`{"call": "…"}`) once shipped documents
  show real duplication.

The sky-flat document — once listed here as the stress test for the
expression layer's ceiling — shipped as `sky_flat.json` (§ Example
Documents): the per-frame exposure adaptation fits the bounded
expressions, so it did not become the motivating case for `script`
nodes.
