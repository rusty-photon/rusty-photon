@serial
Feature: Exposure lifecycle
  StartExposure requires a connected device (E1, NOT_CONNECTED) and rejects
  a second exposure while one is in flight (E2, INVALID_OPERATION). A
  Duration outside [ExposureMin, ExposureMax] is rejected with INVALID_VALUE
  (E3). Dark frames (Light = false) return NOT_IMPLEMENTED on all models in v0
  — qhyccd-rs 0.1.9 exposes mechanical-shutter presence but no open/close
  actuation, so no model can capture a true dark (E4); the simulated
  QHY178M-Simulated is shutterless. A successful
  light exposure produces an ImageArray of the binned sub-frame with
  ImageReady true and the last-exposure timestamps set (E5); a full frame is
  CameraXSize by CameraYSize, the sensor's effective area rather than the
  chip's overscan margin (G1). CameraState is
  Exposing while in flight and PercentCompleted reaches 100 once ready (E6).
  AbortExposure cancels an in-flight exposure and CanAbortExposure is true
  (E7); StopExposure is not implemented and CanStopExposure is false (E8).
  Mid-exposure SDK error transitions to the Error state (E9, covered by unit
  tests against the mock SDK seam).

  Background:
    Given the qhy-camera service running with the simulation backend

  Scenario: A disconnected camera rejects StartExposure
    Given camera device 0 is not connected
    When I try to StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 64 StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is rejected with ASCOM NOT_CONNECTED

  Scenario: A second exposure while one is in flight is rejected
    Given camera device 0 is connected
    And an exposure is in flight on camera device 0
    When I try to StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 64 StartX 0 StartY 0 Duration 0.01 Light true
    Then the exposure is rejected with ASCOM INVALID_OPERATION

  Scenario Outline: An out-of-range duration is rejected
    Given camera device 0 is connected
    When I try to StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 64 StartX 0 StartY 0 Duration <duration> Light true
    Then the exposure is rejected with ASCOM INVALID_VALUE

    Examples:
      | duration |
      | -1.0     |
      | 100000.0 |

  Scenario: A shutterless camera rejects a dark frame
    Given camera device 0 is connected
    And camera device 0 reports HasShutter as false
    When I try to StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 64 StartX 0 StartY 0 Duration 0.01 Light false
    Then the exposure is rejected with ASCOM NOT_IMPLEMENTED

  Scenario: A successful light exposure produces an image of the requested sub-frame
    Given camera device 0 is connected
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports ImageReady as true
    And camera device 0 returns an ImageArray of 64 by 48
    And camera device 0 reports a set LastExposureStartTime
    And camera device 0 reports LastExposureDuration as 0.01

  Scenario: A full-frame exposure delivers exactly CameraXSize by CameraYSize
    Given camera device 0 is connected
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 3048 NumY 2044 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 returns an ImageArray of 3048 by 2044
    And every row and column of the ImageArray from camera device 0 carries data

  Scenario: PercentCompleted is 100 once the image is ready
    Given camera device 0 is connected
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 64 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports CameraState as Idle
    And camera device 0 reports PercentCompleted as 100

  Scenario: Aborting an in-flight exposure leaves no image ready
    Given camera device 0 is connected
    And camera device 0 reports CanAbortExposure as true
    And an exposure is in flight on camera device 0
    When I abort the exposure on camera device 0
    Then camera device 0 reports ImageReady as false
    And camera device 0 reports CameraState as Idle

  Scenario: A fresh exposure completes immediately after an abort
    Given camera device 0 is connected
    And an exposure is in flight on camera device 0
    When I abort the exposure on camera device 0
    And I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 64 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports ImageReady as true
    And camera device 0 reports CameraState as Idle

  Scenario: Disconnecting during an exposure closes the device cleanly
    Given camera device 0 is connected
    And an exposure is in flight on camera device 0
    When I disconnect camera device 0
    Then camera device 0 reports Connected as false

  Scenario: StopExposure is not implemented
    Given camera device 0 is connected
    Then camera device 0 reports CanStopExposure as false
    When I try to StopExposure on camera device 0
    Then the call is rejected with ASCOM NOT_IMPLEMENTED

  Scenario: Pulse guiding is not supported
    Given camera device 0 is connected
    Then camera device 0 reports CanPulseGuide as false
