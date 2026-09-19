@serial
Feature: Connection lifecycle
  set_connected validates that the cache directory is writable. The
  survey endpoint is probed with a short HEAD request; a probe failure
  is logged at warn! but does NOT block Connect (C3) — tying ASCOM
  Connect latency to a network round-trip would make the simulator
  flaky on slow links and in CI. A non-writable cache directory still
  fails Connect with ASCOM UNSPECIFIED_ERROR (C2). Disconnect cancels
  any in-flight exposure. Every member that reports exposure state --
  CameraState, ImageReady, PercentCompleted, LastExposureStartTime,
  LastExposureDuration and ImageArray -- answers NOT_CONNECTED while
  the camera is disconnected rather than describing a session that is
  not running (C5) — before a first connect, and again after a session
  that took a frame. The session's *settings* answer the same way: the
  BinX/BinY, NumX/NumY and StartX/StartY getters refuse, and so do
  their setters and SetGain / SetReadoutMode, because a geometry a
  client cannot expose with is not a geometry (C6). What keeps
  answering is what this service knows without a device at all: the
  configured optics, the fixed sensor description, and the abort it
  performs itself.

  Background:
    Given a sky-survey-camera with default optics

  Scenario: Successful connect when cache writable and SkyView reachable
    Given a writable cache directory
    And SkyView is reachable
    When I start the service
    And I connect the camera
    Then the camera is connected

  Scenario: Connect succeeds with a warn when SkyView is unreachable
    Given a writable cache directory
    And SkyView is unreachable
    When I start the service
    And I connect the camera
    Then the camera is connected

  Scenario: Connect fails when cache directory is not writable
    Given a non-writable cache directory
    And SkyView is reachable
    When I start the service
    And I try to connect the camera
    Then the connect attempt fails with ASCOM UNSPECIFIED_ERROR
    And the camera is not connected

  Scenario: Disconnect leaves the camera not connected
    Given a writable cache directory
    And SkyView is reachable
    When I start the service
    And I connect the camera
    And I disconnect the camera
    Then the camera is not connected

  Scenario: A disconnected camera neither reports nor accepts session settings
    Given a writable cache directory
    And SkyView is reachable
    When I start the service
    Then reading these members is rejected with ASCOM NOT_CONNECTED:
      | member |
      | BinX   |
      | BinY   |
      | NumX   |
      | NumY   |
      | StartX |
      | StartY |
    And writing these members is rejected with ASCOM NOT_CONNECTED:
      | member      | value |
      | BinX        | 2     |
      | BinY        | 2     |
      | NumX        | 320   |
      | NumY        | 240   |
      | StartX      | 8     |
      | StartY      | 8     |
      | Gain        | 0     |
      | ReadoutMode | 0     |
    And reading these members still answers while disconnected:
      | member           |
      | CameraXSize      |
      | CameraYSize      |
      | MaxBinX          |
      | MaxADU           |
      | SensorType       |
      | Gain             |
      | ReadoutMode      |
      | HasShutter       |
      | CanAbortExposure |
      | CanStopExposure  |

  Scenario: The exposure-state surface answers only for a running session
    Given a writable cache directory
    And SkyView is reachable
    And the survey backend returns a healthy FITS cutout
    When I start the service
    Then reading these members is rejected with ASCOM NOT_CONNECTED:
      | member                |
      | CameraState           |
      | ImageReady            |
      | PercentCompleted      |
      | LastExposureStartTime |
      | LastExposureDuration  |
      | ImageArray            |
    When I connect the camera
    And I StartExposure with default parameters
    Then the resulting image has dimensions 640 by 480
    When I disconnect the camera
    Then reading these members is rejected with ASCOM NOT_CONNECTED:
      | member                |
      | CameraState           |
      | ImageReady            |
      | PercentCompleted      |
      | LastExposureStartTime |
      | LastExposureDuration  |
      | ImageArray            |
