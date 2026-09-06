# Planetarium Target Import Plan — pick targets in a planetarium, image them when conditions are right

**Status: COMPLETE (archived 2026-09-05).** P1–P4 all shipped to `main`.
P1 built the `rp-targets` store into `rp` — CRUD/goals tools,
altitude-gating parity, capture-time target linkage, the on-disk
progress scan that replaced stored frame counts, and the
`rp-vocabulary` value types with `get_plan_schema` / `validate_plan`
([ADR-019](../../decisions/019-plan-data-vocabulary-and-validation.md)).
P2 added position-angle plumbing end to end. P3 delivered the
`planetarium-bridge` service — a virtual Alpaca Telescope whose Align
gesture imports the framed target, with a restart-surviving spool,
ConformU-clean and doctor-registered
([`planetarium-bridge.md`](../../services/planetarium-bridge.md)) —
after the P3a and P3b spikes answered every open question and flipped
the import gesture from GoTo to Align (Decision 2's amendments). P4
shipped `ui-htmx`'s `/targets` inbox. P5 (Stellarium) and P6 (Cartes du
Ciel) stay deferred until the P3/P4 loop is proven in the field.

## Goal

Target selection today means adding targets to rp's redb target store by
hand, one `add_target` MCP call at a time (the legacy `targets[]` JSON
config array was removed once the store cutover landed).
The operator's actual planning tool is a planetarium — SkySafari on the couch,
Stellarium or Cartes du Ciel at a desk — where they compose the frame: an
offset center to fit a nebula plus a star field, a mosaic anchor, a rotation
that puts a companion galaxy in the corner. None of that survives into
rusty-photon; it is retyped, approximately, into the store.

The outcome of this plan: **pressing Align in the planetarium adds the
target to rusty-photon's target database** (the gesture was GoTo until
P3b proved GoTo unconditionally horizon-gated — see Decision 2's
2026-07-30 amendment). The planetarium connects to a virtual
ASCOM Alpaca telescope served by a new `planetarium-bridge` service; an
Align delivers the selected object's exact J2000 coordinates — composed
framings ride a faint anchor star at the intended frame center — the
target is named rp-side by reverse catalog lookup, and
the target lands *paused* in an inbox for the operator to review — attach
acquisition goals, adjust the position angle, activate. rp's planner then
images it whenever conditions are right; the planetarium and the scheduler
stay fully decoupled.

