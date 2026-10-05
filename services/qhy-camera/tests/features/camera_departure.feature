@serial
Feature: A camera that leaves the bus
  A camera whose power or cable is cut while it is connected keeps its SDK
  handle, and nothing in the handle says it has gone (C9). qhy-camera finds
  out at the next SDK call that fails, or at the next capability probe, whose
  "absent" is an answer rather than a failure: it asks the SDK about the one
  control every connect requires, and a camera that has left no longer has
  it. From then on the camera and the filter wheel on it answer Connected as
  false, and every member that needs a session answers NOT_CONNECTED, the
  members served from cache included. A departed camera is never described
  as present, nor as a camera without a cooler.

  The check is lazy: until a call reaches the SDK, the camera still reads
  connected. Disconnecting a departed camera succeeds. Reconnecting it fails
  while it is gone and connects afresh once it is back, never taking the lost
  session back. Against the simulation backend the camera leaves the bus
  while a departure file the suite controls exists, and returns when it is
  removed.

  Background:
    Given the qhy-camera service running with a simulated camera that can leave the bus
    And camera device 0 is connected

  Scenario: A departed camera still reads connected until a call reaches the SDK
    When camera device 0 leaves the bus
    Then camera device 0 reports Connected as true

  Scenario: The first SDK failure after the camera leaves reports the disconnect
    When camera device 0 leaves the bus
    And I try to read CCDTemperature from camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And camera device 0 reports Connected as false

  Scenario: A departed camera is not described as a camera without a cooler
    When camera device 0 leaves the bus
    Then reading these members from camera device 0 is rejected with ASCOM NOT_CONNECTED:
      | member               |
      | CanSetCCDTemperature |
    And camera device 0 reports Connected as false

  Scenario: Once the departure is known the members served from cache refuse too
    When camera device 0 leaves the bus
    And I try to read CCDTemperature from camera device 0
    Then reading these members from camera device 0 is rejected with ASCOM NOT_CONNECTED:
      | member      |
      | Gain        |
      | Offset      |
      | BinX        |
      | CameraXSize |
      | CameraState |

  Scenario: The filter wheel on a departed camera reads disconnected too
    Given filterwheel device 0 is connected
    When camera device 0 leaves the bus
    And I try to read CCDTemperature from camera device 0
    Then filterwheel device 0 reports Connected as false

  Scenario: A departed camera disconnects cleanly
    When camera device 0 leaves the bus
    And I try to read CCDTemperature from camera device 0
    And I disconnect camera device 0
    Then camera device 0 reports Connected as false

  Scenario: A departed camera reconnects only once it is back
    When camera device 0 leaves the bus
    And I try to read CCDTemperature from camera device 0
    And I try to connect camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And camera device 0 reports Connected as false
    When camera device 0 returns to the bus
    And I connect camera device 0
    Then camera device 0 reports Connected as true
    And camera device 0 reports CanSetCCDTemperature as true
