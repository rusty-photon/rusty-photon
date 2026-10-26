Feature: Park, unpark, and SetPark
  Park stops tracking, then slews both axes to the in-memory
  park-target encoder pair ONLY when the coordinate frame is anchored;
  with an unanchored frame Park stops both axes in place. AtPark is set
  when both axes report stopped either way. The frame is anchored by a
  named `mount.unpark_from_ap_position` (the operator's declared
  power-up pose, typically `ap_park_3` — Sky-Watcher's stock home), by
  a successful sync this connection, or by the recovery on the mount's
  setup page; the ship default `ap_park_0` ("current position",
  the only honest value when the operator declared nothing) is
  unanchored until a sync. The park
  target is resolved on connect, per axis, from the raw
  `mount.park_ra_ticks` / `mount.park_dec_ticks` override when set
  (honored regardless of anchoring — raw ticks are the operator's own
  frame assertion), otherwise from the `mount.preferred_ap_park` AP
  park (default `ap_park_3`) when anchored, otherwise no target (park
  in place). Slewing to an absolute pose from an unanchored frame would
  command real motion to a fabricated position — the workspace tenet
  "no actuation on connect" forbids it. Tracking remains disabled after
  Park (per ASCOM). Unpark clears AtPark but does not auto-enable
  tracking. SetPark captures the current encoder pair and writes it
  back into the running config file via atomic rename.

  Scenario: AtPark is false on first connect
    Given a running star-adventurer service
    When I connect the device
    Then AtPark should be false

  Scenario: Park stops tracking before slewing home
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I enable tracking
    And I park the mount
    Then the mount should have received command :K1 before any :S1
    And Tracking should be false

  Scenario: Park targets the preferred AP park when the frame is anchored
    # unpark_from_ap_position = ap_park_3 seeds the encoder on the fresh
    # power-up, anchoring the frame. Latitude 0, preferred_ap_park =
    # ap_park_3 → mech_HA = -6h (ra = -6/24 * cpr_ra = -907200 at the
    # RA CPR of 3,628,800) and dec_enc = +90° (dec = 90/360 * cpr_dec =
    # +725760 at the Dec CPR of 2,903,040 — the GTi's axes have
    # different CPRs). The seed already placed the encoder at those exact
    # values, so this park is a zero-distance goto: an install whose
    # declared power-up pose equals its preferred park is motion-free
    # by construction on a park issued right after connect.
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I park the mount
    Then the mount should have received a :S1 command targeting encoder -907200
    And the mount should have received a :S2 command targeting encoder 725760

  Scenario: Park without an anchored frame stops in place without slewing
    # ap_park_0 = "current position": the driver has no ground truth for
    # the encoder-to-pose mapping, so Park must not slew to a fabricated
    # absolute target. Both axes are stopped where they stand and AtPark
    # is set.
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_0"
    When I connect the device
    And I park the mount
    Then the mount should not have received any goto command
    And AtPark should eventually be true within 5 seconds

  Scenario: A sync anchors the frame so Park slews to the preferred AP park
    # After SyncToCoordinates the encoder-to-pose mapping is measured
    # ground truth, so the park target re-arms from preferred_ap_park
    # (default ap_park_3 at latitude 0: RA -907200 at CPR 3,628,800,
    # Dec +725760 at CPR 2,903,040).
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_0"
    When I connect the device
    And I sync to RA 6.0 hours and Dec 30.0 degrees
    And I park the mount
    Then the mount should have received a :S1 command targeting encoder -907200
    And the mount should have received a :S2 command targeting encoder 725760

  Scenario: Park targets the configured park_ra_ticks when present
    Given a star-adventurer service configured with park_ra_ticks 5000 and park_dec_ticks -7000
    When I connect the device
    And I park the mount
    Then the mount should have received a :S1 command targeting encoder 5000
    And the mount should have received a :S2 command targeting encoder -7000

  Scenario: Park is idempotent
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I park the mount
    And the mount reports both axes stopped at encoder 0
    And I park the mount
    Then the mount should not have received a second :S1 command

  Scenario: AtPark becomes true once both axes settle at the park target
    Given a star-adventurer service configured with unpark_from_ap_position "ap_park_3"
    When I connect the device
    And I park the mount
    And the mount reports both axes stopped at encoder 0
    Then AtPark should eventually be true within 5 seconds

  Scenario: Unpark clears AtPark
    Given a running star-adventurer service
    And the device is parked
    When I unpark the mount
    Then AtPark should be false

  Scenario: Unpark does not auto-enable tracking
    Given a running star-adventurer service
    And the device is parked
    When I unpark the mount
    Then Tracking should be false

  Scenario: SetPark writes the current encoder ticks back to the config file
    Given a running star-adventurer service
    And the RA-axis encoder reads 8000 ticks
    And the Dec-axis encoder reads -3000 ticks
    When I connect the device
    And I set the park position
    Then the persisted config should have park_ra_ticks 8000 and park_dec_ticks -3000

  Scenario: SetPark fails when disconnected
    Given a running star-adventurer service
    When I try to set the park position
    Then the operation should fail with not-connected

  Scenario: Park fails while disconnected
    Given a running star-adventurer service
    When I try to park the mount
    Then the operation should fail with not-connected
