Feature: Meridian-flip support
  The driver always plans meridian flips; no setting turns them off.
  CanSetPierSide reports true, and SetSideOfPier(side) triggers a
  through-wrap flip slew that keeps the OTA on its current celestial
  target while landing on the requested pier side. The slew planner
  picks the side via the decision tree shared with
  DestinationSideOfPier: stay on the current side when its safety
  envelope covers the target HA, flip to the opposite side otherwise.

  Through-wrap routing is observable on the wire: a flip slew from
  the Northern-Hemisphere pre-flip side (pierWest at mech_HA ≈ 0)
  toward the post-flip side issues :G1 with the CCW bit set (mode
  byte 01 = Goto+Fast+CCW), routing the RA encoder through the
  negative-mech_HA half (counterweight-below-horizon arc) and the
  encoder wrap at -12 h to the mirror band on the post-flip side.

  Scenario: CanSetPierSide reports true
    Given a running star-adventurer service
    When I connect the device
    Then CanSetPierSide should be true

  Scenario: SetSideOfPier with Unknown returns invalid-value
    Given a running star-adventurer service
    When I connect the device
    And I try to set SideOfPier to Unknown
    Then the operation should fail with invalid-value

  Scenario: SetSideOfPier refuses when not connected
    Given a running star-adventurer service
    When I try to set SideOfPier to East
    Then the operation should fail with not-connected

  Scenario: SetSideOfPier refuses while parked
    Given a running star-adventurer service
    When I connect the device
    And I park the mount
    And I try to set SideOfPier to East
    Then the operation should fail with invalid-while-parked

  Scenario: SetSideOfPier to the current side succeeds without changing pier side
    Given a running star-adventurer service
    When I connect the device
    And I set SideOfPier to West
    Then SideOfPier should be West

  Scenario: SetSideOfPier(East) from pierWest issues a CCW Goto on the RA axis
    Given a running star-adventurer service
    When I connect the device
    And I set SideOfPier to East
    Then the mount should have received command :G101

  Scenario: SetSideOfPier(East) marks Slewing while the flip is in progress
    # The long post-slew settle keeps Slewing true after the flip motion
    # completes, so the assertion observes a window that outlives the
    # scenario instead of racing the flip's natural completion.
    Given a star-adventurer service configured with a 600 second post-slew settle
    When I connect the device
    And I set SideOfPier to East
    Then Slewing should be true

  Scenario: AbortSlew during a SetSideOfPier flip halts both axes
    Given a running star-adventurer service
    When I connect the device
    And I set SideOfPier to East
    And I abort the slew
    Then the mount should have received command :L1
    And the mount should have received command :L2

  Scenario: DestinationSideOfPier returns the current side when target is reachable from it
    Given a star-adventurer service configured with site latitude 45.0 degrees
    When I connect the device
    And I read DestinationSideOfPier for RA 6.0 hours and Dec 30.0 degrees
    Then DestinationSideOfPier should be West
