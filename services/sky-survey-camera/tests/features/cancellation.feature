@serial
Feature: Exposure cancellation
  AbortExposure and StopExposure cancel an in-flight survey fetch and
  leave ImageReady false. ASCOM forbids either from throwing on an idle
  camera, so with nothing in flight both succeed and leave a frame an
  earlier exposure left ready untouched. While disconnected both return
  ASCOM NOT_CONNECTED, which is the only thing a refusal can still mean.

  Scenario: AbortExposure cancels an in-flight exposure
    Given the camera is connected with the survey backend stubbed
    And an exposure is already in flight
    When I AbortExposure
    Then the cancellation succeeds
    And ImageReady is false

  Scenario: StopExposure cancels an in-flight exposure
    Given the camera is connected with the survey backend stubbed
    And an exposure is already in flight
    When I StopExposure
    Then the cancellation succeeds
    And ImageReady is false

  Scenario: AbortExposure with no exposure in progress succeeds
    Given the camera is connected with the survey backend stubbed
    When I AbortExposure
    Then the cancellation succeeds

  Scenario: StopExposure with no exposure in progress succeeds
    Given the camera is connected with the survey backend stubbed
    When I StopExposure
    Then the cancellation succeeds

  Scenario: AbortExposure leaves a completed frame readable
    Given the camera is connected with the survey backend stubbed
    And an exposure has completed
    When I AbortExposure
    Then the cancellation succeeds
    And ImageReady is true

  Scenario: StopExposure leaves a completed frame readable
    Given the camera is connected with the survey backend stubbed
    And an exposure has completed
    When I StopExposure
    Then the cancellation succeeds
    And ImageReady is true

  Scenario: AbortExposure while disconnected is rejected
    Given the camera is started but not connected
    When I AbortExposure
    Then the exposure is rejected with ASCOM NOT_CONNECTED

  Scenario: StopExposure while disconnected is rejected
    Given the camera is started but not connected
    When I StopExposure
    Then the exposure is rejected with ASCOM NOT_CONNECTED
