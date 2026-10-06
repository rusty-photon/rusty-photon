@serial
Feature: A camera that leaves the bus
  A camera whose power or cable is cut while it is connected keeps its SDK
  handle, and nothing in that handle changes when it goes (C6). The SVBony
  SDK answers a call on a camera that has left with a status of its own,
  SVB_ERROR_CAMERA_REMOVED, and svbony-camera keeps that status: the first
  call it answers marks the session lost. From then on the camera answers
  Connected as false, and every member that needs a session answers
  NOT_CONNECTED, the members served from cache included. That takes in the
  capabilities cached true at connect, so a departed cooled camera is not
  described as one that still has its cooler. The call that found the camera
  gone answers NOT_CONNECTED too.

  The check is lazy: until a call reaches the SDK, the camera still reads
  connected, and a StartExposure is accepted. An exposure finds out by itself,
  at its own next SDK call. Disconnecting a departed camera succeeds.
  Reconnecting it fails while it is gone and connects afresh once it is back,
  never taking the lost session back. Against the simulation backend the
  camera leaves the bus while a departure file the suite controls exists,
  answering every call on it with CAMERA_REMOVED, and returns when the file
  is removed.

  Background:
    Given the svbony-camera service running with a simulated camera that can leave the bus
    And camera device 0 is connected

  Scenario: A departed camera still reads connected until a call reaches the SDK
    When camera device 0 leaves the bus
    Then camera device 0 reports Connected as true

  Scenario: The first call the SDK answers with CAMERA_REMOVED reports the disconnect
    When camera device 0 leaves the bus
    And I try to read CCDTemperature from camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And camera device 0 reports Connected as false

  Scenario: Once the departure is known the members served from cache refuse too
    When camera device 0 leaves the bus
    And I try to read CCDTemperature from camera device 0
    Then reading these members from camera device 0 is rejected with ASCOM NOT_CONNECTED:
      | member               |
      | CanSetCCDTemperature |
      | CanGetCoolerPower    |
      | Gain                 |
      | Offset               |
      | BinX                 |
      | CameraXSize          |
      | CameraState          |

  Scenario: An exposure on a departed camera finds the departure by itself
    When camera device 0 leaves the bus
    And I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is accepted
    And camera device 0 eventually reports Connected as false
    When I try to read CameraState from camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED

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
    And camera device 0 reports a finite CCDTemperature
