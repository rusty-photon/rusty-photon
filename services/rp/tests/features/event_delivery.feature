@serial
Feature: Event delivery to webhook subscribers
  rp emits events when equipment operations occur. Plugins subscribe to
  events via webhook URLs configured at startup. rp POSTs events to each
  subscriber and records their acknowledgment. Unsubscribed plugins do
  not receive events. Unreachable plugins do not block operations.

  Scenario: Exposure complete event is delivered to subscribed plugin
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "exposure_complete"
    And rp is running with a camera on the simulator and the test plugin
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    Then the test webhook receiver should receive an "exposure_complete" event
    And the event payload should contain the document id
    And the event payload should contain the file path

  Scenario: Exposure started event is delivered before exposure complete
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "exposure_started" and "exposure_complete"
    And rp is running with a camera on the simulator and the test plugin
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    Then the test webhook receiver should receive an "exposure_started" event
    And the test webhook receiver should receive an "exposure_complete" event
    And "exposure_started" should have been received before "exposure_complete"

  Scenario: Filter switch event is delivered when filter changes
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "filter_switch"
    And rp is running with a filter wheel on the simulator and the test plugin
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Red"
    Then the test webhook receiver should receive a "filter_switch" event

  Scenario: Plugin acknowledges event with timing estimates
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "exposure_complete"
    And the test webhook receiver acknowledges with estimated 5 seconds and max 10 seconds
    And rp is running with a camera on the simulator and the test plugin
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    Then the test webhook receiver should receive an "exposure_complete" event
    And rp should have recorded the plugin timing estimates

  Scenario: Unsubscribed plugin does not receive events
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "slew_started"
    And rp is running with a camera on the simulator and the test plugin
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    Then the test webhook receiver should not have received any events

  Scenario: Unreachable plugin does not block capture
    Given a running Alpaca simulator
    And a plugin configured with webhook URL "http://localhost:1/webhook" subscribed to "exposure_complete"
    And rp is running with a camera on the simulator and the unreachable plugin
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    Then the tool result should contain an image path

  Scenario: Multiple plugins receive the same event independently
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "exposure_complete"
    And a plugin configured with webhook URL "http://localhost:1/webhook" subscribed to "exposure_complete"
    And rp is running with a camera on the simulator and the test plugin
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    Then the test webhook receiver should receive an "exposure_complete" event
    And the tool result should contain an image path

  Scenario: Delivery reaches a subscriber that requires authentication
    Given a running Alpaca simulator
    And a test webhook receiver requiring the credential "observatory" with password "flat-secret" subscribed to "filter_switch"
    And the event plugin registration carries the credential "observatory" with password "flat-secret"
    And rp is running with a filter wheel on the simulator and the test plugin
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Red"
    Then the test webhook receiver should receive a "filter_switch" event

  Scenario: A challenging subscriber receives nothing when the registration omits its credential
    Given a running Alpaca simulator
    And a test webhook receiver requiring the credential "observatory" with password "flat-secret" subscribed to "filter_switch"
    And rp is running with a filter wheel on the simulator and the test plugin
    And an MCP client connected to rp
    When the MCP client calls "set_filter" with filter wheel "main-fw" and filter "Red"
    Then the test webhook receiver should not have received any events
