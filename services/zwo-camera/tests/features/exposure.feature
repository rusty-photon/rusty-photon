@serial
Feature: Exposure lifecycle
  StartExposure requires a connected device (E1, NOT_CONNECTED) and rejects
  a second exposure while one is in flight (E2, INVALID_OPERATION). A
  Duration outside [ExposureMin, ExposureMax] is rejected with INVALID_VALUE
  (E3). ASI sensors have no mechanical shutter, so a dark frame (Light =
  false) is accepted on every model and captured identically (E4) and
  HasShutter is false. A successful exposure produces an ImageArray of the
  binned sub-frame with ImageReady true and the last-exposure timestamps set
  (E5); CameraState is Exposing while in flight and PercentCompleted reaches
  100 once ready (E6). AbortExposure discards an in-flight frame and
  CanAbortExposure is true (E7); StopExposure gracefully stops and preserves
  the frame for readout, and CanStopExposure is true (E8) -- both back onto
  ASIStopExposure. Pulse guiding is supported via ST4 (PG1, CanPulseGuide
  true) and PulseGuide on a disconnected device is rejected with NOT_CONNECTED
  (PG2); the no-ST4 NOT_IMPLEMENTED branch of PG2 is covered by unit tests,
  since the simulation backend always reports ST4 present. A mid-exposure SDK
  error transitions to the Error state (E9, covered by unit tests against the
  mock SDK seam). The exposure state belongs to the session that produced it,
  so every member reporting it -- CameraState, ImageReady, PercentCompleted,
  LastExposureStartTime, LastExposureDuration and ImageArray -- answers
  NOT_CONNECTED once the device is disconnected, rather than from the session
  that has ended (E11).

  Background:
    Given the zwo-camera service running with the simulation backend

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

  Scenario: A dark frame is accepted on a shutterless camera
    Given camera device 0 is connected
    And camera device 0 reports HasShutter as false
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light false
    And the exposure on camera device 0 completes
    Then camera device 0 reports ImageReady as true

  Scenario: A successful light exposure produces an image of the requested sub-frame
    Given camera device 0 is connected
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    Then camera device 0 reports ImageReady as true
    And camera device 0 returns an ImageArray of 64 by 48
    And camera device 0 reports a set LastExposureStartTime
    And camera device 0 reports LastExposureDuration as 0.01

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

  Scenario: StopExposure gracefully stops and preserves the frame for readout
    Given camera device 0 is connected
    And camera device 0 reports CanStopExposure as true
    And an exposure is in flight on camera device 0
    When I stop the exposure on camera device 0
    Then camera device 0 reports ImageReady as true

  Scenario: Pulse guiding is supported via ST4
    Given camera device 0 is connected
    Then camera device 0 reports CanPulseGuide as true

  Scenario: A disconnected camera rejects PulseGuide
    Given camera device 0 is not connected
    When I try to PulseGuide on camera device 0 in direction North for 100 ms
    Then the PulseGuide is rejected with ASCOM NOT_CONNECTED

  Scenario Outline: A disconnected camera reports no exposure state from the session that ended
    Given camera device 0 is connected
    When I StartExposure on camera device 0 with BinX 1 BinY 1 NumX 64 NumY 48 StartX 0 StartY 0 Duration 0.01 Light true
    And the exposure on camera device 0 completes
    And I disconnect camera device 0
    And I try to read <member> from camera device 0
    Then the call is rejected with ASCOM NOT_CONNECTED

    Examples:
      | member                |
      | CameraState           |
      | ImageReady            |
      | PercentCompleted      |
      | LastExposureStartTime |
      | LastExposureDuration  |
      | ImageArray            |
