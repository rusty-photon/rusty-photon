@serial
Feature: Camera enumeration and connection lifecycle
  qhy-camera enumerates every connected QHY camera (and any CFW discovered
  on it) at startup and registers each as an ASCOM device,
  index 0, 1, 2, ..., on one port (C0). Each device's UniqueID is derived
  from its SDK serial, so two identical-model cameras are distinguished by
  serial. Connect is per-device (C4): connecting or disconnecting one camera
  does not affect the others. Opening a device (C1) caches its CCD info,
  valid binning modes, and exposure/gain/offset limits. An open failure
  leaves the device not connected (C2). Disconnect closes the device and
  cancels any in-flight exposure (C3) — proven by the next session taking a
  frame of its own, which a capture still holding the device would refuse;
  the exposure state that session produced is not readable once it is
  disconnected (E10), and the connect that follows starts with no frame
  ready (C6). With zero cameras discovered the
  service still starts, registering no Camera devices and logging a warning.
  Against the qhyccd-rs simulation backend exactly one camera
  (QHY178M-Simulated, 3072x2048, monochrome, 16-bit) and one 7-position
  filter wheel are present.

  Background:
    Given the qhy-camera service running with the simulation backend

  Scenario: The simulated camera is registered as device 0
    Then ASCOM camera device 0 is available
    And camera device 0 reports a non-empty UniqueID

  Scenario: A camera starts disconnected
    Then camera device 0 reports Connected as false

  Scenario: Connecting opens the camera
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
    Given the qhy-camera service running with an empty simulation backend
    Then no ASCOM camera devices are registered
    And the service is healthy
