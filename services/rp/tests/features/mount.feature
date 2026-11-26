@serial
Feature: Mount tools
  rp exposes Telescope (mount) device operations as MCP tools. The
  mount is singular per rp deployment — piggyback rigs share one
  mount across multiple optical trains, so the slew / sync_mount /
  get_mount_position / get_tracking / set_tracking tools take no
  mount_id parameter.

  slew drives the mount to absolute equatorial coordinates and blocks
  until Slewing == false plus the configured / per-call settle. ASCOM
  requires Tracking == true before equatorial slews — slew propagates
  the natural Alpaca error if tracking is off rather than auto-enabling.
  Callers manage tracking explicitly via set_tracking / get_tracking.

  sync_mount sets the mount's reported position immediately, no polling.
  get_mount_position returns the mount's current RA (hours) / Dec
  (degrees). get_tracking returns both the current tracking state and
  the CanSetTracking capability; it fails loud if the Tracking read
  errors (no half-success).

  Scenario: Tool catalog includes Mount tools
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "slew"
    And the tool list should include "sync_mount"
    And the tool list should include "get_mount_position"
    And the tool list should include "get_tracking"
    And the tool list should include "set_tracking"
    And the tool list should include "park"
    And the tool list should include "unpark"
    And the tool list should include "get_park_state"
    And the tool list should include "abort_slew"

  Scenario: slew drives the mount to absolute equatorial coordinates
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the mount is unparked
    And the mount tracking is set to true
    When the MCP client calls "slew" with ra "10.6847" dec "41.2689"
    Then the tool call should succeed
    And the slew result actual_ra should be 10.6847
    And the slew result actual_dec should be 41.2689

  Scenario: slew with tracking off returns an error
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the mount is unparked
    And the mount tracking is set to false
    When the MCP client calls "slew" with ra "10.6847" dec "41.2689"
    Then the tool call should return an error
    And the error message should contain "failed to slew"

  Scenario: slew with no mount configured returns an error
    Given a running Alpaca simulator
    And rp is running without a mount
    And an MCP client connected to rp
    When the MCP client calls "slew" with ra "0.0" dec "0.0"
    Then the tool call should return an error
    And the error message should contain "no mount configured"

  Scenario: slew with mount not connected returns an error
    Given rp is running with a mount at "http://localhost:1" device 0
    And an MCP client connected to rp
    When the MCP client calls "slew" with ra "0.0" dec "0.0"
    Then the tool call should return an error
    And the error message should contain "mount not connected"

  Scenario Outline: slew rejects out-of-range and missing parameters
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the mount tracking is set to true
    When the MCP client calls "slew" with ra "<ra>" dec "<dec>"
    Then the tool call should return an error
    And the error message should contain "<fragment>"

    Examples:
      | ra      | dec     | fragment                       |
      | -1.0    | 0.0     | ra -1 is outside [0, 24)       |
      | 25.0    | 0.0     | ra 25 is outside [0, 24)       |
      | 0.0     | -91.0   | dec -91 is outside [-90, 90]   |
      | 0.0     | 91.0    | dec 91 is outside [-90, 90]    |
      | MISSING | 0.0     | missing required parameter: ra |
      | 0.0     | MISSING | missing required parameter: dec|

  # OmniSim rejects SyncToCoordinates with `InvalidOperationException`
  # when tracking is off, so this scenario explicitly enables tracking
  # before the sync.
  Scenario: sync_mount sets the mount's reported position
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the mount tracking is set to true
    When the MCP client calls "sync_mount" with ra "5.0" dec "10.0"
    Then the tool call should succeed

  Scenario: sync_mount with no mount configured returns an error
    Given a running Alpaca simulator
    And rp is running without a mount
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "0.0" dec "0.0"
    Then the tool call should return an error
    And the error message should contain "no mount configured"

  Scenario Outline: sync_mount rejects out-of-range parameters
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "<ra>" dec "<dec>"
    Then the tool call should return an error
    And the error message should contain "<fragment>"

    Examples:
      | ra   | dec   | fragment                    |
      | -1.0 | 0.0   | ra -1 is outside [0, 24)    |
      | 0.0  | 91.0  | dec 91 is outside [-90, 90] |

  Scenario: get_mount_position returns the mount's current RA and Dec
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_mount_position"
    Then the tool call should succeed

  Scenario: get_mount_position with no mount configured returns an error
    Given a running Alpaca simulator
    And rp is running without a mount
    And an MCP client connected to rp
    When the MCP client calls "get_mount_position"
    Then the tool call should return an error
    And the error message should contain "no mount configured"

  Scenario: get_tracking returns tracking state and can_set_tracking
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_tracking"
    Then the tool call should succeed
    And the get_tracking result can_set_tracking should be true

  Scenario: set_tracking enables tracking on the mount
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "set_tracking" with enabled true
    And the MCP client calls "get_tracking"
    Then the tool call should succeed
    And the get_tracking result tracking should be true

  Scenario: set_tracking disables tracking on the mount
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "set_tracking" with enabled false
    And the MCP client calls "get_tracking"
    Then the tool call should succeed
    And the get_tracking result tracking should be false

  Scenario: park stows the mount and reports at_park == true
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "park"
    Then the tool call should succeed
    When the MCP client calls "get_park_state"
    Then the tool call should succeed
    And the get_park_state result at_park should be true

  Scenario: park with no mount configured returns an error
    Given a running Alpaca simulator
    And rp is running without a mount
    And an MCP client connected to rp
    When the MCP client calls "park"
    Then the tool call should return an error
    And the error message should contain "no mount configured"

  Scenario: unpark clears the mount's parked flag
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "park"
    Then the tool call should succeed
    When the MCP client calls "get_park_state"
    Then the tool call should succeed
    And the get_park_state result at_park should be true
    When the MCP client calls "unpark"
    Then the tool call should succeed
    When the MCP client calls "get_park_state"
    Then the tool call should succeed
    And the get_park_state result at_park should be false

  Scenario: get_park_state returns at_park, can_park, and can_unpark
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_park_state"
    Then the tool call should succeed
    And the get_park_state result at_park should be false
    And the get_park_state result can_park should be true
    And the get_park_state result can_unpark should be true

  Scenario: abort_slew with no mount configured returns an error
    Given a running Alpaca simulator
    And rp is running without a mount
    And an MCP client connected to rp
    When the MCP client calls "abort_slew"
    Then the tool call should return an error
    And the error message should contain "no mount configured"
