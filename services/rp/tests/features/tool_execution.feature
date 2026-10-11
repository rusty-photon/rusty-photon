@serial
Feature: MCP tool execution against equipment
  rp exposes equipment operations as MCP tools. Workflow plugins call
  tools via the MCP server endpoint. Each tool validates parameters,
  calls the Alpaca device, and returns a structured result.

  Scenario: Capture returns image path and document id
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    Then the tool result should contain an image path
    And the tool result should contain a document id

  Scenario: Set filter changes the active filter
    Given a running Alpaca simulator
    And rp is running with a filter wheel on the simulator
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Red"
    And the MCP client calls "get_filter" with filter wheel "main-fw"
    Then the current filter should be "Red"

  # A wheel written while it travels does not take the new slot: the
  # simulator, like qhy-camera, refuses it as "already moving". So
  # set_filter waits for the move another client started to end, then
  # writes. The simulator turns one slot a second, so the move from
  # Luminance to slot 3 is still under way when set_filter arrives.
  Scenario: Set filter waits out a move already under way and lands on its filter
    Given a running Alpaca simulator
    And rp is running with a filter wheel on the simulator
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Luminance"
    And another client starts the filter wheel on the simulator moving to slot 3
    And the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Green"
    Then the tool call should succeed
    When the MCP client calls "get_filter" with filter wheel "main-fw"
    Then the current filter should be "Green"

  Scenario: Capture with invalid camera id returns an error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "nonexistent" for 1000 ms
    Then the tool call should return an error

  Scenario: Tool catalog includes capture and filter tools
    Given a running Alpaca simulator
    And rp is running with a camera and filter wheel on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "capture"
    And the tool list should include "set_filter"
    And the tool list should include "get_filter"

  Scenario: Capture with missing camera_id returns an error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with no camera_id
    Then the tool call should return an error
    And the error message should contain "camera_id"

  Scenario: Capture with missing duration returns an error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" but no duration
    Then the tool call should return an error
    And the error message should contain "duration"

  Scenario: Set filter with nonexistent filter wheel returns an error
    Given a running Alpaca simulator
    And rp is running with a filter wheel on the simulator
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with filter wheel "nonexistent" and filter "Red"
    Then the tool call should return an error
    And the error message should contain "filter wheel not found"

  Scenario: Set filter with nonexistent filter name returns an error
    Given a running Alpaca simulator
    And rp is running with a filter wheel on the simulator
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Ultraviolet"
    Then the tool call should return an error
    And the error message should contain "filter not found"

  Scenario: Get filter with nonexistent filter wheel returns an error
    Given a running Alpaca simulator
    And rp is running with a filter wheel on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_filter" with filter wheel "nonexistent"
    Then the tool call should return an error
    And the error message should contain "filter wheel not found"

  Scenario: Capture with disconnected camera returns an error
    Given rp is running with a camera at "http://localhost:1" device 0
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    Then the tool call should return an error
    And the error message should contain "camera not connected"

  Scenario: Set filter with disconnected filter wheel returns an error
    Given rp is running with a filter wheel at "http://localhost:1" device 0
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Red"
    Then the tool call should return an error
    And the error message should contain "filter wheel not connected"

  Scenario: Get filter with disconnected filter wheel returns an error
    Given rp is running with a filter wheel at "http://localhost:1" device 0
    And an MCP client connected to rp
    When the MCP client calls "get_filter" with filter wheel "main-fw"
    Then the tool call should return an error
    And the error message should contain "filter wheel not connected"

  Scenario: Unknown MCP method returns an error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls an unknown method "tools/bogus"
    Then the tool call should return an error
    And the error message should contain "__unknown_method__"

  Scenario: Unknown tool name returns an error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "nonexistent_tool"
    Then the tool call should return an error
    And the error message should contain "nonexistent_tool"
