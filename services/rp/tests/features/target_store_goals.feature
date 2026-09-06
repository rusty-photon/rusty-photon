Feature: Target acquisition goals and filter-roster validation (P1)
  `set_goals` replaces a target's goal set atomically; `add_target`
  applies `targets.default_goals` from config when the caller supplies
  none (Decision 10, docs/plans/archive/planetarium-target-import.md — default
  goals are rp-owned policy, not bridge/UI config). Every goal's
  `filter` is validated against the connected rig's configured filter
  roster at add/set time, so a plan referencing a filter the rig lacks
  fails at add, not mid-session. rp.md § Target Store.

  Scenario: set_goals replaces the goal set atomically
    Given rp is running with a target store and filter roster "Luminance, Red, Green, Blue"
    And an MCP client connected to rp
    And the MCP client has added a target named "Galaxy Frame" at ra_hours 5.0 dec_degrees 10.0
    When the MCP client calls "set_goals" for slug "galaxy-frame" with goals:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
      | Red       | 1x1     | 5m       | 20            |
    Then the tool call should succeed
    And the fetched target should have exactly these goals:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
      | Red       | 1x1     | 5m       | 20            |

  Scenario: A second set_goals call replaces rather than merges
    Given rp is running with a target store and filter roster "Luminance, Red, Green, Blue"
    And an MCP client connected to rp
    And the MCP client has added a target named "Galaxy Frame" at ra_hours 5.0 dec_degrees 10.0
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
      | Red       | 1x1     | 5m       | 20            |
    When the MCP client calls "set_goals" for slug "galaxy-frame" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Blue   | 1x1     | 5m       | 15            |
    Then the tool call should succeed
    And the fetched target should have exactly these goals:
      | filter | binning | exposure_duration | desired_count |
      | Blue   | 1x1     | 5m       | 15            |

  Scenario: add_target with no goals applies the configured default goals
    Given rp is configured with default target goals:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 20            |
    And rp is running with a target store and filter roster "Luminance, Red, Green, Blue"
    And an MCP client connected to rp
    When the MCP client calls "add_target" with display_name "Default Goals Frame" ra_hours 5.0 dec_degrees 10.0
    Then the tool call should succeed
    When the MCP client fetches the target it just added
    Then the tool call should succeed
    And the fetched target should have exactly these goals:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 20            |

  Scenario: add_target with an explicit goals list overrides the configured default
    Given rp is configured with default target goals:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 20            |
    And rp is running with a target store and filter roster "Luminance, Red, Green, Blue"
    And an MCP client connected to rp
    When the MCP client calls "add_target" with display_name "Explicit Goals Frame" ra_hours 5.0 dec_degrees 10.0 and goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 5m       | 5              |
    Then the tool call should succeed
    When the MCP client fetches the target it just added
    Then the tool call should succeed
    And the fetched target should have exactly these goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 5m       | 5              |

  Scenario: add_target rejects a goal naming a filter outside the roster
    Given rp is running with a target store and filter roster "Luminance, Red, Green, Blue"
    And an MCP client connected to rp
    When the MCP client calls "add_target" with display_name "Bad Filter Frame" ra_hours 5.0 dec_degrees 10.0 and goals:
      | filter | binning | exposure_duration | desired_count |
      | Ha     | 1x1     | 5m       | 10             |
    Then the tool call should fail
    And the tool error message should mention "Ha"

  Scenario: set_goals rejects a goal naming a filter outside the roster
    Given rp is running with a target store and filter roster "Luminance, Red, Green, Blue"
    And an MCP client connected to rp
    And the MCP client has added a target named "Galaxy Frame" at ra_hours 5.0 dec_degrees 10.0
    When the MCP client calls "set_goals" for slug "galaxy-frame" with goals:
      | filter | binning | exposure_duration | desired_count |
      | OIII   | 1x1     | 5m       | 10             |
    Then the tool call should fail
    And the tool error message should mention "OIII"
