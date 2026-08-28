@serial
Feature: Exposure lifecycle (soft-trigger video capture)
  SVBony has no snap-exposure API: every exposure rides video capture. Per
  docs/services/svbony-camera.md "Behavioral contracts -> Exposure", on
  connect (when IsTriggerCam) the driver selects SVB_MODE_TRIG_SOFT and
  starts video capture once; each ASCOM StartExposure sets SVB_EXPOSURE (in
  microseconds) then calls SendSoftTrigger, then polls GetVideoData under a
  read deadline of twice the exposure plus 500ms, floored at 5s: the SDK's own
  per-exposure recommendation, under a floor that covers the fixed SDK setup
  cost a session's first read pays.

  StartExposure requires a connected device (E1, NOT_CONNECTED) and rejects a
  second exposure while one is in flight (E2, INVALID_OPERATION). A Duration
  outside [ExposureMin, ExposureMax] is rejected with INVALID_VALUE (E3).
  There is no mechanical shutter in video mode, so a dark frame (Light =
  false) is accepted and captured identically (E4) and HasShutter is false.
  A successful exposure produces an ImageArray of the requested sub-frame
  with ImageReady true and the last-exposure timestamps set (E5);
  CameraState is Exposing while in flight and PercentCompleted reaches 100
  once ready (E6). AbortExposure stops video capture and discards the frame
  (E7, CanAbortExposure true). There is no data-preserving stop at the SDK
  level, so StopExposure is NOT_IMPLEMENTED and CanStopExposure is false
  (E8) -- the opposite of zwo-camera's graceful stop, and to be
  confirmed/revised after real-hardware validation. A mid-exposure SDK error
  or an exceeded GetVideoData deadline transitions to the Error state (E9,
  covered by unit tests against the mock backend seam, not BDD). The
  simulated SV605CC-Simulated camera reports no ST4 port, so CanPulseGuide
  is false and PulseGuide is NOT_IMPLEMENTED (PG1/PG2) -- ST4 stays
  capability-driven (SVBCanPulseGuide), not model-driven, for cameras that
  do have a port.

  Background:
    Given the svbony-camera service running with the simulation backend

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
      | 2500.0   |

  Scenario: A dark frame is accepted with no mechanical shutter
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

  Scenario: Aborting an in-flight exposure stops capture and leaves no image ready
    Given camera device 0 is connected
    And camera device 0 reports CanAbortExposure as true
    And an exposure is in flight on camera device 0
    When I abort the exposure on camera device 0
    Then camera device 0 reports ImageReady as false

  Scenario: There is no data-preserving stop
    Given camera device 0 is connected
    Then camera device 0 reports CanStopExposure as false

  Scenario: Attempting StopExposure is not implemented
    Given camera device 0 is connected
    And an exposure is in flight on camera device 0
    When I try to stop the exposure on camera device 0
    Then the exposure is rejected with ASCOM NOT_IMPLEMENTED

  Scenario: A camera without an ST4 port does not support pulse guiding
    Given camera device 0 is connected
    Then camera device 0 reports CanPulseGuide as false

  Scenario: PulseGuide is rejected on a camera with no ST4 port
    Given camera device 0 is connected
    When I try to PulseGuide on camera device 0 in direction North for 100 ms
    Then the PulseGuide is rejected with ASCOM NOT_IMPLEMENTED
