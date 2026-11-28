Feature: Movement
  Move, MoveAbsolute, and MoveMechanical each translate ASCOM angle semantics
  into a single MD:nn.nn command, store the sky-coordinate target, and return
  immediately. IsMoving is the authoritative completion signal — it reads FA
  on every call and never depends on driver-side bookkeeping.

  Scenario: MoveAbsolute subtracts the sync offset before issuing MD
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 0.00 degrees
    And the driver-side sync offset is -104.80 degrees
    And I call MoveAbsolute with 180.00
    Then MD:284.80 should have been sent

  Scenario: MoveAbsolute with zero sync offset passes the angle through verbatim
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 0.00 degrees
    And the driver-side sync offset is 0.00 degrees
    And I call MoveAbsolute with 45.50
    Then MD:45.50 should have been sent

  Scenario: MoveMechanical does NOT subtract the sync offset from the wire value
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 0.00 degrees
    And the driver-side sync offset is -104.80 degrees
    And I call MoveMechanical with 90.00
    Then MD:90.00 should have been sent

  Scenario: Move adds a relative delta to current mechanical and re-normalises
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 350.00 degrees
    And the driver-side sync offset is -30.00 degrees
    And I call Move with 20.00
    # delta is sky-coords: target_sky = (mech + offset + delta) mod 360 = 340
    # mech wire = (target_sky - offset) mod 360 = (mech + delta) mod 360 = 10
    # offset must be non-zero so a buggy "subtract offset twice" impl is caught.
    Then MD:10.00 should have been sent

  Scenario: IsMoving reads FA on every call
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I read IsMoving
    Then an FA command should have been issued

  Scenario: TargetPosition returns the requested sky angle while a move is outstanding
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 0.00 degrees
    And the driver-side sync offset is 45.00 degrees
    # Non-zero offset so a buggy impl that stored the mechanical target
    # (102.84) instead of the sky target (147.84) is caught.
    And I call MoveAbsolute with 147.84
    And I read TargetPosition
    Then TargetPosition should be 147.84 degrees

  Scenario: MoveAbsolute to negative zero sends MD:0.00
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And the rotator reports mechanical position 90.00 degrees
    And I call MoveAbsolute with -0.00
    Then MD:0.00 should have been sent

  Scenario Outline: MoveAbsolute rejects non-finite angles with INVALID_VALUE
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I call MoveAbsolute with <value>
    Then the move should fail with code 1025

    Examples:
      | value     |
      | NaN       |
      | Infinity  |
      | -Infinity |

  Scenario Outline: Move rejects non-finite deltas with INVALID_VALUE
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I call Move with <value>
    Then the move should fail with code 1025

    Examples:
      | value     |
      | NaN       |
      | Infinity  |
      | -Infinity |

  Scenario Outline: MoveMechanical rejects non-finite angles with INVALID_VALUE
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I call MoveMechanical with <value>
    Then the move should fail with code 1025

    Examples:
      | value     |
      | NaN       |
      | Infinity  |
      | -Infinity |
