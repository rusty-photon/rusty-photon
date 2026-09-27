Feature: Asynchronous slewing
  SlewToCoordinatesAsync validates the target, computes target encoder
  positions from RA/Dec + LST + sync offset + side-of-pier choice, then
  issues the INDI eqmod-style sequence on each axis. Per axis the wire
  sequence is :L instant-stop + poll :f until not running (the
  Sky-Watcher firmware rejects :G against a still-decelerating motor
  with !2 MotorNotStopped), then :G goto+fast → :I step-period →
  :H delta-target → :M break-point → :J start. The call returns
  immediately; callers poll Slewing to detect completion. After both
  axes stop, the driver runs an EQMOD-style pickup loop (capped at
  5 iterations) to push any RA/Dec residual under 5". SlewToTargetAsync
  uses the most-recent TargetRightAscension / TargetDeclination set on
  the device.

  The baseline configuration these scenarios run under ships the
  default CW exclusion zone, (0.95, 11.05) h of mech_HA, so a slew
  target is subject to the same safety gate an operator's mount
  applies. The suite pins the site longitude so LST is 6.0 h at
  startup, which puts the canonical RA 6.0 h target on the meridian
  (mech_HA 0) rather than wherever the wallclock would have left it;
  scenarios that care about a specific mech_HA address their targets
  by hour angle instead.

  Scenario: A target inside the CW exclusion zone is refused before any motion
    Given a running star-adventurer service
    When I connect the device
    And I try to slew asynchronously to a target at hour angle 3.0 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-value
    And the error message should mention "CW exclusion zone"

  Scenario: A target outside the CW exclusion zone is accepted
    Given a running star-adventurer service
    When I connect the device
    And I slew asynchronously to a target at hour angle -3.0 hours and Dec 30.0 degrees
    Then TargetDeclination should be 30.0 degrees within 0.001

  Scenario: SlewToCoordinatesAsync rejects RA out of range
    Given a running star-adventurer service
    When I connect the device
    And I try to slew asynchronously to RA 24.0 hours and Dec 0.0 degrees
    Then the operation should fail with invalid-value

  Scenario: SlewToCoordinatesAsync rejects RA below zero
    Given a running star-adventurer service
    When I connect the device
    And I try to slew asynchronously to RA -0.1 hours and Dec 0.0 degrees
    Then the operation should fail with invalid-value

  Scenario: SlewToCoordinatesAsync rejects Dec above +90
    Given a running star-adventurer service
    When I connect the device
    And I try to slew asynchronously to RA 0.0 hours and Dec 90.1 degrees
    Then the operation should fail with invalid-value

  Scenario: SlewToCoordinatesAsync rejects Dec below -90
    Given a running star-adventurer service
    When I connect the device
    And I try to slew asynchronously to RA 0.0 hours and Dec -90.1 degrees
    Then the operation should fail with invalid-value

  Scenario: SlewToCoordinatesAsync fails while parked
    Given a running star-adventurer service
    And the device is parked
    When I try to slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    Then the operation should fail with invalid-while-parked

  Scenario: SlewToCoordinatesAsync issues :G :I :H :M :J on both axes
    Given a running star-adventurer service
    When I connect the device
    And I slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    Then the mount should have received commands matching:
      | pattern  |
      | :G1.*    |
      | :I1.*    |
      | :H1.*    |
      | :M1.*    |
      | :J1      |
      | :G2.*    |
      | :I2.*    |
      | :H2.*    |
      | :M2.*    |
      | :J2      |

  Scenario: SlewToCoordinatesAsync remembers the target
    Given a running star-adventurer service
    When I connect the device
    And I slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    Then TargetRightAscension should be 6.0 hours within 0.001
    And TargetDeclination should be 30.0 degrees within 0.001

  Scenario: SlewToTargetAsync without a stored target fails
    Given a running star-adventurer service
    When I connect the device
    And I try to slew to the stored target
    Then the operation should fail with invalid-operation

  Scenario: SlewToTargetAsync uses the last set target
    Given a running star-adventurer service
    When I connect the device
    And I set TargetRightAscension to 12.0 hours
    And I set TargetDeclination to 45.0 degrees
    And I slew to the stored target
    Then the slew target on the wire should correspond to RA 12.0 hours and Dec 45.0 degrees

  Scenario: Slewing returns true while a slew is in progress
    # The long post-slew settle keeps Slewing true after the mount reaches
    # the target, so the assertion observes a window that outlives the
    # scenario instead of racing the slew's natural completion.
    Given a star-adventurer service configured with a 600 second post-slew settle
    When I connect the device
    And I slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    Then Slewing should be true

  Scenario: Slewing returns false after both axes stop
    Given a running star-adventurer service
    When I connect the device
    And I slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    And the mount reports both axes stopped in goto mode
    Then Slewing should eventually be false within 5 seconds

  Scenario: Tracking resumes after a slew completes
    Given a running star-adventurer service
    When I connect the device
    And I enable tracking
    And I slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    And the mount reports both axes stopped in goto mode
    Then the mount should eventually receive a tracking-mode :G1 within 5 seconds

  Scenario: Tracking does not resume after a slew if it was off
    Given a running star-adventurer service
    When I connect the device
    And I slew asynchronously to RA 6.0 hours and Dec 30.0 degrees
    And the mount reports both axes stopped in goto mode
    Then the mount should not receive a tracking-mode :G1
