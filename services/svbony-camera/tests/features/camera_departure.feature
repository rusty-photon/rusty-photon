@serial
Feature: A camera that leaves the bus
  A camera whose power or cable is cut while it is connected keeps its SDK
  handle, and the SVBony SDK never says it has gone (C6): every call on the
  handle goes on answering from what the SDK cached, and a triggered frame
  simply never comes. Only a failure asks whether the camera is still there.
  A call that fails, the read of a frame that never came among them, rescans
  the bus, and a camera the rescan no longer finds marks the session lost.
  Nothing else asks, so a departed camera reads connected until something
  fails on it, which for an idle camera is its next exposure. From then on
  the camera answers Connected as false, and every member that needs a
  session answers NOT_CONNECTED, the members served from cache included.
  That takes in the capabilities cached true at connect, so a departed
  cooled camera is not described as one that still has its cooler.

  Disconnecting a departed camera succeeds. Reconnecting it fails while it is
  gone and connects afresh once it is back, never taking the lost session
  back. Against the simulation backend the camera leaves the bus while a
  departure file the suite controls exists, as SDK 1.13.4 was measured
  behaving: its calls keep answering, its frames never come, and it drops
  out of the rescan. It returns when the file is removed.

  Background:
    Given the svbony-camera service running with a simulated camera that can leave the bus
    And camera device 0 is connected

  Scenario: A departed camera is found gone by its next exposure
    When camera device 0 leaves the bus
    Then camera device 0 reports Connected as true
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is accepted
    And camera device 0 eventually reports Connected as false
    And reading these members from camera device 0 is rejected with ASCOM NOT_CONNECTED:
      | member               |
      | CCDTemperature       |
      | CanSetCCDTemperature |
      | CanGetCoolerPower    |
      | Gain                 |
      | Offset               |
      | BinX                 |
      | CameraXSize          |
      | CameraState          |

  Scenario: A departed camera disconnects cleanly
    When camera device 0 leaves the bus
    And I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is accepted
    And camera device 0 eventually reports Connected as false
    When I disconnect camera device 0
    Then camera device 0 reports Connected as false

  Scenario: A departed camera reconnects only once it is back
    When camera device 0 leaves the bus
    And I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is accepted
    And camera device 0 eventually reports Connected as false
    When I try to connect camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And camera device 0 reports Connected as false
    When camera device 0 returns to the bus
    And I connect camera device 0
    Then camera device 0 reports Connected as true
    And camera device 0 reports CanSetCCDTemperature as true
    And camera device 0 reports a finite CCDTemperature
