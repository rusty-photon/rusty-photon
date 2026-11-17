Feature: Connection lifecycle
  The dsd-fp2 binary opens its serial port and runs a `[GFRM]` handshake,
  which verifies the board identifies as `DeepSkyDad.FP2`, before it
  serves. `Connected = true` takes a session on that open port, and the
  cover and calibrator states read from the cache the handshake seeded
  and the poll refreshes. `Connected = false` releases the session and
  leaves the port open; a device that is not connected reports both
  states `Unknown`.

  Scenario: Device starts disconnected
    Given a running FP2 service
    Then the device should report disconnected
    And cover_state should be Unknown
    And calibrator_state should be Unknown

  Scenario: Connecting reports connected and the cached panel state
    Given a running FP2 service
    When the device is connected
    Then the device should report connected
    And cover_state should eventually be Closed
    And calibrator_state should eventually be Off

  Scenario: Disconnecting reports disconnected and the states Unknown
    Given a running FP2 service
    When the device is connected
    And the device is disconnected
    Then the device should report disconnected
    And cover_state should be Unknown
    And calibrator_state should be Unknown
