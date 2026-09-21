Feature: Pier-side selection follows the counterweight exclusion zone
  Pier side is an output, not a policy. For a target RA/Dec there are
  two encoder solutions; the driver uses the one whose destination
  mech_HA lies outside the CW exclusion zone and whose RA sweep from
  the current encoder position does not cross it, preferring to stay
  on the side the mount is already on. Slewing and
  DestinationSideOfPier share that one selector, so a prediction and
  the slew it predicts can never disagree.

  The zone has the shape (x, 12 - x), x being the counterweight-up
  allowance - 0.95 h = 57 minutes by default. Three behaviours follow
  from that one number: tracking runs to 57 minutes past the meridian
  on the counterweight-down side; a target up to 57 minutes east of
  the meridian can be acquired counterweight-up; and inside
  |target_HA| <= 57 minutes both sides work, so the mount stays where
  it is. Because the counterweight-up side's mech_HA = target_HA - 12
  is negative for every target west of the meridian, that side reaches
  the whole western sky - a mount that is already counterweight-up
  does not flip back to chase a western target, and is not refused
  one.

  These scenarios address targets by hour angle (the steps compute
  RA = LST - HA when they run) because the zone is expressed in
  mech_HA, which a hardcoded RA would tie to the wallclock. Northern
  hemisphere: counterweight-down is pierWest, counterweight-up is
  pierEast. The mount is placed counterweight-up by seeding a Dec
  encoder past the pole, the same classification SideOfPier applies.

  Scenario: A western target keeps a counterweight-up mount where it is
    # Issue #1301: the flipped side is the only side that reaches
    # HA +3 (its mech_HA is -9, outside the zone; the
    # counterweight-down mech_HA of +3 is inside it), so staying is
    # the only correct answer.
    Given a star-adventurer service configured with flip_policy enabled, site latitude 45.0 degrees and a CW exclusion zone of 0.95 to 11.05 hours
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reports angle 135.0 degrees
    And a running star-adventurer service
    When I connect the device
    And the mount has settled on pier side East
    And I read DestinationSideOfPier for a target at hour angle 3.0 hours and Dec 30.0 degrees
    Then DestinationSideOfPier should be East

  Scenario: A western target is accepted while the mount is counterweight-up
    Given a star-adventurer service configured with flip_policy enabled, site latitude 45.0 degrees and a CW exclusion zone of 0.95 to 11.05 hours
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reports angle 135.0 degrees
    And a running star-adventurer service
    When I connect the device
    And the mount has settled on pier side East
    And I slew asynchronously to a target at hour angle 3.0 hours and Dec 30.0 degrees
    Then TargetDeclination should be 30.0 degrees within 0.001

  Scenario: A target inside the overlap band does not move the mount off its side
    # |HA| = 0.7 h is inside the +/-0.95 h band both sides reach. The
    # mount stays counterweight-up rather than slewing back through
    # the wrap for a target it can already see.
    Given a star-adventurer service configured with flip_policy enabled, site latitude 45.0 degrees and a CW exclusion zone of 0.95 to 11.05 hours
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reports angle 135.0 degrees
    And a running star-adventurer service
    When I connect the device
    And the mount has settled on pier side East
    And I read DestinationSideOfPier for a target at hour angle 0.7 hours and Dec 30.0 degrees
    Then DestinationSideOfPier should be East

  Scenario: An eastern target the counterweight-up side cannot reach flips the mount back
    # HA -3 puts the counterweight-up mech_HA at +9, inside the zone;
    # counterweight-down reaches it at -3.
    Given a star-adventurer service configured with flip_policy enabled, site latitude 45.0 degrees and a CW exclusion zone of 0.95 to 11.05 hours
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reports angle 135.0 degrees
    And a running star-adventurer service
    When I connect the device
    And the mount has settled on pier side East
    And I read DestinationSideOfPier for a target at hour angle -3.0 hours and Dec 30.0 degrees
    Then DestinationSideOfPier should be West

  Scenario: A western target flips a counterweight-down mount
    Given a star-adventurer service configured with flip_policy enabled, site latitude 45.0 degrees and a CW exclusion zone of 0.95 to 11.05 hours
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And a running star-adventurer service
    When I connect the device
    And the mount has settled on pier side West
    And I read DestinationSideOfPier for a target at hour angle 3.0 hours and Dec 30.0 degrees
    Then DestinationSideOfPier should be East

  Scenario: An eastern target keeps a counterweight-down mount where it is
    Given a star-adventurer service configured with flip_policy enabled, site latitude 45.0 degrees and a CW exclusion zone of 0.95 to 11.05 hours
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And a running star-adventurer service
    When I connect the device
    And the mount has settled on pier side West
    And I read DestinationSideOfPier for a target at hour angle -3.0 hours and Dec 30.0 degrees
    Then DestinationSideOfPier should be West

  Scenario: Sync while counterweight-up writes the counterweight-up encoder solution
    # Dec +30 reached counterweight-up is a Dec encoder at
    # 180 - 30 = 150 degrees, past the pole. Writing +30 instead
    # would tell the firmware the mount is counterweight-down and
    # leave every later slew planning from a false position.
    Given a star-adventurer service configured with flip_policy enabled, site latitude 45.0 degrees and a CW exclusion zone of 0.95 to 11.05 hours
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reports angle 135.0 degrees
    And a running star-adventurer service
    When I connect the device
    And the mount has settled on pier side East
    And I sync to a target at hour angle 3.0 hours and Dec 30.0 degrees
    Then the Dec encoder position written to the wire should be 150.0 degrees within 0.01

  Scenario: Sync while counterweight-down writes the counterweight-down encoder solution
    Given a star-adventurer service configured with flip_policy enabled, site latitude 45.0 degrees and a CW exclusion zone of 0.95 to 11.05 hours
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And a running star-adventurer service
    When I connect the device
    And the mount has settled on pier side West
    And I sync to a target at hour angle -3.0 hours and Dec 30.0 degrees
    Then the Dec encoder position written to the wire should be 30.0 degrees within 0.01
