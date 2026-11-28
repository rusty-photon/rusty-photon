Feature: Coordinate reads
  RightAscension and Declination are derived from the polled axis encoder
  positions, the configured site longitude (for LST), and the cached
  CPR-per-axis. Reads while disconnected fail with NOT_CONNECTED.

  Scenario: RightAscension fails while disconnected
    Given a running star-adventurer service
    When I try to read RightAscension
    Then the operation should fail with not-connected

  Scenario: Declination fails while disconnected
    Given a running star-adventurer service
    When I try to read Declination
    Then the operation should fail with not-connected

  Scenario: RightAscension at the meridian for sidereal-rest pose
    Given a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the RA-axis encoder reads 0 ticks
    And site longitude is 0 degrees
    And UTC is "2026-01-01T00:00:00Z"
    And a running star-adventurer service
    When I connect the device
    Then RightAscension should equal SiderealTime within 0.001 hours

  Scenario: Declination at the celestial equator for encoder zero
    Given a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reads 0 ticks
    And a running star-adventurer service
    When I connect the device
    Then Declination should be 0.0 degrees within 0.001

  Scenario: Declination at the celestial equator on the counterweight-up side reads 0.0, not -0.0
    On the counterweight-up side the equator sits at the Dec encoder's
    wrap, half a revolution from zero. SideOfPier reading East shows the
    seeded encoder has reached the reads.
    Given a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reports angle 180.0 degrees
    And a running star-adventurer service
    When I connect the device
    Then SideOfPier should be East
    And Declination should be exactly 0.0 degrees

  Scenario: SiderealTime is computed from UTC and SiteLongitude
    Given a star-adventurer service configured with site longitude 0 degrees
    And UTC is "2026-01-01T12:00:00Z"
    When I connect the device
    Then SiderealTime should be approximately 18.7397 hours within 0.01

  Scenario: Slewing is false when both axes report stopped
    Given a running star-adventurer service
    And the mount reports both axes stopped
    When I connect the device
    Then Slewing should be false

  Scenario: Slewing is true while either axis is in goto motion
    Given a running star-adventurer service
    And the mount reports the RA axis running in goto mode
    When I connect the device
    Then Slewing should be true
