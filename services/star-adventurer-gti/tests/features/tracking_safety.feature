Feature: Tracking-time exclusion-zone safety guard
  While Tracking = true a background guard watches the live encoder
  mech_HA and stops the mount (:K1) before tracking drift can carry the
  counterweights into the CW exclusion zone (mech_HA inside the active
  cw_exclusion_zone interval (min_hours, max_hours), default
  (0.95, 11.05) h). The guard fires once mech_HA enters the band widened
  by tracking_guard_margin_hours on each edge -- the configurable margin
  (default 0.05 h, ~45 s of sidereal drift) lets cautious operators stop
  before the zone entry rather than at it.

  The guard does not pick a pier side or flip: it stops the mount,
  clears the in-memory Tracking flag to match, and warns, leaving the
  operator (or higher-level automation) to flip via SetSideOfPier, slew
  elsewhere, or park. It is the safety floor and runs whenever the zone
  is active, independent of auto-flip.

  Scenario: Tracking drifting into the exclusion zone stops the mount
    Given a star-adventurer service with the CW exclusion zone from 0.95 to 11.05 hours
    And a tracking-guard margin of 0.05 hours
    And the RA encoder is at mechanical HA 3.0 hours
    And a running star-adventurer service
    When I connect the device
    And I enable tracking
    Then the mount should stop tracking within 6000 ms
    And the mount should have received command :K1
    And Slewing should be false

  Scenario: Tracking clear of the exclusion zone keeps running
    Given a star-adventurer service with the CW exclusion zone from 0.95 to 11.05 hours
    And a tracking-guard margin of 0.05 hours
    And the RA encoder is at mechanical HA -3.0 hours
    And a running star-adventurer service
    When I connect the device
    And I enable tracking
    Then the mount should still be tracking after 1000 ms

  Scenario: The margin stops tracking before the zone entry is reached
    # mech_HA 0.92 is below the 0.95 zone entry but inside the 0.05 h
    # margin band (0.90, 11.10), so the guard stops the mount early.
    Given a star-adventurer service with the CW exclusion zone from 0.95 to 11.05 hours
    And a tracking-guard margin of 0.05 hours
    And the RA encoder is at mechanical HA 0.92 hours
    And a running star-adventurer service
    When I connect the device
    And I enable tracking
    Then the mount should stop tracking within 6000 ms
    And the mount should have received command :K1

  Scenario: The guard is inactive when the exclusion zone is disabled
    Given a star-adventurer service with the CW exclusion zone disabled
    And a tracking-guard margin of 0.05 hours
    And the RA encoder is at mechanical HA 3.0 hours
    And a running star-adventurer service
    When I connect the device
    And I enable tracking
    Then the mount should still be tracking after 1000 ms
