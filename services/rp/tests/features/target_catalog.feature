@serial
Feature: Target catalog resolution
  rp ships an embedded sky catalog (docs/crates/rp-catalog.md: Messier,
  NGC, IC and further deep-sky catalogs, plus the HD stars) and exposes
  it via the `resolve_target` MCP tool. Lookup is case- and
  whitespace-insensitive: `"M 41"`, `"M41"`, `"m 41"`, and
  `"Messier 41"` all resolve to the same target. Common-name aliases
  (`"Andromeda Galaxy"` → NGC 224) are honoured.

  On a miss, the tool returns a structured error payload of the
  form `{"error": "target_not_found", "name": <query>,
  "suggestions": [<top-3 fuzzy matches>]}` so a planner plugin can
  surface "did you mean…?" without parsing free-form text.

  A target-store add takes literal RA/Dec as well as a catalog name
  (target_store_crud.feature).

  Scenario: Tool catalog includes resolve_target
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "resolve_target"

  Scenario: M 31 resolves to known coordinates
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "resolve_target" with name "M 31"
    Then the tool call should succeed
    And the resolved target name should be "M 31"
    And the resolved target ra_hours should be approximately 0.7123
    And the resolved target dec_degrees should be approximately 41.2691

  Scenario Outline: Alternate spellings of M 41 resolve to the same target
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "resolve_target" with name "<query>"
    Then the tool call should succeed
    And the resolved target name should be "M 41"

    Examples:
      | query       |
      | M 41        |
      | M41         |
      | m 41        |
      | Messier 41  |
      | messier 41  |

  Scenario: Common-name alias resolves to canonical NGC entry
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "resolve_target" with name "Andromeda Galaxy"
    Then the tool call should succeed
    And the resolved target name should be "NGC 224"

  Scenario: Missing target returns structured not-found with suggestions
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "resolve_target" with name "M 999"
    Then the tool call should fail
    And the tool error payload should have field "error" equal to "target_not_found"
    And the tool error payload should carry a non-empty suggestions list
