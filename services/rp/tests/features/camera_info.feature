@serial
Feature: Camera info tool
  The get_camera_info MCP tool reads camera capabilities from the connected
  ASCOM Alpaca device. It returns max_adu (full well depth in ADU),
  exposure time limits, sensor dimensions, binning, and the gain and
  offset the sensor currently runs at (read live from the device; null
  only when the driver does not implement the property — any other
  read failure is a tool error). Workflow plugins use this to
  compute target ADU levels for flat calibration and to pin the gain a
  flat-timing record was trained at.

  Scenario: Returns max_adu and sensor dimensions for connected camera
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_camera_info" with camera "main-cam"
    Then the tool result should contain "max_adu" as a positive integer
    And the tool result should contain "sensor_x" as a positive integer
    And the tool result should contain "sensor_y" as a positive integer

  Scenario: Returns exposure limits for connected camera
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_camera_info" with camera "main-cam"
    Then the tool result should contain "exposure_min"
    And the tool result should contain "exposure_max"

  Scenario: Tool catalog includes get_camera_info
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "get_camera_info"

  Scenario: get_camera_info with nonexistent camera returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_camera_info" with camera "nonexistent"
    Then the tool call should return an error
    And the error message should contain "camera not found"

  Scenario: get_camera_info with disconnected camera returns error
    Given rp is running with a camera at "http://localhost:1" device 0
    And an MCP client connected to rp
    When the MCP client calls "get_camera_info" with camera "main-cam"
    Then the tool call should return an error
    And the error message should contain "camera not connected"

  Scenario: get_camera_info with missing camera_id returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_camera_info" with no camera_id
    Then the tool call should return an error
    And the error message should contain "camera_id"

  Scenario: Reports gain and offset read from the device
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_camera_info" with camera "main-cam"
    Then the tool result should contain "gain" as an integer or null
    And the tool result should contain "offset" as an integer or null
