Feature: Power switching
  A workflow switches a power box's outputs through rp, and takes every
  device on an output out of rp's hands before it cuts the output. rp's
  power map, the outputs list of a switches entry, names each output by
  the name its driver reports, lists the roster ids of the devices the
  output feeds, and marks an output protected with the reason it is.

  disconnect_device holds a device out of the reconnect supervisor and
  sends its driver Connected=false; the supervisor then leaves it alone.
  connect_device releases the hold-out and connects the device at once,
  and leaves a live session as it is rather than cycling it.

  get_switch reads an output beside the devices the power map puts on
  it. set_switch refuses an output the map does not list, refuses to
  switch off a protected output, and refuses to switch off an output
  while a device on it is not held out.

  Background:
    Given a stub power box with the outputs "Camera Power", "Dew Heater", "Host Power" and "Spare Output", all on
    And a stub Alpaca service hosting a camera
    And rp is configured with a camera on the stub service
    And rp's power map puts "main-cam" on the stub power box's "Camera Power" output
    And rp's power map lists the stub power box's "Dew Heater" output with no devices
    And rp's power map protects the stub power box's "Host Power" output because "the host runs off it"
    And an equipment reconnect interval of 300 milliseconds
    And a test webhook receiver subscribed to "equipment_changed"
    When rp starts

  Scenario: An output reads back with the devices the power map puts on it
    When the MCP client calls "get_switch" with the JSON {"switch_id": "power-box", "output": "Camera Power"}
    Then the tool result at "/outputs/0/name" should be the JSON "Camera Power"
    And the tool result at "/outputs/0/index" should be the JSON 0
    And the tool result at "/outputs/0/on" should be the JSON true
    And the tool result at "/outputs/0/mapped" should be the JSON true
    And the tool result at "/outputs/0/protected" should be the JSON null
    And the tool result at "/outputs/0/devices" should be the JSON [{"device_id": "main-cam", "kind": "camera", "connected": true, "held_out": false}]

  Scenario: Every output the box reports reads back, an unmapped one included
    When the MCP client calls "get_switch" with the JSON {"switch_id": "power-box"}
    Then the tool result at "/outputs/2/name" should be the JSON "Host Power"
    And the tool result at "/outputs/2/protected" should be the JSON "the host runs off it"
    And the tool result at "/outputs/3/name" should be the JSON "Spare Output"
    And the tool result at "/outputs/3/mapped" should be the JSON false
    And the tool result at "/unmatched" should be the JSON []

  Scenario: An output stays on while a device on it is still connected through rp
    When the MCP client calls "set_switch" with the JSON {"switch_id": "power-box", "output": "Camera Power", "on": false}
    Then the tool call should return an error
    And the error message should contain "main-cam"
    And the stub power box's "Camera Power" output should be on

  Scenario: An output switches off once every device on it is held out
    When the MCP client calls "disconnect_device" with the JSON {"device_id": "main-cam"}
    Then the tool result at "/close" should be the JSON "closed"
    And the tool result at "/kind" should be the JSON "camera"
    And the stub camera should read disconnected
    And an "equipment_changed" event should report the device "main-cam" as disconnected
    And the equipment status should show the camera "main-cam" held out
    When the MCP client calls "set_switch" with the JSON {"switch_id": "power-box", "output": "Camera Power", "on": false}
    Then the tool result at "/on" should be the JSON false
    And the stub power box's "Camera Power" output should be off

  Scenario: The reconnect supervisor leaves a held-out device disconnected
    When the MCP client calls "disconnect_device" with the JSON {"device_id": "main-cam"}
    And the reconnect supervisor has passed the stub power box 3 more times
    Then the stub camera should read disconnected
    And the equipment status should show the camera "main-cam" held out

  Scenario: connect_device gives a held-out device back to rp at once
    When the MCP client calls "disconnect_device" with the JSON {"device_id": "main-cam"}
    And the MCP client calls "connect_device" with the JSON {"device_id": "main-cam"}
    Then the tool result at "/connected" should be the JSON true
    And the stub camera should read connected
    And an "equipment_changed" event should report the device "main-cam" as connected
    And the equipment status should show the camera "main-cam" connected and not held out

  Scenario: connect_device leaves a live session as it is
    When the MCP client calls "connect_device" with the JSON {"device_id": "main-cam"}
    Then the tool result at "/connected" should be the JSON true
    And the stub camera should have been sent Connected=true once and Connected=false never

  Scenario: A protected output is never switched off
    When the MCP client calls "set_switch" with the JSON {"switch_id": "power-box", "output": "Host Power", "on": false}
    Then the tool call should return an error
    And the error message should contain "the host runs off it"
    And the stub power box's "Host Power" output should be on

  Scenario: An output the power map does not list is not switched
    When the MCP client calls "set_switch" with the JSON {"switch_id": "power-box", "output": "Spare Output", "on": false}
    Then the tool call should return an error
    And the error message should contain "power map"
    And the stub power box's "Spare Output" output should be on

  Scenario: An output that feeds no device switches off and back on
    When the MCP client calls "set_switch" with the JSON {"switch_id": "power-box", "output": "Dew Heater", "on": false}
    Then the tool result at "/on" should be the JSON false
    And the stub power box's "Dew Heater" output should be off
    When the MCP client calls "set_switch" with the JSON {"switch_id": "power-box", "output": "Dew Heater", "on": true}
    Then the tool result at "/on" should be the JSON true
    And the stub power box's "Dew Heater" output should be on

  Scenario: A device id rp does not know is refused
    When the MCP client calls "disconnect_device" with the JSON {"device_id": "no-such-device"}
    Then the tool call should return an error
    And the error message should contain "no-such-device"
