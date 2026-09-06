Feature: Target store CRUD (P1)
  P1 of the planetarium-target-import plan (docs/plans/archive/planetarium-target-import.md)
  replaces the config `targets[]` array with a redb-backed store
  (rp-targets crate) exposed over `add_target`, `get_target`,
  `list_targets`, `update_target`, and `delete_target` MCP tools (rp.md
  § Target Store). Coexists with the legacy `targets[]` planner tools
  (planner.feature) pending the Dynamic Planner cutover
  (target_store_planner.feature).

  `add_target` derives a slug from `catalog_ref` (a catalog lookup) or
  `display_name` (a custom add), then resolves it against the store:
  absent → use the base slug; present and the same object (matching
  `catalog_ref`, or coordinates within a small tolerance) → in-place
  edit, reusing the slug; present and a different object → allocate
  the lowest unused `"{base}-{n}"` suffix. The same-`catalog_ref`
  branch is itself gated on coordinate proximity, so a later catalog
  add can never silently clobber an earlier framed import of the same
  object (Decision 3, docs/plans/archive/planetarium-target-import.md).

  Scenario: Tool catalog includes the target CRUD tools
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "add_target"
    And the tool list should include "get_target"
    And the tool list should include "list_targets"
    And the tool list should include "update_target"
    And the tool list should include "delete_target"
    And the tool list should include "set_goals"

  Scenario: Adding a non-catalog target creates it active by default
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "add_target" with display_name "Comet Test" ra_hours 5.5 dec_degrees 20.0
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "comet-test"
    When the MCP client fetches the target it just added
    Then the tool call should succeed
    And the fetched target should have display_name "Comet Test"
    And the fetched target should be active

  Scenario: Adding a catalog target resolves and denormalizes catalog fields
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "add_target" with catalog_ref "M 31"
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "m-31"

  Scenario: Re-adding the same catalog object updates it in place
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added catalog target "M 31"
    When the MCP client calls "add_target" with catalog_ref "M 31" and notes "re-add"
    Then the tool call should succeed
    And the target result should be an in-place update
    And the target slug should be "m-31"
    When the MCP client calls "list_targets"
    Then the tool call should succeed
    And list_targets should report exactly 1 target

  Scenario: A different object whose base slug collides gets a suffixed slug
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added a target named "Frame A" at ra_hours 5.0 dec_degrees 10.0
    When the MCP client calls "add_target" with display_name "Frame A" ra_hours 5.5 dec_degrees 15.0
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "frame-a-2"

  # NGC 7000's catalog centroid is ~RA 20.9767h, Dec +44.33deg. The
  # existing row below is framed a full degree east of it — far beyond
  # any plausible dedup tolerance (arcmin/arcsec scale) — so the second
  # catalog add must not clobber it (Decision 3's cross-writer
  # protection: this rule applies to every `catalog_ref` writer, not
  # only the P3 planetarium bridge).
  Scenario: A catalog add whose coordinates differ from an existing framed target gets a new slug
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added a target with catalog_ref "NGC 7000" ra_hours 21.9767 dec_degrees 44.33
    When the MCP client calls "add_target" with catalog_ref "NGC 7000"
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "ngc-7000-2"
    When the MCP client calls "list_targets"
    Then the tool call should succeed
    And list_targets should report exactly 2 targets

  Scenario: update_target edits fields without changing the slug
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added a target named "Frame A" at ra_hours 5.0 dec_degrees 10.0
    When the MCP client calls "update_target" for slug "frame-a" setting display_name "Frame A (revised)"
    Then the tool call should succeed
    And the fetched target slug should still be "frame-a"

  Scenario: update_target activates a pending target
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added an inactive target named "Pending Frame" at ra_hours 5.0 dec_degrees 10.0
    When the MCP client calls "update_target" for slug "pending-frame" setting active true
    Then the tool call should succeed
    And the fetched target should be active

  Scenario: list_targets filters to active targets only
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added an inactive target named "Pending Frame" at ra_hours 5.0 dec_degrees 10.0
    And the MCP client has added a target named "Live Frame" at ra_hours 6.0 dec_degrees 12.0
    When the MCP client calls "list_targets" with active_only true
    Then the tool call should succeed
    And the target list should contain exactly "live-frame"

  Scenario: delete_target removes the plan row
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added a target named "Doomed Frame" at ra_hours 5.0 dec_degrees 10.0
    When the MCP client calls "delete_target" for slug "doomed-frame"
    Then the tool call should succeed
    And the target result deleted should be true
    When the MCP client calls "list_targets"
    Then the tool call should succeed
    And list_targets should report exactly 0 targets

  Scenario: delete_target reports false for an absent slug
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "delete_target" for slug "does-not-exist"
    Then the tool call should succeed
    And the target result deleted should be false

  # Framing angle (P2, rp.md § Target Store → Position angle): the
  # per-target sky position angle in degrees east of north, in
  # move_rotator's domain (0.0 inclusive to 360.0 exclusive, finite).
  # Absent means "inherit the imaging train's configured default" —
  # the effective-angle fallback itself is pinned in
  # target_position_angle.feature.
  Scenario: A framing angle set at add time is stored and returned
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "add_target" with display_name "Framed" ra_hours 5.0 dec_degrees 10.0 and position angle 254.5
    Then the tool call should succeed
    When the MCP client fetches the target it just added
    Then the tool call should succeed
    And the fetched target position angle should be 254.5

  Scenario: An out-of-range framing angle fails at add time
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "add_target" with display_name "Framed" ra_hours 5.0 dec_degrees 10.0 and position angle 360.0
    Then the tool call should fail
    And the tool error message should mention "position_angle_degrees"

  Scenario: update_target sets a framing angle on an existing target
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added a target named "Frame A" at ra_hours 5.0 dec_degrees 10.0
    When the MCP client calls "update_target" for slug "frame-a" setting position angle 12.5
    Then the tool call should succeed
    And the fetched target position angle should be 12.5

  # Explicit null is the one update_target field with clearing
  # semantics: blank (inherit the train default) and 0.0 (explicit
  # north-up) must stay distinguishable for the P4 inbox.
  Scenario: An explicit null clears the framing angle back to inherit
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "add_target" with display_name "Frame A" ra_hours 5.0 dec_degrees 10.0 and position angle 12.5
    Then the tool call should succeed
    When the MCP client calls "update_target" for slug "frame-a" clearing the position angle
    Then the tool call should succeed
    And the fetched target position angle should be null

  Scenario: An out-of-range framing angle fails at update time
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has added a target named "Frame A" at ra_hours 5.0 dec_degrees 10.0
    When the MCP client calls "update_target" for slug "frame-a" setting position angle -1.0
    Then the tool call should fail
    And the tool error message should mention "position_angle_degrees"
