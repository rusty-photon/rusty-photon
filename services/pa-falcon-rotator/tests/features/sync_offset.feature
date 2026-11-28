Feature: Driver-side Sync
  ASCOM Sync(skyAngle) sets the value reported by Position to skyAngle while
  leaving MechanicalPosition unchanged. The Falcon's SD command rewrites the
  device's stored counter, which would change MechanicalPosition on the next
  FA read — so the driver tracks the offset in software and never issues SD.
  The offset is held at the device's 1/100°, like the mechanical angle it
  joins to the sky: a Sync to an angle with finer digits reads back to a
  hundredth, and MoveAbsolute to the Position read commands the mechanical
  angle the rotator is at.

  ASCOM requires the offset to persist across driver starts, so the driver
  keeps it in a record beside its config file, named after it: config.json
  has config.sync-offset.json, holding {"sync_offset_deg": <offset>}. Sync
  writes the record before it returns; a Sync whose record cannot be written
  fails with INVALID_OPERATION (1035) and leaves the offset as it was. Only
  Sync changes the offset: a disconnect, even the last client's, keeps it.

  Scenario: Sync shifts Position without changing MechanicalPosition
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 142.30 degrees
    And I call Sync with 37.50
    Then no SD command should have been sent
    And MechanicalPosition should be unchanged

  Scenario: After Sync, Position reports the synced sky angle
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 142.30 degrees
    And I call Sync with 37.50
    And I read Position
    Then Position should be 37.50 degrees

  Scenario: A move to the Position read after a Sync to a finer angle commands where the rotator is
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 100.00 degrees
    And I call Sync with 112.345
    And I read Position
    Then Position should be 112.35 degrees
    When I call MoveAbsolute with 112.35
    Then MD:100.00 should have been sent
    When I read TargetPosition
    Then TargetPosition should be 112.35 degrees

  Scenario: The sync offset survives the last client's disconnect
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 142.30 degrees
    And I call Sync with 37.50
    And I disconnect the rotator
    And I connect the rotator
    And I read Position
    Then Position should be 37.50 degrees

  Scenario: Sync writes the offset to the record beside the config file
    Given a running pa-falcon-rotator process
    When I connect the rotator
    And I read MechanicalPosition
    Then MechanicalPosition should be 0.00 degrees
    When I call Sync with 37.50
    Then the record config.sync-offset.json beside the config holds sync_offset_deg 37.50

  Scenario: The record holds the offset to the device's 1/100°
    Given a running pa-falcon-rotator process
    When I connect the rotator
    And I read MechanicalPosition
    Then MechanicalPosition should be 0.00 degrees
    When I call Sync with 12.345
    Then the record config.sync-offset.json beside the config holds sync_offset_deg 12.35

  Scenario: The sync offset survives a restart of the service
    Given a running pa-falcon-rotator process
    When I connect the rotator
    And I read MechanicalPosition
    Then MechanicalPosition should be 0.00 degrees
    When I call Sync with 37.50
    And the process stops and starts again on the same config
    And I connect the rotator
    And I read Position
    Then Position should be 37.50 degrees

  Scenario: A Sync whose record cannot be written fails and leaves the offset as it was
    Given a pa-falcon-rotator process whose sync-offset record cannot be written
    When I connect the rotator
    And I read MechanicalPosition
    Then MechanicalPosition should be 0.00 degrees
    When I call Sync with 37.50
    Then Sync should fail with code 1035
    When I read Position
    Then Position should be 0.00 degrees

  Scenario Outline: Sync rejects non-finite angles with INVALID_VALUE
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I call Sync with <value>
    Then Sync should fail with code 1025

    Examples:
      | value     |
      | NaN       |
      | Infinity  |
      | -Infinity |
