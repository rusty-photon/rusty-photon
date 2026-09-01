@serial
Feature: Equipment session recovery
  An Alpaca device session is server-side state: a device service that
  restarts forgets Connected=true while rp's client handle stays valid,
  and a device service that was down when rp started never had a
  session at all. rp's reconnect supervisor walks every configured
  device at equipment.reconnect_interval (production default 30s;
  these scenarios shorten it), health-checks each session through the
  Connected property, and re-establishes dead sessions with the full
  connect routine — roster re-enumeration, Connected=true, and a fresh
  read of the connect-time property cache. Every successful
  re-establishment emits an "equipment_changed" event with "connected"
  true; a lost session emits one with "connected" false, once per
  transition, and GET /api/equipment reflects the live session state.
  Recovery never requires an rp restart, and until a safety monitor's
  session recovers it reads as unsafe — fail-unsafe holds throughout.

  Scenario: A safety monitor session survives its device service restarting
    Given a stub Alpaca service hosting a safety monitor
    And rp is configured with a safety monitor on the stub service
    And an equipment reconnect interval of 500 milliseconds
    And a test webhook receiver subscribed to "safety_changed" and "equipment_changed"
    When rp starts
    Then the equipment status should show the stub safety monitor as connected
    When the stub Alpaca service stops
    Then the "safety_changed" event payload field "new_state" should be "unsafe"
    And an "equipment_changed" event should report the device "stub-monitor" as disconnected
    And the equipment status should show the stub safety monitor as disconnected within 15 seconds
    When the stub Alpaca service comes back with its session state lost
    Then an "equipment_changed" event should report the device "stub-monitor" as connected
    And the equipment status should show the stub safety monitor as connected within 15 seconds
    And the safety monitor should report safe again without an rp restart

  Scenario: A camera that was down at rp startup is picked up when its service appears
    Given a stub Alpaca service hosting a camera, currently stopped
    And rp is configured with a camera on the stub service
    And an equipment reconnect interval of 500 milliseconds
    And a test webhook receiver subscribed to "equipment_changed"
    When rp starts
    Then the equipment status should show the camera as disconnected
    When the stub Alpaca service comes back with its session state lost
    Then an "equipment_changed" event should report the device "main-cam" as connected
    And the equipment status should show the camera as connected within 15 seconds
