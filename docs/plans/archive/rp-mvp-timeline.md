# The rp service's MVP timeline, March 2026

A record, moved whole from `docs/skills/development-workflow.md` on
2026-11-01: how the rp service's MVP went through the design-first,
test-first workflow that skill describes, phase by phase.

The rp service followed this workflow. The full commit history
([PR #41](https://github.com/rusty-photon/rusty-photon/pull/41)) shows the
phases clearly:

## Phase 1: Design (8 commits, Mar 3-4)

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

## Phase 2: BDD tests (1 commit, Mar 6)

Two days after the design stabilized, the full BDD test infrastructure was
scaffolded in a single commit (2,031 insertions):

- 4 feature files, 22 scenarios, 136 steps
- OmniSim Docker management for Alpaca device simulation
- In-process webhook receiver for event capture
- Configurable test orchestrator for session lifecycle testing

## Phase 3: Implementation (1 commit, Mar 8)

Two days after the tests, all 7 core modules were implemented in a single
commit (1,230 insertions). All 22 BDD scenarios passed.

## Phase 4: Stabilization (26 commits, Mar 8-17)

The longest phase addressed real-world CI and cross-platform concerns:

- OmniSim Docker replaced with native process for cross-platform CI
- Windows compatibility (temp_dir paths)
- macOS compilation fixes (trait solver overflow)
- Race condition fixes (port TOCTOU → two-phase ServerBuilder, session restart)
- Coverage setup for child-process BDD tests (graceful shutdown + SIGTERM)
- Sanitizer workflow fixes (env var isolation for OmniSim, per-package matrix)
- Error scenario expansion (22 → 46 scenarios with negative test cases)

## Key takeaway

The design and test phases (10 commits) produced a clear specification.
The implementation phase (1 commit) was fast because the behavior was
already fully defined. The stabilization phase (26 commits) was where CI,
cross-platform, and edge-case work happened — but the core design never
changed.
