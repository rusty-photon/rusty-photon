Feature: Side of pier
  SideOfPier is derived from the Dec-axis encoder position and the
  configured site latitude. Convention follows ASCOM and INDI eqmod:
  in the northern hemisphere, a Dec encoder magnitude within ±90°
  (= ±cpr_dec/4 ticks) of home maps to pierWest (the "normal"
  pointing state — counterweight east, OTA west of pier); a Dec
  encoder past 90° in either direction (the mount has rotated the
  Dec axis through one of the celestial poles) maps to pierEast.
  The boundary at exactly ±90° is West (the mount can sit at the
  pole via normal pointing). Southern hemisphere inverts the
  convention. DestinationSideOfPier applies the same Dec-encoder
  check to the encoder pair the mount would land at for a target
  RA/Dec, and rejects targets that fall outside the safety envelope
  with the same INVALID_VALUE error a slew would.

  Scenario Outline: Northern-hemisphere SideOfPier per Dec encoder angle
    Given a star-adventurer service configured with site latitude 45.0 degrees
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reports angle <dec_deg> degrees
    And a running star-adventurer service
    When I connect the device
    Then SideOfPier should be <expected>

    Examples:
      | dec_deg | expected |
      | -90.0   | West     |
      | -45.0   | West     |
      | -0.01   | West     |
      | 0.0     | West     |
      | 45.0    | West     |
      | 90.0    | West     |
      | 90.001  | East     |
      | 135.0   | East     |
      | 180.0   | East     |

  Scenario Outline: Southern-hemisphere SideOfPier inverts the convention
    Given a star-adventurer service configured with site latitude -33.0 degrees
    And a mount with CPR 3628800 on the RA axis and 2903040 on the Dec axis
    And the Dec-axis encoder reports angle <dec_deg> degrees
    And a running star-adventurer service
    When I connect the device
    Then SideOfPier should be <expected>

    Examples:
      | dec_deg | expected |
      | -45.0   | East     |
      | 0.0     | East     |
      | 45.0    | East     |
      | 90.0    | East     |
      | 90.001  | West     |
      | 180.0   | West     |

  Scenario: DestinationSideOfPier predicts West for a valid northern-hemisphere target
    Given a star-adventurer service configured with site latitude 45.0 degrees
    And a running star-adventurer service
    When I connect the device
    And I read DestinationSideOfPier for RA 6.0 hours and Dec 30.0 degrees
    Then DestinationSideOfPier should be West

  Scenario: DestinationSideOfPier predicts East for a valid southern-hemisphere target
    Given a star-adventurer service configured with site latitude -33.0 degrees
    And a running star-adventurer service
    When I connect the device
    And I read DestinationSideOfPier for RA 6.0 hours and Dec 30.0 degrees
    Then DestinationSideOfPier should be East

  Scenario: DestinationSideOfPier rejects targets outside the safety envelope
    Given a star-adventurer service configured with site latitude 45.0 degrees
    And a running star-adventurer service
    When I connect the device
    And I try to read DestinationSideOfPier for RA 6.0 hours and Dec 95.0 degrees
    Then the operation should fail with invalid-value
