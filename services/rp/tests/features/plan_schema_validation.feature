Feature: Plan schema and validation tools
  The plan-data analogue of what a service's setup pages do for its
  configuration (ADR-019, rp.md § Plan schema and validation): two read-only
  MCP tools that let any surface discover the shape of plan data and check
  a candidate against the same rules the write tools enforce, without
  writing anything.

  `get_plan_schema` returns the JSON Schema for one plan-data entity,
  generated from the same types the write tools deserialize.
  `validate_plan` checks a candidate value and returns
  `{valid, errors: [{path, msg}]}` — the FieldError shape a service's
  config validation returns, so a surface renders a plan error next to its
  field, as a setup page renders a config error. Paths are dotted
  and indexed into the submitted value ("ra_hours",
  "goals[1].binning", "scheduling.min_altitude_degrees").

  `entity` is one of "target" (the add_target payload), "goals" (a goals
  array), or "naming_pattern" ({file_naming_pattern, directory_pattern}).

  These tools exist only if they cannot disagree with the writers, so
  they re-implement nothing: validate_plan and add_target call the same
  functions in rp's plan_validation module. The scenarios below pin that
  agreement in both directions rather than leaving it to convention.

  Both tools are MCP-only and read-only: no REST counterpart, no store
  write, no actuation.

  Scenario: get_plan_schema returns a JSON Schema for the target entity
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "get_plan_schema" for entity "target"
    Then the tool call should succeed
    And the returned schema should describe properties "display_name, ra_hours, dec_degrees, goals"

  Scenario: get_plan_schema covers every validatable entity
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "get_plan_schema" for entity "goals"
    Then the tool call should succeed
    When the MCP client calls "get_plan_schema" for entity "naming_pattern"
    Then the tool call should succeed

  Scenario: An unknown entity is rejected rather than silently answered
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client calls "get_plan_schema" for entity "telescope"
    Then the tool call should return an error
    And the error message should contain "telescope"

  Scenario: A well-formed target payload validates clean
    Given rp is running with a target store and filter roster "Red, Blue"
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {
        "display_name": "Validation Field",
        "ra_hours": 5.0,
        "dec_degrees": 10.0,
        "goals": [
          {"filter": "Red", "binning": "1x1", "exposure_duration": "2m", "desired_count": 10}
        ]
      }
      """
    Then the tool call should succeed
    And the validation should report valid
    And the validation should report no errors

  Scenario: An out-of-range coordinate is reported at its own field path
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {"display_name": "Too Far", "ra_hours": 25.0, "dec_degrees": 10.0}
      """
    Then the tool call should succeed
    And the validation should report invalid
    And the validation errors should include path "ra_hours"

  Scenario: A malformed goal is reported at its indexed path
    Given rp is running with a target store and filter roster "Red, Blue"
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {
        "display_name": "Bad Goal",
        "ra_hours": 5.0,
        "dec_degrees": 10.0,
        "goals": [
          {"filter": "Red", "binning": "1x1", "exposure_duration": "2m", "desired_count": 10},
          {"filter": "Blue", "binning": "not-a-binning", "exposure_duration": "2m", "desired_count": 5}
        ]
      }
      """
    Then the tool call should succeed
    And the validation should report invalid
    And the validation errors should include path "goals[1].binning"

  Scenario: A filter absent from the rig's roster is reported against the goal
    Given rp is running with a target store and filter roster "Red, Blue"
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {
        "display_name": "Wrong Filter",
        "ra_hours": 5.0,
        "dec_degrees": 10.0,
        "goals": [
          {"filter": "Sulphur", "binning": "1x1", "exposure_duration": "2m", "desired_count": 10}
        ]
      }
      """
    Then the tool call should succeed
    And the validation should report invalid
    And the validation errors should include path "goals[0].filter"

  # A write stops at its first offending field; validate exists to fill in
  # a whole form at once, so it reports every violation in one pass.
  Scenario: Validation reports every violation, not just the first
    Given rp is running with a target store and filter roster "Red, Blue"
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {
        "display_name": "Many Problems",
        "ra_hours": 25.0,
        "dec_degrees": 100.0,
        "position_angle_degrees": 400.0
      }
      """
    Then the tool call should succeed
    And the validation should report invalid
    And the validation should report at least 2 errors

  Scenario: A goals array validates on its own
    Given rp is running with a target store and filter roster "Red, Blue"
    And an MCP client connected to rp
    When the MCP client validates entity "goals" with value:
      """
      [{"filter": "Red", "binning": "1x1", "exposure_duration": "2m", "desired_count": 0}]
      """
    Then the tool call should succeed
    And the validation should report invalid
    And the validation errors should include path "goals[0].desired_count"

  Scenario: A naming pattern validates against the same token rules config load applies
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client validates entity "naming_pattern" with value:
      """
      {"file_naming_pattern": "{target}_{bogus_token}"}
      """
    Then the tool call should succeed
    And the validation should report invalid
    And the validation errors should include path "file_naming_pattern"

  # An empty pattern and an absent one are different states: absent means
  # "use the default" (directory_pattern) or "no templated naming"
  # (file_naming_pattern), while "" is a pattern with nothing in it. Both
  # fields say so the same way, and both name null — the spelling that
  # actually unsets them — so a cleared form field cannot quietly mean
  # something the operator never chose.
  Scenario: An empty pattern is rejected by both fields alike, naming the way to unset it
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client validates entity "naming_pattern" with value:
      """
      {"file_naming_pattern": "", "directory_pattern": ""}
      """
    Then the tool call should succeed
    And the validation should report invalid
    And the validation error at path "file_naming_pattern" should mention "null"
    And the validation error at path "directory_pattern" should mention "null"

  # The property the tools exist for: a surface that pre-flights with
  # validate_plan must not then be surprised by add_target, in either
  # direction. Both call the same rules.
  Scenario: A payload validate_plan accepts is accepted by add_target
    Given rp is running with a target store and filter roster "Red, Blue"
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {
        "display_name": "Agreed Good",
        "ra_hours": 5.0,
        "dec_degrees": 10.0,
        "goals": [
          {"filter": "Red", "binning": "1x1", "exposure_duration": "2m", "desired_count": 10}
        ]
      }
      """
    Then the validation should report valid
    When the MCP client adds the target it just validated
    Then the tool call should succeed

  Scenario: A payload validate_plan rejects is rejected by add_target
    Given rp is running with a target store and filter roster "Red, Blue"
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {
        "display_name": "Agreed Bad",
        "ra_hours": 5.0,
        "dec_degrees": 10.0,
        "goals": [
          {"filter": "Sulphur", "binning": "1x1", "exposure_duration": "2m", "desired_count": 10}
        ]
      }
      """
    Then the validation should report invalid
    When the MCP client adds the target it just validated
    Then the tool call should return an error

  # Grading thresholds were the direction this agreement used to fail:
  # config load rejected a negative default_grading while add_target
  # accepted the same value as a per-target override. Both now go
  # through the one validator.
  Scenario: A negative grading threshold is rejected by both tools alike
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {
        "display_name": "Bad Grading",
        "ra_hours": 5.0,
        "dec_degrees": 10.0,
        "grading": {"max_hfr_pixels": -3.0}
      }
      """
    Then the validation should report invalid
    And the validation errors should include path "grading.max_hfr_pixels"
    When the MCP client adds the target it just validated
    Then the tool call should return an error

  Scenario: Validating writes nothing to the store
    Given rp is running with a target store and filter roster "Red, Blue"
    And an MCP client connected to rp
    When the MCP client validates entity "target" with value:
      """
      {
        "display_name": "Never Stored",
        "ra_hours": 5.0,
        "dec_degrees": 10.0,
        "goals": [
          {"filter": "Red", "binning": "1x1", "exposure_duration": "2m", "desired_count": 10}
        ]
      }
      """
    Then the validation should report valid
    When the MCP client calls "list_targets"
    Then the tool call should succeed
    And list_targets should report exactly 0 targets
