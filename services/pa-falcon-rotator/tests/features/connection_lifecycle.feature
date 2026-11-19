Feature: Connection lifecycle
  The pa-falcon-rotator service exposes a Rotator and a Status Switch device on
  a single ASCOM Alpaca server. The service opens the port and runs the Falcon
  handshake (F# → FV → DR:0 → FA → VS) when it starts, before it serves any
  client, and keeps the port open until it stops. Both devices share the port:
  a connect runs no handshake, and each device connects and disconnects
  independently of the other.

  A disconnect that leaves no client on either device clears the target of
  the last move, so TargetPosition reads the current Position again; one that
  leaves the other device connected keeps it.

  Scenario: Rotator starts disconnected
    Given a running pa-falcon-rotator service
    Then the rotator should be disconnected

  Scenario: Rotator connects successfully after the startup handshake
    Given a running pa-falcon-rotator service
    When I connect the rotator
    Then the rotator should be connected
    And the handshake should have issued F# before any other command

  Scenario: Rotator disconnects after connecting
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I disconnect the rotator
    Then the rotator should be disconnected

  Scenario: Connect is idempotent
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I connect the rotator
    Then the rotator should be connected

  Scenario: The status switch stays connected when the rotator disconnects
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I connect the status switch
    And I disconnect the rotator
    Then the status switch should be connected
    And the handshake should have run exactly once

  Scenario: Both devices read disconnected once each has disconnected
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I connect the status switch
    And I disconnect the rotator
    And I disconnect the status switch
    Then the rotator should be disconnected
    And the status switch should be disconnected

  # Error-model contract: every device-bound op rejects with NOT_CONNECTED
  # (ASCOM code 1031 = 0x407) when called while disconnected. Three
  # representative ops — one rotator read, one rotator write, one switch
  # read — pin the guard without re-listing every property.

  Scenario: Rotator Position read returns NOT_CONNECTED when disconnected
    Given a running pa-falcon-rotator service
    When I read Position
    Then the operation should fail with code 1031

  Scenario: Rotator MoveAbsolute returns NOT_CONNECTED when disconnected
    Given a running pa-falcon-rotator service
    When I call MoveAbsolute with 90.00
    Then the move should fail with code 1031

  Scenario: Status Switch GetSwitchValue returns NOT_CONNECTED when disconnected
    Given a running pa-falcon-rotator service
    When I read GetSwitchValue for id 0
    Then the switch read should fail with code 1031

  Scenario: The last client's disconnect clears the stored target
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I call MoveAbsolute with 90.00
    And the rotator reports mechanical position 120.00 degrees
    And I disconnect the rotator
    And I connect the rotator
    And I read TargetPosition
    Then TargetPosition should be 120.00 degrees

  Scenario: A disconnect that leaves the status switch connected keeps the stored target
    Given a running pa-falcon-rotator service
    When I connect the rotator
    And I connect the status switch
    And I call MoveAbsolute with 90.00
    And the rotator reports mechanical position 120.00 degrees
    And I disconnect the rotator
    And I connect the rotator
    And I read TargetPosition
    Then TargetPosition should be 90.00 degrees
