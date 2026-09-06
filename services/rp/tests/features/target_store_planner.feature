@serial
Feature: Planner altitude-gating parity against the target store (P1)
  Decision 9 (docs/plans/archive/planetarium-target-import.md) is a fixed P1
  migration requirement: `get_next_target` must keep eliminating
  targets below their altitude floor for targets that live in the
  rp-targets store (see planner.feature for the legacy config
  `targets[]` array coverage — `get_next_target` evaluates both
  sources together). The floor comes from
  `target.scheduling.min_altitude_degrees`, falling back to
  `targets.default_scheduling.min_altitude_degrees` from config — the
  same two-level per-target-then-default fallback the config-array
  planner already applies today (rp.md § Target Store, § Dynamic
  Planner).

  # The evaluation time is pinned to true astronomical night at this
  # site (matching planner.feature's equinox convention) — otherwise
  # the no-survivors branch could land on wait_for_twilight or
  # end_of_session depending on the real wall-clock the suite runs at.
  Scenario: A store-backed target below its per-target altitude floor is eliminated
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added a target named "Below Floor" at ra_hours 0.0 dec_degrees 0.0 with min_altitude_degrees 90
    When the MCP client calls "get_next_target" at time "2026-03-20T22:00:00Z"
    Then the tool call should succeed
    And the result reason should be "all_below_min_altitude"

  Scenario: A store-backed target with no override falls back to the configured default floor
    Given rp is configured with a target-store default minimum altitude of 90 degrees
    And a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added a target named "No Override" at ra_hours 0.0 dec_degrees 0.0
    When the MCP client calls "get_next_target" at time "2026-03-20T22:00:00Z"
    Then the tool call should succeed
    And the result reason should be "all_below_min_altitude"
