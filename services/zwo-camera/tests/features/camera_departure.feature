@serial
Feature: A camera that leaves the bus
  A camera whose power or cable is cut while it is connected keeps its SDK
  handle, and Connected is the driver's own record of that handle (C6). The
  ASI SDK never says the camera has gone. Until something rescans the bus it
  answers reads from memory and fails the calls that have to reach the
  camera, so every SDK failure asks whether the camera is still there, by
  rescanning. A camera the rescan no longer finds has left: from then on it
  answers Connected as false, and every member that needs a session answers
  NOT_CONNECTED, the members served from cache included. The call whose
  failure found it answers NOT_CONNECTED too.

  Only a failure asks. Until a call fails, a departed camera still reads
  connected and its reads still answer, and a capture in flight finds out
  when its frame is due. Disconnecting a departed camera succeeds.
  Reconnecting it fails while it is gone and connects afresh once it is
  back, never taking the lost session back. Against the simulation backend
  the camera leaves the bus while a departure file the suite controls exists,
  and returns when it is removed.

  Background:
    Given the zwo-camera service running with a simulated camera that can leave the bus
    And camera device 0 is connected

  Scenario: A departed camera still reads connected until a call fails
    When camera device 0 leaves the bus
    And I try to read CCDTemperature from camera device 0
    Then the read succeeds
    And camera device 0 reports Connected as true

  Scenario: The first failing call after the camera leaves reports the disconnect
    When camera device 0 leaves the bus
    And I try to set the target CCD temperature to -10 on camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And camera device 0 reports Connected as false

  Scenario: Once the departure is known the members served from cache refuse too
    When camera device 0 leaves the bus
    And I try to set the target CCD temperature to -10 on camera device 0
    Then reading these members from camera device 0 is rejected with ASCOM NOT_CONNECTED:
      | member               |
      | CCDTemperature       |
      | Gain                 |
      | Offset               |
      | BinX                 |
      | CameraXSize          |
      | CameraState          |
      | CanSetCCDTemperature |

  Scenario: A camera that leaves during an exposure reads disconnected once its frame is due
    Given a 1 s exposure is in flight on camera device 0
    When camera device 0 leaves the bus
    Then camera device 0 reports Connected as false once the frame is due
    And reading these members from camera device 0 is rejected with ASCOM NOT_CONNECTED:
      | member      |
      | CameraState |
      | ImageReady  |

  Scenario: A departed camera disconnects cleanly
    When camera device 0 leaves the bus
    And I try to set the target CCD temperature to -10 on camera device 0
    And I disconnect camera device 0
    Then camera device 0 reports Connected as false

  Scenario: A departed camera reconnects only once it is back
    When camera device 0 leaves the bus
    And I try to set the target CCD temperature to -10 on camera device 0
    And I try to connect camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And camera device 0 reports Connected as false
    When camera device 0 returns to the bus
    And I connect camera device 0
    Then camera device 0 reports Connected as true
    And camera device 0 reports CanSetCCDTemperature as true
