Feature: rp configuration page
  The BFF serves rp's own configuration at /config/rp — and, when an rp
  target is configured, at / itself: the Configuration nav surface IS rp's
  settings page (per-device configuration is reached from the equipment
  page's Configure buttons instead). Both routes speak rp's plain-REST
  config API (GET /api/config, GET /api/config/schema, PUT /api/config) —
  the same schema-driven form machinery as any driver, through a REST
  transport instead of ASCOM actions. rp has no in-process reload
  (ApplyDisposition::Restart), so a successful apply persists to rp's config
  file, reports the changed paths as restart-required, and the page renders
  the restart callout instead of the reconnect poll. rp's equipment arrays
  are arrays of objects and skipped by the schema walker (they round-trip
  through the hidden blob and are edited on the equipment page instead —
  where a camera entry's integer-enum cooler_targets_c array renders as a
  checkbox grid), and rp's optional blocks (site, guider, plate_solver,
  planner) blob-round-trip the same way under the standard composite-skip
  rule — the form edits rp's scalar leaves (session, safety, imaging,
  centering, cooling, server).

  Scenario: The rp config page renders rp's effective configuration
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the config page for "rp"
    Then the page shows an input named "session.file_naming_pattern" with value "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}"
    And the input named "server.port" is disabled

  Scenario: With an rp target the configuration surface is rp's settings page
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the configuration index
    Then the page shows an input named "session.file_naming_pattern" with value "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}"
    And the input named "server.port" is disabled

  Scenario: Applying a change persists to rp's config file and renders the restart callout
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the config page for "rp"
    And I submit the rp form with "session.file_naming_pattern" set to "{target}_{filter}_{binning}_{exposure_duration}_{frame_number}"
    Then the page reports the changes take effect when rp is restarted
    And the restart callout lists "session.file_naming_pattern"
    And rp's config file on disk contains the string "{target}_{filter}_{binning}_{exposure_duration}_{frame_number}"

  # Clearing an optional text box means "unset this", and the form says so
  # in the only spelling that means it: null. An empty string is a
  # different state — rp reads an absent file_naming_pattern as "no
  # templated naming" but an empty one as malformed — so sending "" would
  # turn a clear into a config rp refuses to load.
  Scenario: Clearing an optional field unsets it rather than sending an empty string
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the config page for "rp"
    And I submit the rp form with "session.file_naming_pattern" set to ""
    Then the page reports the changes take effect when rp is restarted
    And rp's config file on disk has "session.file_naming_pattern" set to null

  Scenario: A value rp cannot parse is rejected and rp's config file is untouched
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the config page for "rp"
    And I submit the rp form with "safety.poll_interval" set to "never"
    Then the page shows an error banner mentioning "invalid config JSON"
    And rp's config file on disk does not contain the string "never"

  Scenario: An unreachable rp renders an error banner with a retry
    Given a BFF pointed at an unreachable rp
    When I open the config page for "rp"
    Then the page shows an error banner mentioning "could not reach"
