@serial
Feature: Camera enumeration and connection lifecycle
  svbony-camera enumerates connected SVBony cameras at startup and registers
  each as an ASCOM device, index 0, 1, 2, ..., on one port (C0). Unlike ZWO,
  each device's serial (`SVB_CAMERA_INFO.CameraSN`) arrives at enumeration
  time -- no camera needs to be opened first -- so the UniqueID is minted
  directly from enumeration (`SVBONY:{name}:{serial}`, falling back to
  `SVBONY:{name}:noserial-{index}` for a camera that reports an empty
  serial). Connecting a device (C1) opens it via the SDK; a `Connected`
  device that receives another `set_connected(true)` is a no-op (C1b). An
  open failure leaves the device not connected (C2). Disconnecting (C3)
  closes the device, cancelling any in-flight exposure (C3b, implemented in
  Phase E over the generation-counter guard -- see
  docs/plans/archive/svbony-camera.md) — proven by the next session taking a
  frame of its own, which a capture still holding the device would refuse;
  the exposure state that session produced is not readable once it is
  disconnected (state-machine step 9), and the connect that follows starts
  with no frame ready. With zero cameras discovered the service
  still starts, registering no Camera devices and logging a warning (C0b).
  Against the
  svbony-rs simulation backend exactly one camera is present
  (SV605CC-Simulated, 3008x3008, colour/OSC, 14-bit, cooled, trigger-capable,
  no ST4 port).

  Background:
    Given the svbony-camera service running with the simulation backend

  Scenario: The simulated camera is registered as device 0
    Then ASCOM camera device 0 is available
    And camera device 0 reports a non-empty UniqueID

  Scenario: A camera starts disconnected
    Then camera device 0 reports Connected as false

  Scenario: Connecting opens the camera
    When I connect camera device 0
    Then camera device 0 reports Connected as true

  Scenario: Reconnecting an already-connected camera is a no-op
    Given camera device 0 is connected
    When I connect camera device 0
    Then camera device 0 reports Connected as true

  Scenario: Disconnecting leaves the camera not connected
    When I connect camera device 0
    And I disconnect camera device 0
    Then camera device 0 reports Connected as false

  Scenario: Disconnecting cancels an in-flight exposure and the next session starts clean and usable
    Given camera device 0 is connected
    And an exposure is in flight on camera device 0
    When I disconnect camera device 0
    And I try to read ImageReady from camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And camera device 0 reports Connected as false
    When I connect camera device 0
    Then camera device 0 reports ImageReady as false
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 returns an ImageArray of 64 by 48

  Scenario: The service starts with no Camera devices when no camera is present
    Given the svbony-camera service running with an empty simulation backend
    Then no ASCOM camera devices are registered
    And the service is healthy
