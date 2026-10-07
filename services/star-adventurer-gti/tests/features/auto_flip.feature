Feature: Auto-flip during tracking
  With auto_flip.enabled = true, the per-connection tracking watcher
  initiates a driver-planned meridian flip on its own: while Tracking =
  true on the natural (pre-flip) pier side, the flip fires once the live
  encoder mech_HA reaches auto_flip.meridian_offset_hours (default 0.0 --
  flip exactly at meridian crossing; positive values delay the flip past
  the meridian). The flip is the same through-wrap flip slew
  SetSideOfPier issues (RA :G1 mode byte 01 = Goto+Fast+CCW, observable
  as :G101 on the wire), and tracking re-engages on the new pier side
  once the flip completes.

  auto_flip.enabled defaults to false -- hosts like NINA / SGP
  own flip timing themselves via SetSideOfPier, and a mid-exposure flip
  breaks running frames. The stop-only tracking guard stays the safety
  floor: it fires instead of a flip when mech_HA is already inside the
  guarded band, and remains the fallback when a flip attempt fails.

  Scenario: Tracking reaching the meridian offset triggers an automatic flip
    Given auto-flip during tracking at meridian offset 0.0 hours
    And the RA encoder is at mechanical HA 0.2 hours
    And a running star-adventurer service
    When I connect the device
    And I enable tracking
    Then the mount should have received command :G101
    And the mount should be tracking on pier side East within 30 seconds

  Scenario: No automatic flip without the auto-flip opt-in
    Given a running star-adventurer service
    And the RA encoder is at mechanical HA 0.2 hours
    When I connect the device
    And I enable tracking
    Then the mount should still be tracking after 1000 ms
    And the mount should not have received command :G101

  Scenario: Auto-flip waits for the configured meridian offset
    Given auto-flip during tracking at meridian offset 0.5 hours
    And the RA encoder is at mechanical HA 0.2 hours
    And a running star-adventurer service
    When I connect the device
    And I enable tracking
    Then the mount should still be tracking after 1000 ms
    And the mount should not have received command :G101

  Scenario: The safety guard stops the mount instead of flipping inside the guarded band
    Given a star-adventurer service with the CW exclusion zone from 0.95 to 11.05 hours
    And a tracking-guard margin of 0.05 hours
    And auto-flip during tracking at meridian offset 0.0 hours
    And the RA encoder is at mechanical HA 3.0 hours
    And a running star-adventurer service
    When I connect the device
    And I enable tracking
    Then the mount should stop tracking within 6000 ms
    And the mount should not have received command :G101
