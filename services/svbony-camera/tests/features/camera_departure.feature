@serial
Feature: A camera that leaves the bus
  A camera whose power or cable is cut while it is connected keeps its SDK
  handle, and the SVBony SDK never says it has gone (C6): every call on the
  handle goes on answering from what the SDK cached, and a triggered frame
  simply never comes. What does change is the bus, so svbony-camera checks
  the camera's presence by rescanning it, every second while it is connected,
  and a camera the rescan no longer finds marks the session lost. From then
  on the camera answers Connected as false, with no client call needed, and
  every member that needs a session answers NOT_CONNECTED, the members
  served from cache included. That takes in the capabilities cached true at
  connect, so a departed cooled camera is not described as one that still
  has its cooler. An exposure in flight does not hold the session open;
  its capture also stops rather than waiting out its read deadline, which
  the unit tests pin, since a client cannot see a disconnected camera's
  capture.

  Disconnecting a departed camera succeeds. Reconnecting it fails while it is
  gone and connects afresh once it is back, never taking the lost session
  back. Against the simulation backend the camera leaves the bus while a
  departure file the suite controls exists, as SDK 1.13.4 was measured
  behaving: its calls keep answering, its frames never come, and it drops
  out of the rescan. It returns when the file is removed.

  Background:
    Given the svbony-camera service running with a simulated camera that can leave the bus
    And camera device 0 is connected

  Scenario: A departed camera reads disconnected with no client call
    When camera device 0 leaves the bus
    Then camera device 0 eventually reports Connected as false

  Scenario: Once the departure is known the members served from cache refuse too
    When camera device 0 leaves the bus
    Then camera device 0 eventually reports Connected as false
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

  Scenario: An exposure in flight does not keep a departed camera connected
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 30 Light true
    Then the exposure is accepted
    When camera device 0 leaves the bus
    Then camera device 0 eventually reports Connected as false
    When I try to read CameraState from camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED

  Scenario: A departed camera disconnects cleanly
    When camera device 0 leaves the bus
    Then camera device 0 eventually reports Connected as false
    When I disconnect camera device 0
    Then camera device 0 reports Connected as false

  Scenario: A departed camera reconnects only once it is back
    When camera device 0 leaves the bus
    Then camera device 0 eventually reports Connected as false
    When I try to connect camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And camera device 0 reports Connected as false
    When camera device 0 returns to the bus
    And I connect camera device 0
    Then camera device 0 reports Connected as true
    And camera device 0 reports CanSetCCDTemperature as true
    And camera device 0 reports a finite CCDTemperature
