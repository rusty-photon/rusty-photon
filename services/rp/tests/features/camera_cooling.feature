@serial
Feature: Camera cooling selects and holds a dark-library setpoint
  A camera's cooler_targets_c lists the temperatures the operator keeps dark
  libraries for: unique integers on the 5 °C grid from -40 to +15. Cooling
  is driven by two ungated MCP tools (rp.md § Camera Cooling): the workflow
  decides when to cool and when to warm, rp decides which rung.

  start_cooldown answers at once with the cameras it is driving and runs
  one cooldown pass per ladder camera in the background: rp commands the
  lowest listed rung and polls the cooler. Stabilizing within 1.0 °C of
  the rung with cooler power at or below 90 % adopts the rung and emits
  cooler_stabilized. A trajectory that flattens above the rung, or one
  that holds the rung only at pegged power, marks tonight's floor: rp
  snaps up to the lowest rung at least 3 °C above the floor. When no rung
  qualifies, the cooler is switched off, cooler_unreachable is emitted,
  and the tool has still succeeded — the night proceeds uncooled. The
  tool is idempotent: a second call leaves a running pass alone and adopts
  a cooler already regulating at a rung without re-selecting or
  re-announcing.

  start_warmup answers at once with the cameras it is warming and ramps
  each setpoint up in +5 °C steps before switching the cooler off; a
  camera rp never commanded is left alone. rp never touches a cooler on
  its own — not at startup, not on a safety transition.

  Every capture stamps cooler_setpoint_c and sensor_temperature_c on its
  exposure document. Cameras with an empty ladder are never touched.

  The simulator profile shipped by bdd-infra models ambient +10 °C with a
  maximum cooler delta of 40 °C: rungs above -30 stabilize with power
  headroom, while -30 itself only holds at 100 % power — tonight's floor.

  Background:
    Given a running Alpaca simulator
    And cooling is tuned for test speed

  Scenario: The lowest reachable rung is adopted and announced
    Given a test webhook receiver subscribed to "cooler_stabilized"
    And rp is running with a camera with cooler targets "-10, 5" on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "start_cooldown"
    Then the tool call should succeed
    And the tool result cameras should be exactly "main-cam"
    And the test webhook receiver should receive a "cooler_stabilized" event
    And the "cooler_stabilized" event payload field "target_c" should be the number -10
    And the camera cooler should be on

  Scenario: A rung holding only at pegged power snaps up to the next rung
    Given a test webhook receiver subscribed to "cooler_stabilized"
    And rp is running with a camera with cooler targets "-30, -10" on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "start_cooldown"
    Then the test webhook receiver should receive a "cooler_stabilized" event
    And the "cooler_stabilized" event payload field "target_c" should be the number -10
    And the "cooler_stabilized" event payload should contain a "floor_c"

  Scenario: No reachable rung switches the cooler off and the tool still succeeds
    Given a test webhook receiver subscribed to "cooler_unreachable"
    And rp is running with a camera with cooler targets "-30" on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "start_cooldown"
    Then the tool call should succeed
    And the tool result cameras should be exactly "main-cam"
    And the test webhook receiver should receive a "cooler_unreachable" event
    And the "cooler_unreachable" event payload field "warmest_target_c" should be the number -30
    And the camera cooler should be off

  Scenario: Captured frames stamp the chosen setpoint and the sensor temperature
    Given a test webhook receiver subscribed to "cooler_stabilized"
    And rp is running with a camera with cooler targets "-10" on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "start_cooldown"
    Then the test webhook receiver should receive a "cooler_stabilized" event
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the document field "cooler_setpoint_c" should be the number -10
    And the document should carry a numeric "sensor_temperature_c"

  Scenario: A second start_cooldown adopts the held rung without re-selecting
    Given a test webhook receiver subscribed to "cooler_stabilized"
    And rp is running with a camera with cooler targets "-10, 5" on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "start_cooldown"
    And the test webhook receiver has received a "cooler_stabilized" event
    And the MCP client calls tool "start_cooldown"
    Then the tool call should succeed
    And the tool result cameras should be exactly "main-cam"
    # A fresh pass would re-announce within about 1.5 s under the fast
    # profile; adoption announces nothing.
    When the MCP client stays idle for 3 seconds
    Then the test webhook receiver should have received 1 "cooler_stabilized" events
    And the camera cooler should be on

  Scenario: start_warmup ramps the cooler warm and switches it off
    Given a test webhook receiver subscribed to "cooler_warmup_started" and "cooler_warmup_complete"
    And rp is running with a camera with cooler targets "5" on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "start_cooldown"
    And the camera cooler becomes on
    And the MCP client calls tool "start_warmup"
    Then the tool call should succeed
    And the tool result cameras should be exactly "main-cam"
    And the test webhook receiver should receive a "cooler_warmup_started" event
    And the test webhook receiver should receive a "cooler_warmup_complete" event
    And the camera cooler should be off

  Scenario: start_warmup leaves a camera rp never commanded alone
    Given a test webhook receiver subscribed to "cooler_warmup_started" and "cooler_warmup_complete"
    And rp is running with a camera with cooler targets "-10" on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "start_warmup"
    Then the tool call should succeed
    And the tool result cameras should be empty
    And the camera cooler should be off
    And the test webhook receiver should not have received any events

  Scenario: A camera with an empty ladder is never cooled
    Given a test webhook receiver subscribed to "cooler_stabilized" and "cooler_unreachable"
    And rp is running with a camera with no cooler targets on the simulator
    And an MCP client connected to rp
    When the MCP client calls tool "start_cooldown"
    Then the tool call should succeed
    And the tool result cameras should be empty
    And the camera cooler should be off
    And the test webhook receiver should not have received any events
