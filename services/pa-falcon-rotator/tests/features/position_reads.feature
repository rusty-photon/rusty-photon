Feature: Position reads
  Each property read maps to a single FA request — there is no cached state.
  MechanicalPosition is the device's raw position in [0, 360), exactly as
  the FA degree field reports it. Position adds the driver-side sync_offset,
  re-normalises, and rounds to the device's 1/100°. TargetPosition returns
  the stored sky-coordinate target when one is outstanding, otherwise it
  falls back to the current Position. Every angle is exact: the double
  nearest its two-decimal value, with no floating-point residue.

  Scenario: MechanicalPosition is the device's reported degrees, exactly
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 147.84 degrees
    And I read MechanicalPosition
    Then MechanicalPosition should be 147.84 degrees
    And an FA command should have been issued

  Scenario: Position adds the driver-side sync offset to MechanicalPosition
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 142.30 degrees
    And the driver-side sync offset is -104.80 degrees
    And I read Position
    Then Position should be 37.50 degrees

  Scenario: Position wraps a sum past 360 and stays exact to 1/100°
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 200.07 degrees
    And the driver-side sync offset is -12.16 degrees
    And I read Position
    Then Position should be 187.91 degrees

  Scenario: Position normalises offset-induced negative values into [0, 360)
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 10.00 degrees
    And the driver-side sync offset is -30.00 degrees
    And I read Position
    Then Position should be 340.00 degrees

  Scenario: TargetPosition falls back to Position when no move is outstanding
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 147.84 degrees
    And I read TargetPosition
    Then TargetPosition should be 147.84 degrees
