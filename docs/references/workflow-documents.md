# Authoring Workflow Documents

A **workflow document** is a JSON file that describes an imaging session —
the procedure to run, the reactive rules that watch over it, and the state
it keeps — executed by the generic
[`session-runner`](../services/session-runner.md) orchestrator against
[`rp`](../services/rp.md)'s MCP tool catalog. Three first-party documents
ship with `session-runner` (`deep_sky.json`, `calibrator_flats.json`,
`sky_flat.json`, in `services/session-runner/workflows/`); this guide is
for writing your own, or adapting one of those.

This is the *author's* reference: the format, the expression language, and
the habits that make a document survive a real night. The engine's exact
semantics (evaluation pins, trigger pump internals, event plumbing) are
specified in [`session-runner.md`](../services/session-runner.md) — link
targets are given throughout. The machine-checkable contract is the
published JSON Schema,
`services/session-runner/schema/workflow-v1.schema.json`.

Three rules shape everything below (the engine's tenets):

1. **Documents describe procedure and reaction — never choice or
   safety.** Target and filter *choice* is `rp`'s planner
   (`get_next_target`, `record_exposure`); safety is `rp`'s alone and a
   document cannot express, delay, or override it.
2. **Position is derived, not stored.** After a crash or interruption the
   engine re-executes your document from the top against the persisted
   state. Documents must be written *re-entrant* — see
   [The re-entrancy contract](#the-re-entrancy-contract).
3. **Everything validates before anything moves.** Schema, tool catalog,
   and parameters are all checked before the first instruction runs;
   `POST /validate` gives you the same check while you write.

## How a document runs

`session-runner` registers with `rp` as an orchestrator plugin. The
registration's `config` object names the document and carries its
parameters, and `rp` forwards it verbatim whenever a session starts:

```jsonc
// rp config, plugins[]:
{
  "name": "session-runner",
  "type": "orchestrator",
  "invoke_url": "http://localhost:11171/invoke",
  "requires_tools": [],
  "config": {
    "workflow": "deep_sky",               // resolves workflows_dir/deep_sky.json
    "parameters": { "camera_id": "main-cam", "focuser_id": "main-foc" }
  }
}
```

Starting a session (`POST /api/session/start`) makes `rp` invoke
`session-runner`, which loads and validates the document, connects back to
`rp`'s MCP server, executes the tree, and posts a completion when it
finishes. One `session-runner` instance runs whichever document its
registration names — different session types are different registrations
(or a config edit), not different services.

## Document anatomy

```jsonc
{
  "version": 1,
  "name": "my-workflow",                  // must match the file stem, '_' → '-'
  "description": "One paragraph of what this session does.",
  "parameters": { /* declared inputs — see below */ },
  "estimated_duration": "8h",             // optional; the /invoke acknowledgment
  "max_duration": "14h",                  // optional; rp kills the run past this
  "triggers": [ /* reactive rules — see Triggers */ ],
  "root": { "sequence": [ /* the procedure — see Instructions */ ] }
}
```

- `version` is the format version; the engine rejects versions it does not
  implement.
- `max_duration` must comfortably exceed the session's worst case — `rp`
  treats its expiry as an orchestrator timeout and secures the equipment.
- Durations everywhere in a document are humantime strings (`"300s"`,
  `"1h30m"`) matching the schema's surface pattern.

### Parameters

Every input your document takes is declared up front. Each parameter has a
`type` — `string`, `integer`, `number`, `boolean`, `duration`, or `array` —
and either `required: true` or a `default`:

```jsonc
"parameters": {
  "camera_id":  { "type": "string", "required": true },
  "exposure_duration": { "type": "duration", "default": "300s" },
  "max_frames": { "type": "integer", "default": 0 },
  "filters":    { "type": "array", "required": true }   // e.g. [ { "name": "L", "count": 20 }, … ]
}
```

- Supplied values are type-checked at invocation; unknown or missing
  required parameters fail the session before anything moves.
- Names beginning with `_` are reserved for the engine (`params._recovery.*`
  arrives on recovery invocations).
- An `array` parameter is opaque — element shapes are not declared, so a
  malformed element surfaces as a loud expression error at run time, not at
  load. Keep array elements simple and document their shape in the
  parameter's surrounding description.
- `duration` values stay humantime *strings*: convert with `seconds()` when
  you need to do math on them, and `humantime()` to build a tool argument
  back from a number (see [Expressions](#expressions)).

## Instructions

The procedure tree is built from nine instruction types. Every instruction
is an object with exactly one discriminant key, plus the optional common
keys `id` (names the instruction in logs and errors) and `once` (see
[re-entrancy](#the-re-entrancy-contract)). Unknown keys are validation
errors — a misspelling can never silently no-op.

### `tool` — call an MCP tool

```jsonc
{ "tool": "capture",
  "args": { "camera_id": { "$expr": "params.camera_id" },
            "duration": "300s" },
  "retry": { "max_attempts": 3, "backoff": "10s" } }
```

Argument values are **literal JSON by default**; a computed value is
wrapped as `{ "$expr": "<expression>" }`, and only as a *direct* argument
value — a `$expr` nested inside a literal is a validation error. Literal
arguments are type-checked against the tool's schema at load; every tool
name must exist in `rp`'s live catalog. A tool error (after any `retry`)
raises a workflow error that propagates to the nearest enclosing `try`.

### `sequence` — ordered container

```jsonc
{ "sequence": [ /* instructions, in order */ ] }
```

There is deliberately no parallel container: concurrency in this domain is
device-level and already lives inside `rp`'s tools.

### `repeat` — loop

```jsonc
{ "repeat": { "while": "session.frames < params.max_frames",
              "max_iterations": 20000 },
  "body": [ /* instructions */ ] }
```

Exactly one of `count` (integer or `$expr`), `while` (checked **before**
each pass), or `until` (checked **after** each pass — the body always runs
at least once). `max_iterations` is **required** with `while`/`until`;
running out of budget is not an error — the loop completes with
`result.converged = false` and your document decides whether that is fatal
(`if` + `fail`) or survivable (`log`, partial report). Prefer `while` when
the condition can already be satisfied on entry (a resumed session!), so a
completed loop re-runs zero passes rather than one.

### `if` — conditional

```jsonc
{ "if": "result.hfr > session.last_focus_hfr * 1.2",
  "then": [ /* … */ ],
  "else": [ /* optional */ ] }
```

### `set` — write the blackboard

```jsonc
{ "set": { "session.frames": "session.frames + 1",
           "session.report.total_frames": "session.frames + 1" } }
```

Keys must be `session.*` paths; values are always expressions. All values
are evaluated **before** any key is written, so a `set` cannot read its own
writes (note the example computes `frames + 1` twice), and keys within one
`set` must not overlap (`session.a` alongside `session.a.b` is rejected).
Each `set` is persisted atomically before the next instruction runs —
`set` is the *only* way state survives an instruction boundary or a crash.

### `try` — cleanup and error handling

```jsonc
{ "try": [ /* body */ ],
  "catch": [ /* optional; error.message / error.instruction_id / error.tool in scope */ ],
  "finally": [ /* optional; always runs */ ] }
```

`finally` runs whether the body succeeded, failed, or was cancelled by
safety — but after a safety cancellation `rp` is refusing gated calls,
so `finally` tool calls that move the mount or expose the optics fail
with the safety error (they are run best-effort and logged, never
masking the original error). `catch` handles the error unless it
re-raises via `fail`.

### `fail` — raise a workflow error

```jsonc
{ "fail": { "message": "'exposure never converged'" } }
```

`message` is an *expression* — quote it (single quotes inside JSON strings
are the ergonomic choice) for a fixed string. Forgetting the quotes is the
classic first authoring error: `"message": "exposure never converged"`
parses as an expression and fails with a parse error at load (loudly, at
least).

### `wait` — pause at a safe point

```jsonc
{ "wait": { "duration": "30s" } }
{ "wait": { "until_event": "guide_settled", "timeout": "5m" } }
{ "wait": { "until": "seconds_until(session.flip_at) <= 0",
            "poll_interval": "10s", "timeout": "2h" } }
```

Exactly one of `duration`, `until_event` (an `rp` event name), or `until`
(re-evaluated every `poll_interval`). `until_event` and `until` require a
`timeout`, whose expiry raises a workflow error. Two author-relevant
subtleties:

- **Durations here are literals.** `wait.duration` cannot be a `$expr`, so
  a parameterized pause is not expressible in v1 — pick a sensible constant
  (the shipped documents use `"5m"` and `"30s"`).
- An `until_event` wait matches events received since the **run started**,
  not since the wait began — an event emitted during an earlier instruction
  still satisfies a later wait. Triggers keep firing during waits.

### `log` — operator-visible message

```jsonc
{ "log": { "level": "info", "message": "session over",
           "values": { "total_frames": "session.report.total_frames" } } }
```

`level` is `debug` (default) or `info`; use `info` only where the operator
derives clear value (the workspace logging rule). `values` entries are
expressions.

### Reserved: `script`

`{ "script": … }` is reserved for a future sandboxed Luau handler node and
rejected by v1 validation with an explicit reservation error.

### `result` scoping

`result` is the structured result of the most recently completed
result-producing instruction: `tool` calls produce their result; a
completed `repeat` produces `{ iterations, converged }`; `set` / `log` /
`wait` leave `result` unchanged; containers leave whatever the last
instruction inside them left. `result` is transient by design — anything
worth keeping across instructions (or a crash) is copied to the blackboard
with `set`, which makes your resume semantics visible in the document
itself. Full rules: `session-runner.md` § `result` scoping.

## Triggers

Triggers are the reactive overlay — cross-cutting rules evaluated while the
procedure runs:

```jsonc
{
  "id": "refocus-on-hfr-degradation",
  "on": { "event": "exposure_complete" },
  "when": "has(session.last_focus_hfr)",
  "while": "session.imaging == true",
  "cooldown": "15m",
  "do": [ /* instructions, same vocabulary as the tree */ ]
}
```

- `on` names the source: `{ "event": "<rp event name>" }` (the envelope's
  payload becomes `event.*`), `{ "poll": { "tool": …, "args": …,
  "interval": "30s" } }` (the tool result becomes `event.*`), or the
  synthetic `{ "event": "correction_requested" }` fired when a tool result
  carries a correction.
- `when` gates at event time; `while` gates at *fire* time — use a
  blackboard flag (`session.imaging`) flipped by the procedure to scope a
  trigger to a phase. A gate that errors or yields a non-boolean **fails
  the session**: write robust gates with `has()` and guards, not optimism.
- `once` fires at most once per session; `cooldown` sets a minimum interval
  between firings. Both are recorded when the action **completes** — an
  action cut short by a crash does not count as fired.
- Trigger actions never preempt an in-flight instruction. They run at
  **safe points** — after the current instruction, or continuously during a
  `wait`. A document that wants "stop the current exposure when X" reacts
  to the correction `rp` delivers; it cannot abort on its own.
- An uncaught error in a `do` block fails the session. A trigger that must
  not take the night down wraps its body in `try` with a logging `catch` —
  the shipped deep-sky document does exactly this around `auto_focus`
  (a failed refocus degrades the night; ending it would be worse).
- A poll's first cycle is due one `interval` after the run starts; a
  failing poll call logs at `debug!` and skips the cycle.

Full pump semantics (queuing order, gate timing, occurrence counters):
`session-runner.md` § Triggers.

## Expressions

Expressions are strings in a small, pure, CEL-style language. They appear
in `if` / `when` / `while` / `until` conditions, `set` values, `$expr`
wrappers, `fail.message`, and `log.values`.

**Namespaces:**

| Namespace | Contents | In scope |
|-----------|----------|----------|
| `params.*` | Invocation parameters (plus `params._recovery.*` on recovery invocations) | everywhere |
| `session.*` | The blackboard — yours to write via `set` | everywhere |
| `result.*` | The last structured result | everywhere |
| `event.*` | The firing's payload / poll result | trigger `when` / `while` / `do` only |
| `error.*` | `message`, `instruction_id`, `tool` | `catch` / `finally` only |

Referencing `event.*` or `error.*` outside their scope is a load-time
validation error.

**Types and operators.** Values are `null`, booleans, f64 numbers, strings,
and JSON arrays/objects from tool results (member access and indexing
only). Operators: `== != < <= > >=`, `+ - * / %`, `&& || !`, `?:`,
parentheses. Functions: `abs`, `min`, `max` (2+ args), `clamp(x, lo, hi)`,
`floor`, `ceil`, `round`, `seconds("1m30s")` → number, `humantime(secs)` →
string, `has(path)`, and `seconds_until("<RFC3339>")` — the one sanctioned
clock read, for dawn/flip math.

**The rules that bite.** The language is deliberately strict — silent
null-propagation in a system that moves telescopes is worse than a loud
2 a.m. error:

- **Missing paths read as `null`, and `null` in arithmetic or ordered
  comparison raises.** Guard with `has()`:
  `has(session.x) && session.x > 0` is sound because `&&`
  short-circuits. The idiomatic initializer is
  `"session.x": "has(session.x) ? session.x : 0"` — which is also what
  makes a document resume-safe (see below).
- **No truthiness.** `&&` / `||` / `!` and `?:` conditions require actual
  booleans; `if: "session.frames"` is an error, write
  `session.frames > 0`.
- **Arithmetic and `<` / `>` are numbers-only.** `+` does not concatenate
  strings; strings compare only with `==` / `!=`.
- **Comparisons don't chain.** `a < b < c` is a parse error —
  parenthesize what you mean.
- **Division / remainder by zero raises**, and any result outside the
  finite f64 range raises at the producing operation. Guard divisors:
  the flats documents write
  `session.median_adu == 0 ? session.duration * 2 : session.duration * (session.target_adu / session.median_adu)`.
- **`%` follows the dividend's sign.** Normalizing an hour angle or RA
  needs the double-mod idiom the sky-flat document uses:
  `((x % 24) + 24) % 24`.
- **Number literals are JSON-style and unsigned** (`0.5` not `.5`, no hex,
  no `_` separators; `-` is the unary operator). Strings take `'…'` or
  `"…"` with the JSON escape set; single quotes avoid escaping inside JSON
  documents.
- **Durations are strings until you convert them.**
  `seconds(params.exposure_duration)` for math; `humantime(session.duration)` to
  hand a number back to a tool argument. Exposure limits from
  `get_camera_info` arrive as humantime strings too — convert once into
  the blackboard, do arithmetic on numbers, convert back at the tool call.
- Field names that collide with reserved words or contain unusual
  characters are reachable by indexing: `result['null']`,
  `params.filters[0]['count']`.

Everything effectful is an instruction; anything algorithmic beyond this
belongs in an `rp` tool. If an expression is getting clever, the design
question is usually "which tool should compute this?".

## State: the blackboard and the report

`session.*` is your document's only mutable state. It is a JSON object,
persisted atomically after every mutation, keyed by the session id — it
survives crashes, safety interruptions, and `rp` restarts, and is deleted
when the session completes. Keys under `session._*` are the engine's
(once-markers, trigger bookkeeping) and cannot be written by documents.

By convention, accumulate your session's summary under `session.report.*`:
the completion posted to `rp` carries everything under it (plus the fixed
`workflow` / `outcome` / `error` keys). The shipped documents report
`report.total_frames`; the sky-flat document adds `report.window_over` so
the operator can tell a full run from a truncated one.

## The re-entrancy contract

The engine never persists "where it is" in the tree. On a recovery
invocation — after a safety interruption, an engine crash, or an `rp`
restart — it reloads the blackboard and re-executes the document **from
the root**. The contract your document must satisfy:

> Running the document from the top with the persisted blackboard and the
> current device state must converge to *continuing* the session, not
> redoing completed work.

Three tools, in order of preference:

1. **Dispatch-driven loops.** A capture loop that asks `get_next_target`
   and records progress with `record_exposure` is naturally re-entrant —
   `rp`'s persisted progress counters *are* the resume state. This is how
   `deep_sky.json` resumes with **zero** once-markers.
2. **Idempotent procedure.** Steps that are safe to repeat simply re-run:
   unpark on an unparked mount is a no-op, `set_tracking` and `set_filter`
   re-assert state, a slew recomputed from live data re-points.
   `sky_flat.json`'s entire pointing preamble re-runs on every resume —
   including a fresh LST, so it re-points correctly however long the
   outage was.
3. **`once` markers** for steps that are genuinely not repeatable:

   ```jsonc
   { "tool": "calibrator_on", "args": { "calibrator_id": "flat-panel" },
     "once": "panel-on" }
   ```

   Recorded on *successful* completion (a failed instruction re-runs);
   skipped on re-execution, leaving `result` unchanged — so the next
   instruction must not assume it just ran. Keys are unique per document.
   A document that needs many once-markers is usually missing a dispatch
   loop.

The idioms that make counters and cursors resume-safe:

- **Guarded initialization** — never reset state unconditionally:

  ```jsonc
  { "set": { "session.filter_index": "has(session.filter_index) ? session.filter_index : 0" } }
  ```

- **Totals-traversal over an array parameter** — a cursor plus a `has()`
  gate; one past the end reads `null`, `has()` turns false, the loop ends:

  ```jsonc
  { "repeat": { "while": "has(params.filters[session.filter_index])",
                "max_iterations": 64 },
    "body": [ /* …use params.filters[session.filter_index]… */
              { "set": { "session.filter_index": "session.filter_index + 1" } } ] }
  ```

- **The index-marker reset** — per-iteration state that must reset when the
  cursor moves but survive a resume at the same cursor. From
  `sky_flat.json` (a per-filter frame count):

  ```jsonc
  { "set": {
      "session.flat_count": "has(session.flat_count) && session.counting_index == session.filter_index ? session.flat_count : 0",
      "session.counting_index": "session.filter_index" } }
  ```

  Because `set` evaluates before writing, `session.counting_index` on the
  right-hand side is the *previous* pass's value — a fresh filter resets
  the count, a resumed one keeps it.

- **Branching on recovery** when re-acquisition is wanted regardless of
  state — `deep_sky.json` nulls its current target so the dispatch loop
  re-slews, re-centers, and re-focuses before the next frame:

  ```jsonc
  { "if": "has(params._recovery.reason)",
    "then": [ { "set": { "session.target_name": "null",
                         "session.imaging": "false" } } ] }
  ```

  A *correct* document does not need `params._recovery.*` — resume must
  work from state alone — but it may use it to be smarter.

Recovery invocations arrive with the same `session_id`, so the blackboard
is found; `rp` restores its own planner counters across restarts, so
dispatch-driven documents continue where the night left off
(`rp.md` § Session Persistence).

## Safety (what your document cannot do)

On an unsafe transition `rp` — never the document — cancels the
in-flight call (it fails with `cancelled: safety`), aborts exposures,
stops guiding, parks the mount, and refuses every further gated call
with the safety error until conditions are safe again. Your document
experiences this as a terminated run: trigger evaluation stops,
enclosing `finally` blocks run best-effort (their gated tool calls fail
with the safety error and are logged), the blackboard is kept, and no
completion is posted. When conditions are safe again, `rp` re-invokes
with `recovery.reason = "safety_interruption"` and the re-entrancy
contract takes over. A document may subscribe to `safety_changed` to
`log`, but by the time such a trigger would run, `rp` is refusing the
run's calls — safety-reaction logic in documents is a smell.

## Validation and the authoring loop

`POST /validate` on `session-runner` runs the schema and catalog layers
and returns every finding with a JSON-Pointer location (and a byte span
into the expression when the finding is inside one):

```jsonc
// request — exactly one of:
{ "document": { …inline document… } }
{ "workflow": "my_workflow" }            // a file in workflows_dir

// response:
{
  "valid": false,
  "errors": [
    { "pointer": "/root/sequence/3/args/gain", "message": "…" },
    { "pointer": "/triggers/0/when", "message": "…",
      "expr_span": { "start": 4, "end": 11 } }
  ],
  "catalog_validation": "checked"        // or "skipped: <reason>"
}
```

Catalog validation (tool names, literal argument types, required tool
parameters) needs a reachable `rp`; standalone `/validate` uses the
configured `mcp_server_url` and says so when it can only run the schema
layer. `/invoke` always runs all layers — an invalid document fails the
session before any hardware moves.

The same endpoint is the hook for CI on a shared workflow repository and
for LLM-assisted authoring: the JSON Schema is a validatable generation
target, and `/validate` closes the loop — generate, validate, repair,
repeat. Nothing an LLM (or anyone) writes runs until it passes all three
layers and a human wires it into a registration.

Practical loop while writing by hand:

1. Copy the nearest shipped document (`services/session-runner/workflows/`).
2. Edit; keep `name` = file stem with `_` → `-` (the golden test enforces
   this for shipped documents).
3. `curl -s localhost:11171/validate -d '{"workflow": "my_workflow"}' | jq`
   until `valid: true`.
4. Dry-run against a simulator rig (OmniSim) before pointing it at glass.

## Worked examples: the three shipped documents

Each shipped document is also a teaching artifact — together they cover
the format's range. All three live in `services/session-runner/workflows/`
and are dissected in `session-runner.md` § Example Documents.

### `calibrator_flats.json` — the convergence loop

Panel flats: close the cover, light the panel, per filter find the
exposure that hits the target ADU, then capture N flats at the converged
duration. Demonstrates:

- `try` / `finally` cleanup (panel off, cover open — even on failure);
- an `until` convergence loop with a `$expr` bound and the
  `result.converged == false` warning path;
- the totals-traversal filter cursor;
- fail-fast guards before any hardware moves (`target_adu <= 0`);
- humantime conversion at the tool boundary
  (`humantime(session.duration)`).

### `deep_sky.json` — the dispatch loop and the trigger overlay

The classic night: unpark → dispatch loop (`get_next_target` → filter
change on plan rotation → acquire on target change: slew, center, focus →
one capture per pass → `record_exposure`) → park. Demonstrates:

- delegating *choice* to the planner and re-asking after every frame —
  target switching, plan rotation, and `end_of_session` all come from
  `rp`, not document logic;
- phase-scoping triggers with a `while session.imaging == true` gate;
- all three trigger shapes that ship: event-sourced refocus rules (count-
  and HFR-based, with `cooldown`), and a `poll` meridian-flip rule;
- resilient triggers (`try` + logging `catch` around `auto_focus`);
- recovery branching (`params._recovery.reason` → re-acquire);
- zero `once` markers — dispatch state lives in `rp`'s counters.

### `sky_flat.json` — adaptive convergence against a moving target

Twilight flats at the zenith: point from live LST, tracking off, per
filter capture-and-rescale after *every* frame while the sky brightens or
dims. Demonstrates:

- computed pointing (`get_local_sidereal_time` → the mod-24 idiom →
  `slew`);
- operator bounds intersected with device limits (`max(…)` / `min(…)` on
  `get_camera_info`'s humantime limits);
- rescale-always adaptation with in-band accept/discard — the loop shape
  for a light source that will not hold still;
- the twilight-window state machine at the clamp rails (dusk waits for a
  bright sky at the floor, ends on a dark sky at the ceiling; `dawn: true`
  swaps the reactions);
- graceful degradation: a closed window or an exhausted attempt budget
  ends the run with a partial report (`report.window_over`) instead of
  failing — partial flats are usable;
- the index-marker reset idiom for the per-filter counter.

## Authoring checklist

Before a document earns a night:

- [ ] `POST /validate` is clean, with catalog validation `"checked"`
      against the rig's real `rp` (its tool surface is what counts).
- [ ] Every `repeat` has the right bound — and `while` rather than
      `until` wherever the condition can be true on entry (resume!).
- [ ] Every division has a zero-guard; every read of maybe-absent state
      has a `has()` guard or a guarded initializer.
- [ ] Fixed strings in `fail.message` are quoted (`"'like this'"`).
- [ ] State that must survive a crash is in `session.*`, written by `set`
      at the moment it becomes true — `result` does not survive.
- [ ] The thought experiment: kill the process after *any* instruction,
      re-run from the top — does the document continue rather than
      repeat? (Check every unconditional `set … = 0` and every
      non-idempotent tool call; that's where the bugs live.)
- [ ] Triggers that must not end the session wrap their bodies in `try`.
- [ ] The session's outcome is legible from `session.report.*`.
- [ ] `estimated_duration` / `max_duration` reflect the real session, with
      margin on `max_duration` — `rp` enforces it.
- [ ] Dry-run against OmniSim (see
      [`docs/references/omnisim.md`](omnisim.md)) before real equipment.
