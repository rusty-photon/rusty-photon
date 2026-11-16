@serial
Feature: Rotator MCP tools
  move_rotator moves a rotator to an absolute sky angle in degrees
  (0.0 inclusive to 360.0 exclusive — the ASCOM Position frame, which
  honors any sync offset the driver holds), blocks polling IsMoving
  until idle, and reads back the sky and mechanical angles;
  get_rotator_position reads them without moving. Both tools address
  the device as rotator_id or train_id — exactly one — where train_id
  resolves through the optical-train model and requires the train to
  contain exactly one rotator. The angle is validated before any
  motion. move_rotator reports moved_trains, every train containing
  the rotator. A move that turns the guiding train runs the
  rotate-while-guiding ladder around it (rotate_while_guiding.feature).
  The move emits a move_rotator_started / move_rotator_complete /
  move_rotator_failed operation triple carrying no predictive
  deadline.

  Scenario: Tool catalog includes the rotator tools
    Given a running Alpaca simulator
    And rp is running with a rotator on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "move_rotator"
    And the tool list should include "get_rotator_position"

  Scenario: get_rotator_position reads the angle and motion state
    Given a running Alpaca simulator
    And rp is running with a rotator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_rotator_position" with rotator "main-rotator"
    Then the rotator result field "rotator_id" should be "main-rotator"
    And the rotator result should carry a numeric "angle"
    And the rotator result should carry a numeric "mechanical_angle"
    And the rotator result field "is_moving" should be false

  Scenario: move_rotator moves to an absolute sky angle and reads back
    Given a running Alpaca simulator
    And rp is running with a rotator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 5.0
    Then the rotator result field "rotator_id" should be "main-rotator"
    And the rotator result field "angle" should be 5.0 within 0.1
    And the rotator result should list no moved trains

  Scenario: move_rotator via train addressing reports the moved trains
    Given a running Alpaca simulator
    And rp is running with a rotator on the simulator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with train "main" to angle 5.0
    Then the rotator result field "rotator_id" should be "main-rotator"
    And the rotator result should list moved train "main"

  Scenario: get_rotator_position accepts train addressing
    Given a running Alpaca simulator
    And rp is running with a rotator on the simulator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "get_rotator_position" with train "main"
    Then the rotator result field "rotator_id" should be "main-rotator"
    And the rotator result field "is_moving" should be false

  Scenario: move_rotator emits the operation triple
    Given a running Alpaca simulator
    And a test webhook receiver subscribed to "move_rotator_started" and "move_rotator_complete"
    And rp is running with a rotator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 5.0
    Then the test webhook receiver should receive a "move_rotator_started" event
    And the test webhook receiver should receive a "move_rotator_complete" event
    And the "move_rotator_started" event payload field "rotator_id" should be "main-rotator"
    And the "move_rotator_started" event payload should contain a "angle"
    And the "move_rotator_complete" event payload should contain a "moved_trains"

  Scenario Outline: move_rotator rejects an out-of-range angle before any motion
    Given rp is running with an offline rotator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle <angle>
    Then the tool call should return an error
    And the error message should contain "angle"

    Examples:
      | angle |
      | 360.0 |
      | -5.0  |

  Scenario: move_rotator rejects rotator_id combined with train_id
    Given rp is running with an offline rotator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" and train "main" to angle 5.0
    Then the tool call should return an error
    And the error message should contain "exactly one of rotator_id or train_id"

  Scenario: move_rotator rejects a call with no addressing
    Given rp is running with an offline rotator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with no addressing to angle 5.0
    Then the tool call should return an error
    And the error message should contain "exactly one of rotator_id or train_id"

  Scenario: move_rotator with an unknown rotator returns an error
    Given rp is running with an offline rotator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "nonexistent" to angle 5.0
    Then the tool call should return an error
    And the error message should contain "rotator not found"

  Scenario: move_rotator with a disconnected rotator returns an error
    Given rp is running with an offline rotator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with rotator "main-rotator" to angle 5.0
    Then the tool call should return an error
    And the error message should contain "rotator not connected"

  Scenario: move_rotator with an unknown train returns an error
    Given rp is running with an offline rotator inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with train "nonexistent" to angle 5.0
    Then the tool call should return an error
    And the error message should contain "train not found"

  Scenario: move_rotator on a train without a rotator returns an error
    Given rp is running with an offline camera-only train
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with train "main" to angle 5.0
    Then the tool call should return an error
    And the error message should contain "no rotator"

  Scenario: move_rotator on a train with two rotators asks for the explicit id
    Given rp is running with two offline rotators inside train "main"
    And an MCP client connected to rp
    When the MCP client calls "move_rotator" with train "main" to angle 5.0
    Then the tool call should return an error
    And the error message should contain "pass rotator_id"