Position angles are handled in layers: per-target angle (set in the ui-htmx
target editor, for rigs with a rotator) → the target's optical train's
configured default angle (a rotator-less rig's fixed camera mounting angle,
matched manually in the planetarium's FOV indicator) → 0° = north-up.

Offset-center and mosaic framing via SkySafari depends on an unverified
assumption — that its UI can GoTo an arbitrary point, not only cataloged
objects. That is a **go/no-go gate in milestone P3a** (Decision 8), not a
footnote: if SkySafari can only GoTo cataloged objects, its channel imports
nominal centers only, and composed framing arrives via the P4 editor or the
P5/P6 frontends.

## Implementation Status

| Phase | Description | Status | Branch / PR |
|-------|-------------|--------|-------------|
| P0 | This plan | In progress | feature/planetarium-target-import |
| P1 | Build the `rp-targets` crate + rp integration (per [rp-targets.md](../../crates/rp-targets.md) MVP). Three requirements are fixed here: **altitude-gating parity**, a **minimal operator surface**, and **capture-time target linkage** (Decisions 9, 10, 11), plus a **shared plan-data vocabulary crate** `rp-vocabulary` with validation-by-construction (Decision 12, settled 2026-07-23 — value types + the `Target`/`PlannerTarget` `coord: IcrsCoord` newtype migration and the `rp-ephemeris` bridge landed; the `schema`/`validate` MCP tools have now landed too — see the P1 status cell) | Crate scaffold, BDD scaffold (Phase 2), and design doc landed. Phase 3 implementation landed *incrementally*: the store wired into rp, its 6 CRUD/goals MCP tools (Decision 10 — done), `session.file_naming_pattern`'s config-load token validation (`target_naming_template.feature`, all 4 scenarios passing), `get_next_target`'s altitude-gating parity against the store (Decision 9 — done: `add_target`/`update_target` now accept `scheduling`, `target_store.default_scheduling.min_altitude_degrees` config, `target_store_planner.feature`'s 2 scenarios passing, `@wip` removed), the naming-template's render/parse engine (`rp::config::naming_template::CompiledTemplate`, regex-backed, unit-tested including a `parse(render(x)) == x` round trip), and (Decision 11 — done) `capture`'s `target`/`frame_type` parameters: `frame_type: Light` resolves `target` against the store and denormalizes onto the exposure document; `Dark`/`Flat`/`Bias` use a reserved `"dark"`/`"flat"`/`"bias"` slug absent an explicit `target`; `capture` renders `directory_pattern` (now a real config field, landed alongside `file_naming_pattern`) then `file_naming_pattern`, deriving `{frame_number}` via an on-disk scan of the target directory (`CompiledTemplate::parse`, scoped to `capture`'s own use, not yet reused by the progress tools below) and `{night_date}` via `rp_ephemeris::Site::night_date`'s noon-rollover rule. `auto_focus`/`center_on_target`'s internal captures are explicitly deferred (see Decision 11's amendment) and keep `frame_type` omitted. The Dynamic Planner cutover has now **landed**: `record_exposure`/`get_session_progress`/`get_target_status` read only the active rows of the target store, identifying targets by slug — the legacy `targets[]` config array is **removed** (`Config.targets` renamed to the typed `target_store`; a stray `targets` key or an array-shaped `target_store` now fails config load loudly). **The on-disk frame scan behind progress has now landed too, completing P1's progress story**: `rp::planner::progress_scan` walks the night directories under a target's slug using the same two compiled templates `capture` writes with, buckets frames by the `(filter, binning, exposure_duration)` triple, and classifies each against the target's effective `GradingThresholds` (its own `grading`, field-wise over the new `target_store.default_grading` config) read from the sidecar's `grading` section — the section contract is now pinned in rp.md (`hfr`/`star_count`/`eccentricity`/`snr`), with **a frame rejected only on evidence** (no sidecar, no section, or no value for the judged metric all count good, so progress still advances on a rig with no grading plugin, which stays out of scope). Frame counts are no longer stored anywhere: `record_exposure` increments nothing and now returns the derived per-goal rows, `get_session_progress`/`get_target_status` carry the same per-goal shape (superseding the filter-keyed `{completed, goal}` map), `get_next_target` derives once per call and ranks against that snapshot (keeping `decision::next_target` pure), goals are judged met on `good` rather than `total`, and the session state file shrank to just `last_filter_key`. Consequence — and the point of the store: a plan spans nights, so a restart resumes at the true count. `add_target`/`update_target` accept `grading`; `"NA"` frames map to the unfiltered goal slot (`capture`'s wheel-less rendering). Two things the cutover forced, both caught by the BDD suites rather than by inspection: **`deep_sky.json` now passes `target` + `frame_type: Light` to `capture`** — it never did, so with progress derived from disk its frames were unattributable and a goal-terminated session could never end — and to make that safe on a rig with no `session.file_naming_pattern`, `frame_type` and the templated path were **decoupled**: supplying `frame_type` without a pattern is no longer an error, it stamps the document and keeps the flat filename (coupling them would have failed every capture on such a rig the moment it adopted the current workflow). A pre-existing `get_target_status` bug was fixed alongside (it matched only the whitespace-stripped slug, so progress read `null` for every multi-word target name `add_target` had kebab-cased), and `target_store.default_grading` without a naming pattern is now a config-load error rather than a silent no-op. **Decision 12's second half has now landed**, closing P1: `get_plan_schema` / `validate_plan` (MCP-only, read-only, no REST counterpart) publish the plan-data JSON Schema and check a candidate against every rule the corresponding write applies, reporting `{path, msg}` FieldErrors with dotted, indexed paths (`goals[1].binning`) — the same shape `config-actions` returns, so a surface renders plan errors with the code it already has. The rules live once, in `rp::mcp::built_in::plan_validation`, which both `validate_plan` and the writers call; BDD pins the agreement in both directions rather than leaving it to convention. Consolidating them closed a real writer gap: `add_target`/`update_target` never range-checked their `grading`/`scheduling` parameters, so a negative or `NaN` threshold was accepted as a per-target override while config load rejected the identical value as a default | feature/rp-targets-p1, feature/rp-progress-frame-scan |
| P2 | Position-angle plumbing: `position_angle_degrees` on `Target`, per-train config default, `get_next_target` returns effective angle, `deep_sky` workflow rotator step. Decision 11's blackboard-threading pattern (target identity into `capture`) is this phase's own idiom, applied to P1 a phase early | **Landed 2026-07-31**: `Target.position_angle_degrees` (serde-default migration, like the writer-identity fields), `equipment.optical_trains[].default_position_angle_degrees` (`PositionAngleDegrees` newtype, parse-don't-validate), `add_target`/`update_target` accept the field (explicit-null clears on update; the import form rejects it), `get_next_target` takes an optional `train_id` and returns the effective angle as a top-level `position_angle_degrees` (rp.md § Target Store → Position angle; `target_position_angle.feature`), and `deep_sky.json` grew a `rotate` parameter (default false) issuing train-addressed `move_rotator` after centering and after the meridian-flip re-slew, failure non-fatal (engine exec tests) | feature/rp-position-angle-p2 |
| P3 | `planetarium-bridge` service: Alpaca Telescope impersonation → target creation via rp (gated by milestone P3a, a sanctioned verification spike) | **P3a COMPLETE 2026-07-29** — real-SkySafari session run against the spike (`spikes/planetarium-bridge-p3a`); all five questions answered in [planetarium-bridge.md](../../services/planetarium-bridge.md). Headlines: **go/no-go = GO** (coordinate-entry GoTos arbitrary points; the entry box is implicitly JNow, converted to J2000 on the wire; tap-empty-sky is not a thing, but faint-star-adjacent framing works), J2000 honored, GoTo = `SlewToCoordinatesAsync` / Align = `SyncToCoordinates`, 1 Hz poll cadence, discovery is subnet-local, GoTo is client-side horizon-gated (below-horizon targets can't be imported until risen; reported position lingering below horizon wedges the client). **P3 design doc drafted 2026-07-29** (settled interactively — sync rejected not ignored, typed writer-identity provenance, rp-side naming; amendments recorded under Decisions 2/3/4 below). **P3b horizon experiment COMPLETE 2026-07-30** (second device, identical SkySafari build): the GoTo horizon gate is unconditional (both forms, every horizon display setting — coordinate entry is gated too), the wedge did not reproduce (device/state-dependent) — outcome: **the import gesture flipped from GoTo to Align** (Decision 2's second amendment; [planetarium-bridge.md](../../services/planetarium-bridge.md) § P3b appendix). **Phase 2 (BDD) landed 2026-07-30**: `services/planetarium-bridge` crate scaffold plus the design doc's four feature files as `@wip` executable specifications, with the harness (typed Alpaca Telescope client + stub rp MCP server). **Bridge implementation landed 2026-07-30**: the full service (virtual Telescope, import pipeline, bounded fsynced spool with restart-surviving replay, `/health`), all 65 BDD scenarios live (`@wip` removed), ConformU clean, spike crate deleted, doctor/packaging/port-table registration done. **rp-side `source` contract landed 2026-07-30**: `Target` writer identity (`created_by`/`updated_by`, serde-default migration), `add_target`'s import form (bare coords + `source`, forced `active: false`, proximity-only dedup via `target_store.import.dedup_arcsec`, Decision 3's pending-and-unedited upsert predicate), rp-side cone-search naming through `rp_catalog::nearest` (class-tiered radii, offset/coordinate display forms) — authoritative text rp.md § Target Store → Writer identity / Import form, exercised by `target_store_import.feature`. **Doctor slice landed 2026-07-30, completing P3**: `joins.fake-mount` hard-fail (static loopback port join + a UniqueID probe of the configured mount for host-name rig URLs), the bridge registered in doctor's embedded catalog (the missing piece of the service registration), and `plan_client_wiring` extended to the bridge's nested `rp.service_auth`/`rp.ca_cert` block | feature/planetarium-bridge-design, feature/planetarium-bridge-bdd, feature/planetarium-bridge-impl, feature/planetarium-rp-source, feature/doctor-fake-mount |
| P4 | ui-htmx target inbox: review pending targets, goal editing, PA override, activate/discard | **Landed 2026-07-31** ([ui-htmx.md § Targets inbox](../../services/ui-htmx.md#targets-inbox-targets)): fourth nav surface at `/targets` — pending/active listing with provenance and stale-goal roster flags, per-target review form (goals editor, tri-state PA field per the P4 note: blank ⇒ explicit-`null` clear, `0` explicit north-up), activate/pause/discard — driving rp's MCP-only target tools through `rp-mcp-client` (consumer #4, per-request sessions) from the existing `rp` config block; `targets_page.feature` (16 scenarios, real rp seeded over MCP) | feature/ui-htmx-target-inbox |
| P5 | Stellarium enrichment frontend (telescope-protocol doorbell + RemoteControl name/Oculars-angle query) | Deferred | |
| P6 | Cartes du Ciel frontend (TCP 3292 client: named selections, `GETFRAMES` mosaic import with per-panel PA) | Deferred | |

Order: P1 first (everything else reads the target store). P2 and P3 then
proceed independently of each other (the bridge writes targets with
`position_angle_degrees: None` until P2 lands); P4 needs P1 and gets richer
as P2/P3 land. P5/P6 are deferred until the P3/P4 loop is proven in the
field. Each phase follows
[development-workflow.md](../../skills/development-workflow.md): design-doc
update first (rp.md for P1/P2, new docs/services/planetarium-bridge.md for
P3, ui-htmx.md for P4), BDD second, code third — with P3a as an explicit,
sanctioned exception (Decision 8).

Explicitly rejected / out of scope (see Decisions 6–8):

- LX200/Meade TCP emulation and SkyFi UDP discovery (the pre-Alpaca SkySafari
  path).
- Observing-list file importers (`.skylist`, Stellarium `observingList.json`,
  CdC lists). `.skylist` carries catalog names only — no coordinates — so a
  file import can only recover catalog centroids, which defeats framing.
  May return later as an explicitly-labeled "unframed seeding" convenience.
- Any LiveSky (SkySafari cloud) integration: no public API, and the service
  cannot export observing lists at all.

## Decisions (fixed — settled interactively 2026-07-22, revised same day after adversarial review)

1. **The universal ingress is a virtual ASCOM Alpaca Telescope, not LX200.**
   SkySafari 7+ speaks Alpaca natively (v7 floor is accepted; v8 shipped
   2025-12), Cartes du Ciel has a cross-platform Alpaca mount driver, and
   this codebase is already an Alpaca shop: the `ascom-alpaca` crate's
   server feature, `AlpacaServerConfig`, doctor registration, and the
   ConformU harness all apply as-is, versus hand-emulating a quirk-laden
   text protocol (stateful `:Sr`/`:Sd`/`:MS#`, ~4 Hz polling with
   connection churn, undocumented JNow semantics). Alpaca also lets the
   device *declare* its epoch (`EquatorialSystem` = J2000) instead of
   guessing. Alpaca UDP discovery (port 32227) is available but follows the
   fleet convention: **opt-in, single responder per host** (see the
   `ports.discovery-collision` doctor check) — the documented default setup
   is manual IP:port entry in the planetarium.
2. **GoTo — and only GoTo — is the add-target gesture.**
   `SlewToCoordinatesAsync`/`SlewToCoordinates` record a target; the device
   reports a brief simulated convergence so the client shows completion.
   **`SyncToCoordinates` is accepted, logged, and ignored**: in every
   planetarium, Sync/Align means "the scope IS pointing here" — a
   pointing-model correction, not target intent — and treating it as
   add-target would mint garbage targets from routine alignment taps. The
   device never touches hardware (tenet 3 is satisfied trivially: it is
   virtual; real rotator motion happens only inside operator-started
   sessions, in the P2 workflow step).
   *Amended 2026-07-29 (P3 design, settled interactively): sync is now
   **rejected**, not accepted-and-ignored — `CanSync = false`, sync verbs
   return `NOT_IMPLEMENTED`. A virtual device has no pointing model to
   correct; rejection also eliminates the sync-induced below-horizon
   wedge P3a discovered. Sync-carries-no-intent stands unchanged.*

   *Amended again 2026-07-30 (P3b, settled interactively) — **full
   reversal: Align (sync) is now the sole add-target gesture; GoTo
   never imports.** P3b proved SkySafari horizon-gates **both** GoTo
   forms (object and coordinate entry) unconditionally, under every
   horizon display setting — a GoTo gesture would restrict planning to
   currently-risen targets, defeating couch planning — and the P3a
   wedge makes GoTo availability client-state-dependent on top (it did
   not even reproduce on a second device running the identical build).
   Align is never gated. `CanSync = true`; sync verbs validate, import,
   and set the virtual pointing; slew verbs remain accepted as
   simulated motion only. Sync-carries-no-intent is superseded for
   this virtual device: imports land paused in the inbox, so a casual
   Align costs one discardable pending row, and proximity dedup
   collapses repeats. The operator workflow is Center (visual framing,
   display-only) → Align (import). Full contract:
   [planetarium-bridge.md](../../services/planetarium-bridge.md)
   § Align is the import gesture.*
3. **Captured targets land paused (`active: false`) with default goals, and
   the bridge never mutates operator-owned state.** Planetariums say
   *where*, never filters/exposures; the operator reviews in the P4 inbox.
   Dedup rules, fixed after review:
   - Bridge dedup is **coordinate-proximity only** (configurable, default
     30 arcsec — comfortably below any mosaic panel spacing). The
     `catalog_ref`-match branch of rp-targets' upsert rule is **never** used
     for bridge imports: two GoTos 15 arcmin apart that both resolve to the
     same catalog name are two targets (mosaic panels), not one.
   - A repeated GoTo within tolerance upserts **only** a target that is
     bridge-originated, still pending, and unedited since import. Targets
     that are active or operator-edited are never modified and never
     re-paused; a nearby GoTo then creates a new pending target with a
     suffixed slug. This must be part of the P1 create-tool contract, not
     bridge-side courtesy.
   - The bridge stamps provenance (source app/client address, receipt time)
     into the target's `notes`. Single-operator use is the MVP assumption;
     provenance makes multi-client confusion diagnosable, not prevented.
     *Amended 2026-07-29 (P3 design, settled interactively): provenance is
     additionally **typed** — `Target` gains `created_by`/`updated_by`
     writer-identity fields beside the existing timestamps, stamped
     `"operator"` by the operator-surface tools and `source.kind` by
     imports. "Bridge-originated, pending, unedited" becomes the
     first-class predicate `!active && updated_by == "planetarium-bridge"`;
     the `notes` line stays as human-readable display data, never parsed.
     Full contract: [planetarium-bridge.md](../../services/planetarium-bridge.md)
     § rp-side contract.*
   - The protection cuts both ways: rp-targets.md's slug-allocation rule 3
     treats *same `catalog_ref`* as "same object → in-place edit", so a
     later **manual catalog add** of "NGC 7000" would clobber a framed
     bridge import carrying that `catalog_ref` with the catalog centroid.
     P1's reconciliation of the same-object rule must therefore also gate
     the catalog_ref branch on coordinate proximity (differing beyond
     tolerance ⇒ suffix-allocate a new slug, never in-place edit),
     protecting framed targets from *all* writers, not just the bridge.
4. **Naming by reverse cone-search — a new `rp-catalog` capability.**
   *(Amended 2026-07-29, P3 design, settled interactively: the
   cone-search and all naming run **rp-side at add-time** — atomic
   against the store — not in the bridge, which sends bare ICRS
   coordinates plus `source`. The rules below are unchanged; only where
   they execute moved. See
   [planetarium-bridge.md](../../services/planetarium-bridge.md) § Naming.)*
   No name crosses the Alpaca wire. The bridge resolves the nearest catalog
   object within a tolerance (configurable, default ~10 arcmin; nearest by
   angular separation wins on ties) for `display_name`/`catalog_ref`.
   *(Amended 2026-07-29: the search is **one query with class tiers** —
   a deep-sky hit (10′ cone) outranks any star hit (2′ cone — the P3a
   tap-anchor gesture) regardless of separation, so field stars never
   take names from nebula framings; star-layer data (HD depth) plus the
   astrophoto DSO catalogs are tracked as issue #767, decoupled because
   naming is display-only.)* No hit at all yields
   a coordinate-derived slug (e.g. `j0042p4116` — the exact slug
   scheme is settled in P1, which owns slug allocation). The naming
   tolerance affects **display only** and never drives target identity
   (Decision 3). Because multiple framings of one object are first-class,
   the generated `display_name` disambiguates by **offset from the
   catalog centroid**, not raw coordinates: `"NGC 7000 +8′E −4′N"` reads
   as *how this framing differs*, which is what the operator composed
   (the cone-search already computes the separation vector). The plain
   name is kept when this is the only target for that object and the
   offset is within the dedup tolerance — a dead-center import stays
   `"NGC 7000"`, not `"NGC 7000 +0.3′E"`. Unresolved targets get a
   coordinate display name matching their slug shape (`"J2059+4432"`) —
   with no reference object, raw coordinates are the right display.
   All of this is an initial value only: `display_name` stays freely
   operator-editable (`"NGC 7000 panel NW"`), and the exact received
   coordinates live in the target row and provenance notes regardless. `rp-catalog` currently has only name→coords lookup
   (`Catalog::resolve`); the coordinate-indexed nearest-neighbor query is
   explicit P3 scope, documented with the service design doc. This is a
   linear scan over the embedded ~13k-row catalog (microseconds at this
   size) — deliberately *not* the DB-seeded indexed cone-search browse
   that rp-targets.md defers; the two must not be conflated.
5. **Position angle is a three-layer fallback, homed per optical train.**
   New field `position_angle_degrees: Option<f64>` on the `rp-targets`
   `Target` (degrees east of north, sky frame). Effective angle =
   target value → the imaging train's
   `equipment.optical_trains[].default_position_angle_degrees` → 0.0
   (north-up). The default is **per-train, not global**: a camera's fixed
   mounting angle is a physical fact of one train (see
   [optical-trains.md](../optical-trains.md)), and a rig with two rotator-less
   trains can carry two different angles. Rotator-less use: set the train
   default once, dial the same angle into the planetarium's FOV indicator,
   frames match. Resolution happens at read time by design: for a
   rotator-less train the config documents physical reality, so re-mounting
   the camera *should* reinterpret inherit-default targets; per-target
   explicit angles freeze framing and are never reinterpreted. With a
   rotator in the train, `get_next_target` returns the effective angle and
   the `deep_sky` workflow moves the rotator (existing `move_rotator` verb,
   sky frame) after slew/centering. SkySafari cannot export its
   FOV-indicator angle through any channel (verified: Alpaca device types
   are telescope + camera only, in both v7 and v8), so per-target angles
   are entered in the inbox.
6. **The bridge is a standalone first-party MCP client of rp, built per
   [ADR-017](../../decisions/017-standard-mcp-client-construction.md).** It
   uses the `rp-mcp-client` crate with the D6 observatory credential and
   TLS trust — there is no unauthenticated MCP carve-out on rp, and doctor
   wires the bridge's `service_auth`/`ca_cert` like any other client (the
   crate's connect-unauthenticated-with-loud-warning degrade is a
   misconfiguration signal, not a supported mode). Note this is a **new
   component shape**: not an orchestrator plugin (rp never invokes the
   bridge, it contributes no tools, it is not supervised by rp) — the
   plugin machinery is simply not involved. It is never on the imaging
   path.
7. **Stellarium and CdC are richer and come later.** Their servers
   (RemoteControl HTTP :8090, CdC TCP :3292) deliver what Alpaca cannot —
   object names, Oculars/mosaic rotation angles, per-panel mosaic frames —
   so P5/P6 use those as the data plane (Stellarium's telescope-protocol
   goto on :10001 remains only as the intent doorbell, with enrichment
   queried back from the sender's own IP). Both apps can also use the P3
   Alpaca device unenriched in the meantime.
8. **Milestone P3a is a sanctioned verification spike (ADR-005 precedent)
   that gates the build-out.** SkySafari's Alpaca client behavior is
   undocumented, so P3a — throwaway logging-skeleton code, exempt from the
   design-first/BDD-first order exactly as the plate-solver spike was —
   answers, against a real SkySafari install: discovery and connection
   lifecycle; whether `EquatorialSystem` J2000 is honored (noting the
   answer may be *per-install configuration*, not a per-version constant —
   the bridge therefore also gets an `assume_epoch` config override);
   which slew/sync verbs are sent; the position-report cadence needed to
   look connected; and the **go/no-go question** of arbitrary-point GoTo
   (see Goal). Findings land in docs/services/planetarium-bridge.md before
   Phase 1 design of the real device begins.
9. **P1 must not regress shipped altitude gating.** Today's planner
   eliminates targets below `min_altitude_degrees` (rp.md, Dynamic Planner
   v1); rp-targets.md defers *general* constraint enforcement to
   post-MVP. Fixed requirement, now landed: the P1 migration kept altitude
   elimination working against the store from day one — `get_next_target`
   gates on the active store rows (the legacy `targets[]` array it once read
   is removed, so gating is store-only) — only the not-yet-shipped
   constraints (moon separation/illumination, meridian window) remain
   deferred. Without this, an imported target could be
   imaged below the horizon profile today's system already respects.
   This deliberately amends rp-targets.md's deferred list (altitude
   gating is *not* new ephemeris work — the shipped v1 planner already
   evaluates it via `rp-ephemeris`); P1's Rule-2 update to rp-targets.md
   records the amendment rather than silently overriding the doc.
10. **P1 ships a minimal operator surface so P3-imported targets are never
    stranded.** The 6 CRUD/goals MCP tools (`add_target`/`get_target`/
    `list_targets`/`update_target`/`set_goals`/`delete_target`) are
    implemented against the new store, giving list/edit/activate before
    the P4 inbox exists. Default acquisition goals are **rp-owned
    policy** (a `target_store.default_goals` config in rp, applied by the
    create tool when the caller supplies none — not bridge config), and
    goal filter names are validated against the configured filter
    roster at create/edit time so a template referencing a filter the
    rig lacks fails at add, not mid-session.

11. **`capture` threads target identity from orchestrator state — `rp`
    has no session-side "current target."** File naming's `{target}`
    token (rp-targets.md's naming template; rp.md § Target Store) needs
    a target slug at capture time, but `slew`/`capture`/`auto_focus`
    are not target-aware today: they take raw coordinates or operate
    on a train/camera, never a target reference, and `rp` tracks no
    per-session "current target" of its own. Fixed for P1: `capture`
    gains an optional `target` (slug) parameter, sourced from
    orchestrator workflow state the caller already holds —
    `session-runner`'s blackboard already writes
    `session.target_name`/`target_ra`/`target_dec` right after every
    `slew` (`deep_sky.json`) and already re-supplies `target_name`
    explicitly to `record_exposure` one workflow step after `capture`
    runs. This is not a new subsystem; it is Decision 5's own
    threading idiom (`get_next_target`'s effective position angle,
    carried through the blackboard into a later `move_rotator` call)
    applied one tool call earlier, to the same blackboard state the
    workflow already tracks. When supplied, `capture` resolves the
    slug against the target store and denormalizes
    `slug`/`display_name`/`ra_hours`/`dec_degrees` onto the exposure
    document's `target` field — a field the document schema has
    documented since before this plan but that no code path populates
    today.

    **Settled 2026-07-23 (interactively, during P1 Phase 3
    implementation), superseding the "left open" note above:** `capture`
    gains a `frame_type` (`Light`/`Dark`/`Flat`/`Bias`) parameter
    alongside `target` — omitted, `capture` keeps today's flat
    `<doc_uuid_8>.fits` behavior unchanged (the fallback for calibration
    frames and any orchestrator not yet updated). `frame_type: Light`
    requires `target`. `Dark`/`Flat`/`Bias` use a **reserved slug equal
    to the lowercased frame type** (`"dark"`/`"flat"`/`"bias"`) when no
    explicit `target` is supplied — a shared bucket per calibration
    type, with an explicit `target` still accepted for a future
    per-target flat-capture flow (needed if a rig's rotator can't
    reliably repeat position, so flats must be retaken per target
    rather than shared across a night). `{filter}`/`{filter_position}`
    render the resolved train's live filter wheel reading for
    `Light`/`Flat`, and the fixed literal `"NA"`/`0` for `Dark`/`Bias`
    (always) or any frame type on a train with no filter wheel. Full
    rules: rp.md § Capture Tool Details; rp-targets.md § File-naming
    template.

    **Deferred, explicitly not decided here:** organizing `auto_focus`'s
    and `center_on_target`'s internal diagnostic captures through this
    same mechanism. Unlike calibration frames these can run multiple
    times against one target in a night, which needs a directory shape
    that doesn't exist yet (e.g. `_diagnostics/<train>/auto_focus/...`)
    and a naming-template token finer than `{night_date}` — no `{time}`
    token exists today. Both tools keep calling `capture` with
    `frame_type` omitted until this is designed as its own follow-up.

12. **P1 also carves out `rp-vocabulary` and makes plan-data validation
    unrepresentable-when-invalid ([ADR-019](../../decisions/019-plan-data-vocabulary-and-validation.md),
    [`rp-vocabulary.md`](../../crates/rp-vocabulary.md)); settled 2026-07-23,
    interactively.** Reviewing the P1 store surfaced validation *drift*:
    ICRS coordinate bounds were duplicated in two places and **missing on
    the store-write path** (`add_target`/`update_target` accepted raw
    `f64`, so a store target could hold coordinates the legacy `targets[]`
    path rejects); `Binning`'s round-trip was split across three modules in
    two crates; and exposure had two disagreeing string encodings. The fix
    is structural, not "call the validator more": a new zero-dependency
    leaf crate `rp-vocabulary` owns the shared plan value types
    (`IcrsCoord`, `Binning`, `FrameType`) as parse-don't-validate
    newtypes, and `Target` + `PlannerTarget` change
    their coordinate fields to a validated `IcrsCoord` — so the compiler
    forces every construction (today's and future) through the one
    validator, closing the store-write gap *by construction*.
    `rp_catalog::ResolvedTarget` adopts `IcrsCoord` too, giving **one**
    validated coordinate type across the plan pipeline (catalog → store →
    planner); `rp-ephemeris` keeps its own computed, `NaN`-capable
    `IcrsCoord` (a false cognate) and bridges to the plan type with
    `From`/`TryFrom` rather than being unified. The crate also
    seeds a plan-data **schema + validate** protocol (a plan-side analogue
    of the `config.get`/`config.schema`/`config.apply` machinery in
    `rusty-photon-config`): it supplies the validating constructors and a
    feature-gated `JsonSchema`, `rp` owns the `schema`/`validate` MCP tools
    and the dotted-`FieldError` mapping, so every current and future surface (UIs,
    grading/mosaic tools, the P3 bridge) validates identically instead of
    re-implementing the rules. Free to do now — the store and tools are new
    in this PR with no shipped caller, so it is a pure internal refactor
    before the wire ossifies into a contract. **Not this crate:** the
    file-naming template *engine* stays in `rp`; exposure is a plain
    `Duration` (named `exposure_duration`, standardized on one humantime
    encoding — bare "exposure" is ambiguous), not a value type; ASCOM
    camera **binning** and mount **pointing** types are false cognates
    (driver contract, not plan vocabulary); and a shared
    `rusty-photon-units` driver foundation is left to ADR-006's own trigger
    (a third pointing device).

## P3 sketch: `planetarium-bridge`

*(Superseded 2026-07-29: the authoritative design is now
[docs/services/planetarium-bridge.md](../../services/planetarium-bridge.md).
The sketch below is kept as the pre-design record; where they differ —
the import gesture (Align since P3b, not GoTo), rp-side naming, the
reported-position altitude floor — the design doc wins.)*

- New service `services/planetarium-bridge`, port **11126** (next free in
  the driver band), standard scaffolding per
  [service-lifecycle.md](../../skills/service-lifecycle.md):
  `ServiceRunner`, `resolve_and_init` config bootstrap (Alpaca `UniqueID`),
  `pkg/doctor.toml` (`class = "alpaca"`), workspace/Bazel registration —
  plus updates to the hand-typed port tables (workspace.md, packaging
  docs, doctor.md).
- Serves one Alpaca `Telescope` device via `ascom-alpaca` (server feature).
  The crate provides no state machine — every mutating member defaults to
  `NOT_IMPLEMENTED` — so the device implements the full ASCOM contract the
  way `star-adventurer-gti`'s telescope does: `AtPark` gating on every
  motion verb, `Target*` property propagation on slew/sync, a coherent
  `Slewing`/`Tracking` state machine with simulated convergence, sidereal
  time/alt-az derived from rp's site config. `EquatorialSystem` = J2000.
  Device name/description state loudly that this is a **virtual
  target-entry device, not a mount**; a doctor check fails provisioning
  when rp's `equipment.mount` points at the bridge's port (the
  fake-mount-as-real-mount misconfiguration would defeat every motion
  safeguard rp believes it has). ConformU-clean via the existing
  mock-backend pattern (`bazel test --config=conformu`).
- On GoTo: optional precession to ICRS (per P3a findings / `assume_epoch`)
  → reverse cone-search (Decision 4) → create-or-update per Decision 3 via
  the P1 target-create MCP tool (working name `add_target`; final name is
  P1's to settle), goals defaulted by rp per Decision 10. rp unreachable ⇒
  targets spool to a **bounded on-disk queue** in the service data
  directory, replayed with backoff on reconnect and across bridge
  restarts; when the bounded spool overflows, oldest entries are dropped
  *with an error log and a sentinel-visible counter* — "never drop
  silently" means observable, not infallible.
- BDD: drive the device with the `ascom-alpaca` *client* feature (same
  crate, same pattern the other drivers use for their harnesses) plus a
  stub rp MCP server; scenarios for goto→add, sync-ignored,
  dedup-upsert of pending-unedited targets, active/edited targets never
  mutated, mosaic-spaced GoTos staying distinct, unresolved-name slugs,
  offset display names (plain when unique/centered, `+8′E −4′N` when
  offset or multiple, `J2059+4432` when unresolved),
  rp-outage spooling and replay-after-restart, epoch handling.

## P4 note: inbox specifics settled by review

The PA field must distinguish "inherit train default" (blank) from
"explicit 0° north-up" — `Option<f64>` carries the distinction; the form
must not collapse empty-string and `"0"`. The inbox flags goals whose
filter names fail roster validation (Decision 10) and shows provenance
(Decision 3).

## Channel reference (research summary, 2026-07-22)

| App | Channel | Payload | Rotation angle |
|---|---|---|---|
| SkySafari 7/8 | Alpaca Telescope GoTo (P3) | exact RA/Dec only | none — FOV angle not exported via any channel |
| SkySafari 5/6 | LX200-over-TCP :4030 (+SkyFi UDP :4031) — rejected | RA/Dec only, JNow, protocol quirks | none |
| SkySafari (files) | `.skylist` — rejected for framing | catalog names only, no coordinates | none |
| Stellarium | RemoteControl HTTP :8090 (P5 data plane); telescope protocol :10001 (P5 doorbell, 20-byte LE fixed-point J2000 goto) | selected object name + J2000 RA/Dec (JSON) | `Oculars.selectedCCDRotationAngle` via StelProperty API |
| Cartes du Ciel | TCP :3292 server, CCDciel client pattern (P6); `GETCHARTEQSYS` for epoch | pushes name + RA/Dec (+catalog `pa:`) on selection | mosaic `GETFRAMES` frames carry true per-panel framing PA |
| LiveSky | — rejected | no public API; cannot export lists | — |

Sources: Stellarium RemoteControl API (stellarium.org/doc/head/remoteControlApi.html),
Stellarium telescope protocol v1.0 (free-astro.org mirror of
Stellarium_telescope_protocol.txt), CdC server commands
(ap-i.net/skychart/en/documentation/server_commands) and CCDciel
`cu_planetarium_cdc.pas` (github.com/pchev/ccdciel), INDI `skysafari.cpp` and
AlpacaScope (github.com/synfinatic/alpacascope) for the rejected LX200 path,
SkySafari 8 Pro product/App Store pages (skysafariastronomy.com,
store.simulationcurriculum.com) for Alpaca device-type coverage, ASCOM
ITelescopeV3 docs (ascom-standards.org/newdocs/telescope.html) for
sync-vs-slew and AtPark semantics.

## Open questions (carried into P3a) — ANSWERED 2026-07-29

All three were answered by the P3a session; full findings in
[planetarium-bridge.md](../../services/planetarium-bridge.md).

- ~~Does SkySafari honor a device-declared J2000 `EquatorialSystem`?~~
  **Yes — J2000 honored** (five GoTos matched J2000 to arcseconds; read
  once at connect; no per-install epoch setting exists, so this is
  client behavior, with `assume_epoch` kept as insurance).
- ~~Can the SkySafari UI GoTo an arbitrary point?~~ **GO, with
  caveats**: not by tapping empty sky, but Search → coordinate entry
  GoTos arbitrary points (the entry box is implicitly JNow, converted
  to J2000 on the wire), and the selectable catalog is deep enough that
  a faint star sits within arcminutes of any intended frame center.
  New constraint discovered: GoTo is client-side **horizon-gated** —
  below-horizon targets cannot be imported until they rise, and a
  reported position lingering below the horizon wedges all GoTos
  (bridge reported-position policy must prevent this).
- ~~Minimum position-report cadence?~~ **1 Hz** poll cycle
  (`Slewing`/`Tracking`/`RightAscension`/`Declination`); a 3 s
  simulated convergence was accepted cleanly.

*P3b correction (2026-07-30): the second answer's "coordinate entry is
not horizon-gated" held only because the observed case reached an
above-horizon point — below 0°, **both** GoTo forms are refused under
every horizon display setting, which is what flipped the import
gesture to Align (Decision 2's second amendment; findings in
[planetarium-bridge.md](../../services/planetarium-bridge.md) § P3b
appendix).*
