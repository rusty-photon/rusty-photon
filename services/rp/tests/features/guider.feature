@serial
Feature: Guiding tools without a guider to reach
  rp's guiding tools proxy to the guider service that the guiding
  config, equipment.mount.guiding, points at. Without that block every
  guiding tool errors with "guider not configured" and reaches no
  guider. A guider that cannot be reached at its configured url fails
  the call with "service unreachable". The tools' behaviour against a
  guider that answers is in guiding_tools.feature and
  guiding_dither.feature.

  Scenario Outline: Guider tools without a configured guider return an error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls the guider tool "<tool>" with empty arguments
    Then the tool call should return an error
    And the error message should contain "guider not configured"

    Examples:
      | tool              |
      | start_guiding     |
      | stop_guiding      |
      | pause_guiding     |
      | resume_guiding    |
      | get_guiding_stats |

  Scenario: Service unreachable error when the guider URL points at an unbound port
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator and a guider pointing at an unbound port
    And an MCP client connected to rp
    When the MCP client calls "start_guiding" with no arguments
    Then the tool call should return an error
    And the error message should contain "service unreachable"
