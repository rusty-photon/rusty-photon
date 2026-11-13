# Living documents describe the present — the cleanup behind AGENTS.md rule 16

## Goal

AGENTS.md rule 16 and [workspace.md § Living documents and
records](../workspace.md#living-documents-and-records) divide the
repository into living documents, which describe the system's current
or future state in the present tense, and records, which describe a
moment and are never rewritten. The tools crate's bare-reference guard
(`tools/rusty-photon-ci/src/bare_refs.rs`) holds the code's comments to
the first consequence of that rule — no issue or pull request named by
bare number — and its `PENDING` table lists every number the comments
still name. The rule and the guard are in place
([rule](https://github.com/rusty-photon/rusty-photon/pull/1688),
[guard](https://github.com/rusty-photon/rusty-photon/pull/1694),
both for
[#1282](https://github.com/rusty-photon/rusty-photon/issues/1282)).
The living documents are not yet written to it: the comments, the
service and crate design docs, the skills, the references and the
READMEs still narrate how the system got here and point at the tracker
in place of saying what is true.

The outcome of this plan: `PENDING` is empty and stays empty; no living
Markdown file names an issue or pull request by bare number, except the
four quoted examples in workspace.md (one in § Living documents and
records, three in § Comments) that show what not to write; the code
files the guard does not read are clean too; every passage of history
that lived inside a living document is a record in
`docs/plans/archive/`, opening with the line that says what it is and
where it came from; and the knowledge the
bare numbers used to stand for is stated once, where a reader would
otherwise get it wrong, with a link to a record only where the text
would otherwise be disbelieved — and a comment or passage whose only
content was history is gone, not reworded.

## Census

Taken 2026-11-02 on `main` at `9dc7de37`, the merge of the guard. The
guard's numbers are exact — its test fails when they drift — and the
rest comes from one scan of the tree with the guard's own matcher: `#`
followed by two to four digits, the first not 0, not preceded by `&`,
not followed by a word character or a hyphenated suffix. A link's text
and an `owner/repo#N` reference count, and so does a `#` inside a URL,
which the scan reports apart.

| What | Where | Count |
|------|-------|-------|
| Bare numbers in comments the guard reads (Rust, TOML, Starlark, workflow YAML, the actions' JavaScript) and in feature files | the `PENDING` table | 286 occurrences, 202 rows, 120 files |
| Bare numbers in code files the guard does not read | `crates/rp-catalog/scripts/*.py` (6 in 4 files), `services/svbony-camera/pkg/rusty-photon-svbony-sdk-install` (2), `installer/Package.wxs` (1), `.cargo-husky/hooks/pre-commit` (1), `tools/bazel-cache-worker/src/cache.js` (1) | 11 occurrences, 8 files |
| Bare numbers in living Markdown | 41 files; the heaviest are `docs/services/star-adventurer-gti.md` (72), `docs/services/doctor.md` (55), `docs/services/svbony-camera.md` (29), `docs/skills/testing.md` (26), `docs/services/rp.md` (17) | 293 occurrences: 64 of them are the text of a link to a record (`[#607](url)`, `[PR #1298](url)`; 4 of the 64 point at another project, `[nodejs/undici#5360](url)`), which the guard's definition counts as bare too; 4 are workspace.md's quoted examples; 3 are the `#` of one upstream URL, cited three times in `crates/qhyccd-rs/README.md`, which no batch rewrites and the final scan excepts; 1 is the stderr label `[sentinel#12]` that testing.md shows. The 286 the batches take equal the `PENDING` total by coincidence |
| Bare numbers in records | archived plans, active plans, ADRs, validation records, changelogs | 1136 occurrences, 103 files — **not touched** |
| Full-URL citations of this repository's issues and pull requests in living documents and comments | 24 files; 20 in `docs/services/doctor.md` — three of them section banners, the rest inline citations and the labels of its MVP-scope list | 68 |
| Record links the matcher does not see: ADR ids (`ADR-019`) and paths to plans (`docs/plans/archive/svbony-camera.md`), in comments and in living Markdown | scanned 2026-11-11 on `main` at `7b660137`; the heaviest are `docs/workspace.md` (44 ADR ids and 22 plan paths, most of them its Decisions and Plans index rows, which stay), `docs/services/zwo-camera.md` (24 and 14), `docs/services/svbony-camera.md` (20 and 18), `docs/services/rp.md` (11 and 28) | ADR ids: 231 in comments the guard reads (138 files), 13 in code files it does not, 337 in living Markdown (49 files). Plan paths: 116 in comments the guard reads (93 files), 3 in code files it does not, 210 in living Markdown (50 files). Not in the batch counts below: a batch counts its own when it starts |
| Lines whose wording suggests narration ("used to", "previously", "no longer", "the old …", "retired", "legacy", "Copilot", "before this" …) | 261 files; the heaviest are `star-adventurer-gti.md` (41), `rp.md` (34), `qhy-camera.md` (28), `svbony-camera.md` (23), `services/qhy-camera/src/camera.rs` (20) | 857 candidate lines; about one in six is narration, by a sample of 45 Rust comment lines |

The narration count is a ceiling for triage, not a target: "retired" and
"legacy" name things the code handles today as often as they explain a
change, and "used to" is mostly purpose. It is a floor for reading, not
a scope: in the root `Cargo.toml` the grep flagged three lines and
reading every comment found a dozen more, the first of them indented
under the member list where a `^#` grep never looks. Each batch reads
every comment in its files by hand and is never gated on the grep.

By 2026-11-11 the `PENDING` table holds 281 occurrences in 198 rows and
117 files: one row went with the trial slice of B9 (the root
`Cargo.toml`, in the status table), three rows and four occurrences
with work on `services/rp` and `services/focus-model` under the rule,
which is how every batch is expected to shrink before it starts. A
batch counts from the table as it stands.

### Records inside living documents

These passages are history by construction — dated phase tables,
"landed" banners, probe logs, spike findings — and move out whole rather
than being rewritten in place. The batch that owns the file confirms the
list; the census found:

- `docs/services/star-adventurer-gti.md` § Phase status with § Phase 4
  findings and § Phase 4 driver-logic changes, and the dated probe log
  under § Real-hardware validation (eight dated entries, 2026-05-16 to
  2026-10-07, most of them probes rather than ConformU runs and so
  without a record in `docs/validation/`).
- `docs/services/svbony-camera.md`'s banner stack at the top of the file
  (two validation passes, five "Follow-up landed" banners, three "Phase …
  landed" banners), § Delivery phasing (a mirror of the archived plan's
  phases) and the dated subsections of § Real-hardware validation, whose
  ConformU runs are already in `docs/validation/`.
- `docs/services/zwo-camera.md` § Delivery phasing, a mirror of its
  plan; `docs/services/qhy-camera.md` § Delivery phasing (E→C), which
  mirrors no plan — the service had none in `docs/plans/`, so this
  section is the only account of its delivery and moves out whole as a
  record; `qhy-camera.md` § Implementation notes (v0 deviations from
  the original design), three hundred lines of "how" decisions framed
  against a design that is no longer the one above them — the decisions
  that still describe the service are folded into the sections they
  belong to, the framing goes to the record.
- `docs/services/phd2-guider.md` § Implementation Phases, a checkbox
  plan that never lived in `docs/plans/`.
- `docs/services/planetarium-bridge.md`'s two appendices of dated spike
  findings (P3a 2026-07-29, P3b 2026-07-30).
- `docs/skills/coverage.md` § History: the Codecov era — its three
  lessons stay as present-tense gating facts, the era does not.
- `docs/services/doctor.md` § MVP scope, written as a list of delivered
  issues, and `docs/services/rp.md`'s account of the OmniSim fork's
  `467.x` releases under § BDD Tests.
- `docs/services/zwo-focuser.md` § Real-hardware validation, one dated
  run (2026-07-14) that no record in `docs/validation/` holds — the
  focuser's departure run of 2026-10-07 has one, this run does not. The
  file is in no subsystem batch, so B9 takes it with the other files no
  batch names.

## Method

Every batch follows the same steps. The rule's text is in
[workspace.md § Comments](../workspace.md#comments); this is how it is
applied at scale.

1. **The design doc first, then the code.** A comment that pointed at an
   issue will point at a design-doc section instead, so the section has
   to exist and be clean before the comment is written. A batch with
   both is one pull request where the doc is small and two where it is
   not; the doc's pull request goes first.

2. **Is the comment needed at all?** Before a number is classified or a
   sentence reworded, the batch asks of every comment and every passage
   in its files — not only the lines the grep flagged — what question
   it answers that a reader of the line, or of the section, would
   otherwise get wrong. For a manifest line that is an exact or
   held-back version, a non-default feature set, a git source, a floor
   a reader would lower, a profile knob; for code, why it is not the
   obvious thing. The batch keeps that answer, as short as it gets, and
   deletes the rest. A comment whose only content was how a setting was
   reached has nothing left to say once the history goes: deleting it is
   the fix, and rewording it into the present tense only restates the
   line beneath it or justifies a non-decision. What a thing is for goes
   when its name and its documentation say it. Mechanism and process
   knowledge — how the lint groups are ordered, what a toolchain bump
   does to them, how a dual-homed crate inherits its versions — lives
   once, in the design doc or in workspace.md, and the comment points
   there only where the line looks wrong without it.

3. **Each citation is one of five things.**
   - *Noise* — `(issue #595)` after a sentence that is complete without
     it. Delete it.
   - *A pointer that carries the knowledge* — "the probe rejects the
     device on the other end of the wire (issue #254)". Replace it with
     the knowledge: what, on what evidence, and a link to the design-doc
     section that holds the reasoning.
   - *Rationale* — why the code is not the obvious thing. Keep it as a
     present-tense fact with the conditions of its measurement, no
     tracker: the Windows file-system flakes `MODULE.bazel` works around
     stay described, the six issue numbers in the description go.
   - *An upstream bug the code works around* — `axum #2673`,
     `nodejs/undici#5360`, `rules_rust #1120`, the `collect_coverage`
     pull requests. State the behaviour beside a full URL.
   - *A citation the text needs to be believed* — the code does
     something that looks wrong and the record holds the evidence. Keep
     it as a full URL or a `docs/` path. The 68 existing full URLs get
     the same test; a section banner never qualifies, because a banner
     names the section, not the issue that built it. A kept citation's
     link text is words — `[issue 607](url)`, or the behaviour it
     evidences — never the number: `[#607](url)` reads as bare to the
     guard and to the final scan alike, and so does an upstream
     project's `[nodejs/undici#5360](url)`, which becomes
     `[undici issue 5360](url)`.

   An ADR id and a `docs/` path to a plan, archived or live, are record
   links too ([workspace.md § Living documents and
   records](../workspace.md#living-documents-and-records) and
   [§ Comments](../workspace.md#comments)) and take the same test: most
   are noise beside a sentence that stands without them, or a pointer
   whose knowledge belongs in the design doc; one that is kept is
   written as its `docs/decisions/` or `docs/plans/` path. The
   Decisions and Plans index rows in workspace.md stay (decision 5).

4. **Narration becomes a fact or a counterfactual.** "Used to",
   "previously", "before #N", "the old …" frame the code against a past
   the reader does not have. Where the old behaviour is why a safeguard
   exists, say so in the counterfactual form — "without the manifest
   resolution the two doc-tests race" — not in the past tense, which
   turns a fixed bug into a live claim. Where it is not, state the
   constraint alone. "Retired" and "legacy" stay when they name what the
   code handles today (a retired config key `doctor` reports) and go
   when they explain a change. A claim that is true today and dated by
   its wording — "the current minor", "still compiles" — is made durable
   or deleted.

5. **A record inside a living document moves out whole.** The passage
   becomes a new file in `docs/plans/archive/` (decision 8) that opens
   the way the two non-plan records already there do
   ([`code-review-record-2026-10.md`](archive/code-review-record-2026-10.md),
   [`rp-mvp-timeline.md`](archive/rp-mvp-timeline.md)): one line
   saying it is a record, what it holds, and the file and date it was
   moved from — "A record, moved whole from `docs/services/<svc>.md` on
   <date>: …". The `Status: COMPLETE` line of
   [archiving-plans.md](../skills/archiving-plans.md) is for plans, and
   an extracted passage is not one. The text is
   moved, not rewritten, and never appended to an existing archived plan
   — a record is not rewritten, and that includes someone else's. The
   living document keeps one present-tense paragraph of what the passage
   established and links the record only where the paragraph would
   otherwise be disbelieved. Validation runs that are already in
   `docs/validation/` are linked there, not duplicated.

   Three searches come before the move, because the link check of
   step 11 finds none of what they find:

   - **A fact the passage alone states.** Each fact in the passage is
     looked for in the rest of the living document, and one stated
     nowhere else is stated where it applies before the passage goes.
     A record is where a reader checks a claim, not where they learn
     what the system does.
   - **A pointer the passage carries.** The passage's text is searched
     for "above", "below" and a section named without a link: any of
     them may point at text that stays behind.
   - **A name another document uses.** `docs/` is searched for the
     names the passage's parts go by — its heading, the headings under
     it, a bullet's bold lead — as words and not only as anchors. A
     record or a plan that names one in plain text holds a pointer that
     follows the passage, and an open plan may tell a later step to
     write into it (§ Out of scope).

6. **A copied comment is fixed in every copy, in one batch.** The
   OmniSim comments the services' `tests/bdd.rs` files copied — the
   loud-reset diagnostic in `rp` and `calibrator-flats`, the exit-code
   rule of testing.md § 2.7 in `pa-falcon-rotator` and `qhy-camera` —
   and the `acquire()` comments the serial drivers copied from the
   shared transport are the known cases; a batch that finds another
   fixes all of it. Before knowledge moves between a comment and a
   document, the batch greps for what names its present home: the
   test-scope allow list in the root `Cargo.toml` has 153 Rust files —
   crate roots, `tests/` entry files and three `src/bin/` tools — saying
   it is documented there, so it stays there until a change of its own
   moves it and every pointer. A slice of a batch cut by file names, in
   its pull request, the copies and pointers it leaves to the rest of
   the batch.

7. **Tests are named for what they pin.** A test named for a review
   comment or a pull request — `// Regression test for the Copilot
   review on PR #221` — is renamed for the failure shape, this input and
   that wrong output, and its comment describes that shape. Feature
   files are the contract: a scenario title or description that names an
   issue is reworded, never exempted.

8. **Dual-homed crates cite by URL.** `qhyccd-rs`, `zwo-rs` and
   `svbony-rs` are dual-homed: their crate tarballs carry no `docs/`,
   and docs.rs renders their doc comments without it, so a doc comment
   there links anything in this repository — a record it cites, a
   design-doc section it points at — by full URL.

9. **Stale status is fixed on the way.** A sentence that says a thing is
   pending, deferred or planned when the code has it is as wrong as a
   narrated past. The Services table in workspace.md has had such rows;
   the batch fixes what it finds in its files. A comment that defends a
   value the line does not need — an exact pin that holds nothing, a
   floor nothing in the workspace requires — is that value's own case
   against it: the value changes and the comment goes, with the owner's
   word where the value is a dependency requirement. A claim about a
   version, the dependency graph or a crate's consumers is checked
   against the lock (`cargo tree -i <crate>`) before it is kept, and so
   is its rewrite.

10. **The guard's rows go.** The batch deletes every `PENDING` row its
    files no longer bear out; `cargo test -p rusty-photon-ci bare_refs`
    names the rows it reached. A row is never added, a number the batch
    cannot clear is reworded, not re-listed, and a row follows its file
    when the file moves.

11. **Local checks.** The AGENTS.md rule 4 gate, and for a crate whose
    doc comments changed `cargo doc --no-deps -p <crate>` with
    `RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links"`: a doc comment
    that loses a link definition (`[issue #254][issue]`) breaks every
    reference that used it. A batch that extracts a record checks the
    moved text's own links, now one directory deeper, and every link
    that pointed into it, as archiving-plans.md steps 4 and 5 do. Any
    change to a `Cargo.toml`, comments included, stales
    `MODULE.bazel.lock` (AGENTS.md rule 10): `cargo ci repin-bazel-lock`
    before the commit, and the lock in the same commit.

12. **Pull request and review.** The pull request description lists the
    records the batch created and what each living document now says in
    their place; the review (AGENTS.md rule 14) rates a remaining breach
    in text the batch adds `likely · medium · fix`
    ([code-review.md](../skills/code-review.md)). The batch's row below
    gets its pull request — a slice of a batch, a row of its own — and
    the plan's row in workspace.md's Plans index its new status.

## Batches

Nine batches, each a subsystem whose code and docs share their numbers.
Code counts are bare-number occurrences in the guard's table plus the
files it does not read; doc counts are occurrences in living Markdown.
Both are the census of 2026-11-02, so a batch starts by re-running the
guard and the scan. B9's count leaves out the root `Cargo.toml`, done as
the method's trial slice (the status table below), so the Code column
sums to 296: 285 of the table's 286 and the 11 in files the guard does
not read.

| Batch | Scope | Code | Docs | Knowledge homes and records to extract |
|-------|-------|-----:|-----:|----------------------------------------|
| B1 | `services/star-adventurer-gti` (61), `crates/skywatcher-motor-protocol` (4); `docs/services/star-adventurer-gti.md` (72), `docs/references/skywatcher-motor-controller-command-set.md` (1), `docs/skills/hardware-validation.md` (1) | 65 | 74 | § Initialisation sequence holds why `:e1` is first; § Connection Lifecycle the startup gate and the moving-mount refusal; § PulseGuide lifecycle the send instant and the carried-forward sample; § Side-of-pier the selector; § Configuration the retired `dec_limits` and `flip_range_hours`. Records out: § Phase status with its Phase 4 findings; the dated probe log under § Real-hardware validation. The heading "Shared transport (Phase E of issue #257)" names the section. hardware-validation.md's sentence that the design docs "narrate what was learned" goes with the probe log: the record holds the run, the design doc states what the hardware does, and a first run on a platform is linked under § Real-hardware validation as the skill asks |
| B2 | `services/doctor` (37), `services/ui-htmx` (12, nine of them axum's SSE shutdown bug, cited once by URL and linked from the rest), `installer/Package.wxs` (1); `docs/services/doctor.md` (55, of which 20 are the link texts of its URL citations), `docs/services/ui-htmx.md` (2), `docs/packaging-windows.md` (3), `installer/README.md` (1) | 50 | 61 | doctor.md § Client-target joins, § TLS and auth and § ACME convergence hold the join rules; the banners of § Client-target joins, § ACME convergence and § The flip orchestrator lose their issue links. ui-htmx.md states the drivers map the service creates for itself, which doctor, the installer and packaging-windows.md all describe against its history. Record out: doctor.md § MVP scope as a list of delivered issues; the present-tense scope stays |
| B3 | `services/rp` (21), `crates/rp-guider` (5), `crates/rp-plate-solver` (5); `docs/services/rp.md` (17), `services/rp/tests/fixtures/star_detection/README.md` (1) | 31 | 18 | rp.md § `center_on_target` Contract holds the stalled-read timeout and the progress emission; § Equipment Integration the shared `ca_cert` and the per-client `auth`; the per-frame acquisition facts their own — today a bold paragraph under § Core Fields, which gets a heading if a comment is to link it. The two client crates' `BUILD.bazel` and `lib.rs` comments say what the field is for, not which issue added it. Record out: the OmniSim fork's `467.x` release account under § BDD Tests, whose present-tense half (what the fork adds and why) joins `docs/references/omnisim.md` |
| B4 | `crates/bdd-infra` (20), the OmniSim comments copied into `tests/bdd.rs` of `rp`, `calibrator-flats`, `pa-falcon-rotator` and `qhy-camera` (5: the loud reset, the exit-code rule, the park hang), `.github/actions/install-omnisim/omnisim.mjs` (4), the OmniSim lines of `.bazelrc`, `bazel.yml` and `services/rp/BUILD.bazel` (3); `docs/skills/testing.md` (26), `docs/references/omnisim.md` (9) | 32 | 35 | testing.md § 2.7 and § 5 and omnisim.md already hold the knowledge — the exit-code rule, the loud reset, serialised restarts, one private simulator per test process, `ServiceHandle`'s health read — so the comments link the sections and the sections drop the numbers; the flake families testing.md cites by URL are judged one by one; the stderr-label example `[sentinel#12]` names no issue, and B4 writes it with a one-digit sequence number so the final scan reads it as the label it is |
| B5 | `crates/rusty-photon-shared-transport` (17), `services/pa-falcon-rotator` (9), `services/qhy-focuser` (3), `services/dsd-fp2`, `services/pa-scops-oag`, `services/ppba-driver` (1 each); `docs/services/upbv2-driver.md` (4), `docs/services/gemini-powerbox-adv3-driver.md` (4), `docs/services/dsd-fp2.md` (4), `docs/services/ppba-driver.md` (2), `docs/services/falcon-rotator.md`, `docs/services/qhy-focuser.md`, `README.md` (1 each) | 32 | 17 | The shared transport has no design doc: the startup gate, the moving-mount refusal, the command lock's run-to-completion and the reconnect ownership live in `shared.rs` comments that cite issues. B5 writes `docs/crates/rusty-photon-shared-transport.md`, shrinks the Shared Crates row to point at it, and makes the adopters' copied `acquire()` comments link it. The "Relationship to issue #1173" sections in upbv2 and gemini state the open design question as a question; dsd-fp2.md's ConformU citation gets a worded link text (step 3) |
| B6 | `crates/zwo-rs` (6), `crates/svbony-rs` (2), `crates/qhyccd-rs` (1, the `Audit #11` internal numbering), `services/zwo-camera`, `services/qhy-camera`, `services/svbony-camera` (1 each), `services/svbony-camera/pkg/rusty-photon-svbony-sdk-install` (2); `docs/services/svbony-camera.md` (29), `docs/services/zwo-camera.md` (6), `docs/services/qhy-camera.md` (4), `docs/services/sky-survey-camera.md` (1), `docs/svbony-camera-windows-install.md` (1) | 14 | 41 | zwo-camera.md holds the per-sensor `MaxADU` and the departure measurements (`docs/validation/2026-10-06-zwo-camera-departure-linux/`, `docs/validation/2026-10-07-zwo-camera-departure-windows/`), svbony-camera.md the udev rule and the readout ladder. Records out: svbony-camera.md's banner stack and its dated validation subsections (the runs are in `docs/validation/`); svbony-camera.md's § Delivery phasing, which its archived plan holds, and zwo-camera.md's, which its live plan ([zwo-driver.md](zwo-driver.md)) holds, so one present-tense sentence replaces each — for zwo after the section's *Remaining* items are checked against the code (step 9) and the plan carries what is still open; qhy-camera.md's § Delivery phasing and § Implementation notes, which no plan holds, so both move out whole. The Windows install guide says what the MSI ships today. Dual-homed crates cite by URL |
| B7 | `services/phd2-guider` (7), `services/plate-solver` (6) with `plate-solver-smoke.yml` (2), `services/sentinel` (6), `crates/rusty-photon-tls` (4), `crates/rusty-photon-service-lifecycle` (2), `crates/rusty-photon-driver` (1); `docs/skills/service-lifecycle.md` (5), `docs/crates/rusty-photon-service-lifecycle.md` (4), `docs/services/plate-solver.md` (4), `docs/packaging.md` (2), `docs/crates/rusty-photon-setup.md`, `docs/services/phd2-guider.md`, `docs/services/sentinel.md` (1 each) | 28 | 18 | The lifecycle docs say what one `Shutdown` handle guarantees, in the counterfactual form, not what twelve services did before it. sentinel.md holds the degraded-answer rule, plate-solver.md the smoke test's pinned fixture and its timing guard. Record out: phd2-guider.md § Implementation Phases |
| B8 | `crates/rp-catalog` (3) with its `scripts/*.py` (6), `services/session-runner` (3), `services/focus-model` (2), `crates/rp-focus-curve` (1); `docs/services/planetarium-bridge.md` (4), `docs/services/session-runner.md` (4), `docs/services/focus-model.md` (3), `docs/crates/rp-focus-curve.md` (2) | 15 | 13 | The catalog's contents (Messier, NGC, IC, the star layer, the corrected coordinates) and its resolution are described once, in a new `docs/crates/rp-catalog.md` that the Shared Crates row points at (decision 10), and planetarium-bridge.md and the catalog scripts point there rather than at the issue that widened it; session-runner.md states the planner contract its three `rp` issues delivered; the focus docs cite the coarse-sweep measurement by its fixture README. Records out: planetarium-bridge.md's two spike appendices |
| B9 | `MODULE.bazel` (10), `.bazelrc` (7), `check.yml` (6), `bazel.yml` (1), `ui-browser-nightly.yml` (1), `.github/actions/lib/install.mjs` and its test (2), `.cargo-husky/hooks/pre-commit` (1), `tools/bazel-cache-worker/src/cache.js` (1); `docs/skills/raspberry-pi-runner.md` (3), `docs/skills/proxmox-runner-pool.md` (2), `docs/skills/babysitting-prs.md`, `docs/skills/ci-logic.md`, `tools/bazel-cache-worker/README.md` (1 each), the upstream link text in workspace.md's ascom-alpaca fork notes (1), plus `docs/skills/coverage.md` § History, the `rp-ephemeris` and `rusty-photon-i18n` archive links in workspace.md's Shared Crates rows (decision 10), and the narration candidates in the files no subsystem batch names (104 files and 240 candidate lines on 2026-11-11, `docs/references/qhyccd-sdk-manual.md` the heaviest, `docs/services/zwo-focuser.md` among them) | 29 | 9 | The Windows file-system rationale in `MODULE.bazel` and `.bazelrc` stays as rationale with its upstream URLs; the comments in `check.yml` and the pre-commit hook that explain the two clippy passes link [pre-push.md](../skills/pre-push.md), which already holds the reason. workspace.md's own history goes: § Holding a transitive dependency back describes a rust-embed hold at 8.11.0 that the lock does not have, and `.bazelrc`'s two E0463 root-cause notes rest on it; § Lints says how the lint groups were widened; the fork sentence under Build Notes says "the old path". Records out: coverage.md's Codecov era and zwo-focuser.md's dated run. B9 also closes the plan: the guard's doc comment and workspace.md § Comments say `PENDING` is empty and a new number is reworded |

B9 is last because it ends with the empty table; B1–B8 are independent
and run in any order, one at a time so that the guard's table has one
writer. The counts are the size of the grep, not of the work: B1's 139
occurrences are a few days of reading and rewriting with the design doc
open, and the narration triage in the same files is the larger half of
it.

## Decisions

Settled in the rule's pull request and the guard's, one in the method's
trial slice and five by the owner before the first batch; they hold
for every batch.

1. **A convention and an AGENTS.md rule, not a tenet and not an ADR.**
   The tenets say what the system does at night; the rule's record is
   its issue and pull requests.
2. **Records are not rewritten.** Commits, pull requests, issues, ADRs,
   validation records, archived plans, active plans in `docs/plans/`
   and the crates' `CHANGELOG.md`s keep every number they have. A plan
   is a record in the making and is archived whole when it completes.
3. **An ADR is superseded, never amended.** The superseding ADR names
   the old one; the old one's Status gains one line and the file moves
   to `docs/decisions/archive/` when superseded in full, and stays
   where it is when superseded in part. The ADRs already amended in
   place stay as they are.
4. **A record is linked only where the text would otherwise be
   disbelieved,** as a full URL or a `docs/` path. A link to a living
   document — a design-doc section — is not a record link and is what
   AGENTS.md rule 11 asks for.
5. **The Plans index rows stay as they are.** Each describes a plan, a
   record in the making, and may name the archived plan it grew out of.
6. **The guard's table only shrinks.** A number no row holds is a new
   breach, fixed in the comment.
7. **A comment is kept for what it answers, not reworded for how it
   reads.** The first pass over the root `Cargo.toml` kept all 27 of
   its comments and reworded them, and with the history gone the result
   read worse than before; the merged pass asks method step 2's question
   first and leaves 85 comment lines of 210, ten comments gone whole,
   ten shrunk, seven as they were.
8. **Extracted records go to `docs/plans/archive/`,** one file per
   passage, named for the subject and the span
   (`star-adventurer-gti-phase-record.md`,
   `svbony-camera-delivery-record-2026-07.md`), beside the two non-plan
   records already there. A `docs/records/` directory for records that
   were never plans would also be the natural home of
   `docs/decisions/archive/` and `docs/validation/`, a wider move than
   this plan; the question is reopened if the archive stops reading as
   one list.
9. **The ADR archival procedure is written when an ADR needs it.** No
   ADR is superseded in full: ADR-008 is superseded in part twice
   (ADR-010 supersedes its canonical-home decision, ADR-014 its
   unconditional link) and stays by decision 3. The pull request that
   first supersedes an ADR in full writes the ADR step into
   archiving-plans.md next to the plan step and creates
   `docs/decisions/archive/` with that first file; no batch of this
   plan does.
10. **`rp-catalog` gets a design page; the other two Shared Crates links
    to archived plans go.** Nothing in the `rp-ephemeris`, `rp-catalog`
    and `rusty-photon-i18n` rows needs "the original implementation
    plan" to be believed. The `rp-ephemeris` and `rusty-photon-i18n`
    rows point at a crate doc and a live plan, so B9 drops their links;
    `rp-catalog`'s link is the row's only pointer to a design, so B8
    writes a short `docs/crates/rp-catalog.md` and the row points there.
    The Plans index rows keep theirs (decision 5).
11. **A slice or phase label stays where something else uses it as a
    name.** `D2`–`D6b` in doctor.md, `E10`, `C1a` and `GO5` in the
    camera docs and `Phase 4` in the GTi doc are the names tests,
    validation records and the code use, and stay as names. "Phase E of
    issue #257" and a label nothing else uses go.
12. **The guard reads code comments only.** `bare_refs.rs` gets no
    Markdown reader, now or later: the review rating
    (method step 12) rates a breach in text a pull request adds a fix,
    which is the guard for the living documents, and B9's final scan is
    the check that they are clean when the plan ends.

## Definition of done

- `PENDING` in `tools/rusty-photon-ci/src/bare_refs.rs` is `&[]`, and
  its doc comment and workspace.md § Comments describe the guard as
  holding an empty table.
- A scan of every living Markdown file with the guard's matcher — a
  link's text included, a `#` inside a URL excepted — finds the four
  quoted examples in workspace.md (§ Living documents and records and
  § Comments) and nothing else; the eight code files the guard does not
  read are clean.
- Every passage listed under *Records inside living documents*, and
  every one a batch found, is a file in `docs/plans/archive/` opening
  with the record line of method step 5 — except a mirror of a plan
  that exists (svbony-camera.md's and zwo-camera.md's § Delivery
  phasing), which the plan already holds — and the living document it
  came from holds one present-tense paragraph in its place.
- Every ADR id and plan path left in a comment or a living document
  passes method step 3's test, or is an index row of workspace.md.
- `docs/crates/rusty-photon-shared-transport.md` and
  `docs/crates/rp-catalog.md` exist and their Shared Crates rows point
  at them; the `rp-ephemeris` and `rusty-photon-i18n` rows link no
  archived plan.
- Every slice or phase label left in a living document is a name that
  a test, a validation record or the code uses (decision 11).
- Every batch row in the status table names its merged pull request(s),
  and the plan is archived per archiving-plans.md.

## Out of scope

- Records: nothing in `docs/plans/`, `docs/plans/archive/`,
  `docs/decisions/`, `docs/validation/` or a `CHANGELOG.md` is edited,
  except the one Status line an ADR gains when superseded in full and
  a pointer from a record into a passage a batch moved — a link's
  target, or a section the record names in plain text — which follows
  the passage: where the pointer leads changes, what the record says
  does not. An open plan that tells a later step to write into a
  passage that moved says where that writing goes now.
- Moving the test-scope allow list out of the root `Cargo.toml`, where
  153 Rust files say it is documented: to workspace.md § Lints with
  every pointer updated, or into a `rusty-photon-ci` check that holds
  the one list. A change of its own.
- Dependency changes a batch surfaces — the `argon2` bump that would
  collapse the duplicate `blake2` and `digest` chains, a pin or a floor
  whose own comment says it does nothing (method step 9). Each is the
  owner's decision, taken in that batch's pull request or outside it.
- Extending the guard to Python, shell or `.wxs` files; a Markdown
  reader is not built at all (decision 12).
- A `docs/records/` directory (decision 8).
- The ADR step of archiving-plans.md and `docs/decisions/archive/`: the
  pull request that first supersedes an ADR in full writes them
  (decision 9).
- Rewriting rationale that is already present-tense and number-free
  because it is long: the Windows flake blocks in `MODULE.bazel` and
  `.bazelrc` lose their issue numbers and keep their length.

## Implementation status

| Batch | Status | Pull request |
|-------|--------|--------------|
| Plan | Merged 2026-11-12 | [the plan](https://github.com/rusty-photon/rusty-photon/pull/1696) |
| B1 — Star Adventurer GTi | Merged: the design docs on 2026-11-12, then the code comments and the guard rows | [design docs](https://github.com/rusty-photon/rusty-photon/pull/1763), [code comments](https://github.com/rusty-photon/rusty-photon/pull/1767) |
| B2 — doctor, ui-htmx, the installer | Not started | — |
| B3 — rp and its client crates | Not started | — |
| B4 — BDD infrastructure and OmniSim | Not started | — |
| B5 — shared transport and the serial drivers | Not started | — |
| B6 — cameras and SDK crates | Not started | — |
| B7 — guiding, solving, sentinel, lifecycle, TLS | Not started | — |
| B8 — catalog, planner, focus | Merged: the design docs on 2026-11-12, then the code comments and the guard rows | [design docs](https://github.com/rusty-photon/rusty-photon/pull/1758), [code comments](https://github.com/rusty-photon/rusty-photon/pull/1759) |
| B9 — build, CI, runners; the empty table | Not started | — |
| B9 — the root `Cargo.toml`, a trial slice of the method | Merged 2026-11-11 | [trial slice](https://github.com/rusty-photon/rusty-photon/pull/1744) |
