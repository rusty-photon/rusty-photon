Feature: Telescope-following pointing mode (F1, F2, F5, F6, F7, F8)

  When `pointing.telescope` is configured, every `StartExposure`
  reads RA/Dec from the configured ASCOM Telescope (with an
  optional configurable arcsec offset) and uses that as the
  exposure's pointing snapshot. Static-mode contracts P1–P7 are
  unchanged when `pointing.telescope` is absent — those scenarios
  live in `pointing_api.feature`.

  In follow mode, `POST /sky-survey/position` arms a one-shot
  pointing override (F6 + F7) consumed by the next light exposure;
  subsequent exposures resume reading the mount. This is a test
  affordance for injecting "the camera saw something different from
  where the mount thinks it is" on a single capture.

  When `pointing.rotator` is also configured, each light exposure
  additionally reads `position` from the ASCOM Rotator and uses it
  as the snapshot's rotation (F8); without it, rotation stays at the
  static `initial_rotation_deg`.

  Scenario: F1/F4 — each exposure reads the mount fresh
    Given a mount reports RA 10.0 hours and Dec 30.0 degrees
    And the camera is configured to follow that mount
    And the camera is started and connected in follow mode
    Then after a successful exposure, the position endpoint reports RA approximately 150.0 Dec approximately 30.0
    When the mount is updated to RA 6.0 hours and Dec -45.0 degrees
    Then after another successful exposure, the position endpoint reports RA approximately 90.0 Dec approximately -45.0

  Scenario: F2 — mount read failure surfaces as an exposure error
    Given a mount that errors on every read
    And the camera is configured to follow that mount
    And the camera is started and connected in follow mode
    When I StartExposure with default parameters
    Then the exposure fails with ASCOM UNSPECIFIED_ERROR

  Scenario: F5a — RA offset shifts the snapshot east-west
    Given a mount reports RA 10.0 hours and Dec 30.0 degrees
    And the camera is configured to follow that mount with offset RA 3600 arcsec and Dec 0 arcsec
    And the camera is started and connected in follow mode
    Then after a successful exposure, the position endpoint reports RA approximately 151.0 Dec approximately 30.0

  Scenario: F5b — Dec offset shifts the snapshot north-south
    Given a mount reports RA 10.0 hours and Dec 30.0 degrees
    And the camera is configured to follow that mount with offset RA 0 arcsec and Dec -3600 arcsec
    And the camera is started and connected in follow mode
    Then after a successful exposure, the position endpoint reports RA approximately 150.0 Dec approximately 29.0

  Scenario: F6/F7 — POST in follow mode arms a one-shot pointing override
    Given a mount reports RA 10.0 hours and Dec 30.0 degrees
    And the camera is configured to follow that mount
    And the camera is started and connected in follow mode
    When I POST RA 1 Dec 2 to the position endpoint
    Then the position response status is 204
    Then after a successful exposure, the position endpoint reports RA approximately 1.0 Dec approximately 2.0
    Then after another successful exposure, the position endpoint reports RA approximately 150.0 Dec approximately 30.0

  Scenario: F8 — the rotator's position angle drives the exposure rotation, read fresh each time
    Given a mount reports RA 10.0 hours and Dec 30.0 degrees
    And a rotator reports position angle 42.0 degrees
    And the camera is configured to follow that mount
    And the camera is started and connected in follow mode
    Then after a successful exposure, the position endpoint reports RA approximately 150.0 Dec approximately 30.0 and rotation approximately 42.0
    When the rotator is updated to position angle 17.5 degrees
    Then after another successful exposure, the position endpoint reports RA approximately 150.0 Dec approximately 30.0 and rotation approximately 17.5

  Scenario: F8 — rotator read failure surfaces as an exposure error
    Given a mount reports RA 10.0 hours and Dec 30.0 degrees
    And a rotator that errors on every read
    And the camera is configured to follow that mount
    And the camera is started and connected in follow mode
    When I StartExposure with default parameters
    Then the exposure fails with ASCOM UNSPECIFIED_ERROR
