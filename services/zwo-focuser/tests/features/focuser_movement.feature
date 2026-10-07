@serial
Feature: Focuser movement
  Absolute positioning, halting, position/is-moving reporting, and live
  temperature for the ZWO EAF (M1-M14). Move is absolute-only (Absolute
  is always true) — there is no relative-move prior art in this codebase
  to diverge from. MaxStep and MaxIncrement report the firmware's working
  travel limit (EAFGetMaxStep, 60000 on the simulated focuser), NOT the
  EAF_INFO::MaxStep ceiling (600000): the firmware stops at the working
  limit even when a move targets beyond it, so Move validates against it.
  While a move is in progress IsMoving stays true across several polls
  and Position ramps live toward the target (the simulated focuser
  advances 640 steps per IsMoving poll — one second of a real EAF's
  ~640 steps-per-second travel); Halt freezes the position mid-travel. TempComp/
  TempCompAvailable/SetTempComp stay stubbed, matching qhy-focuser/
  pa-scops-oag; Temperature returns the live EAFGetTemp reading, unlike
  pa-scops-oag which has no sensor. Every member read from the device,
  MaxStep and MaxIncrement included, needs a connection (M14); the members
  the driver fixes (Absolute, TempComp, TempCompAvailable) answer without
  one.

  Background:
    Given the zwo-focuser service running with the simulation backend

  Scenario: Absolute is always true
    Then focuser device 0 reports Absolute as true

  Scenario: MaxStep and MaxIncrement report the working travel limit
    Given focuser device 0 is connected
    Then focuser device 0 reports MaxStep as 60000
    And focuser device 0 reports MaxIncrement as 60000

  Scenario: MaxStep and MaxIncrement while disconnected are rejected
    Then reading these members from focuser device 0 is rejected with ASCOM NOT_CONNECTED:
      | member       |
      | MaxStep      |
      | MaxIncrement |
      | Temperature  |
      | IsMoving     |

  Scenario: Moving to a position within range starts the move
    Given focuser device 0 is connected
    When I move focuser device 0 to position 3000
    Then focuser device 0 reports IsMoving as true

  Scenario: IsMoving settles to false once the move completes
    Given focuser device 0 is connected
    When I move focuser device 0 to position 3000
    Then focuser device 0 reports IsMoving as true
    And focuser device 0 eventually reports IsMoving as false

  Scenario: Position reports live progress while a move is in progress
    Given focuser device 0 is connected
    When I move focuser device 0 to position 3000
    Then focuser device 0 reports IsMoving as true
    And focuser device 0 reports a Position between 1 and 2999

  Scenario: Position reflects the target once the move completes
    Given focuser device 0 is connected
    When I move focuser device 0 to position 3000
    Then focuser device 0 eventually reports IsMoving as false
    And focuser device 0 reports Position as 3000

  Scenario: Moving past the working travel limit is rejected
    Given focuser device 0 is connected
    When I try to move focuser device 0 to position 60001
    Then the move is rejected with ASCOM INVALID_VALUE

  Scenario: Moving to a negative position is rejected
    Given focuser device 0 is connected
    When I try to move focuser device 0 to position -1
    Then the move is rejected with ASCOM INVALID_VALUE

  Scenario: A second move while already moving is rejected
    Given focuser device 0 is connected
    When I move focuser device 0 to position 1000
    And I try to move focuser device 0 to position 2000
    Then the move is rejected with ASCOM INVALID_OPERATION

  Scenario: Halt freezes the position mid-travel
    Given focuser device 0 is connected
    When I move focuser device 0 to position 5000
    Then focuser device 0 reports IsMoving as true
    When I halt focuser device 0
    Then focuser device 0 reports IsMoving as false
    And focuser device 0 reports a Position between 1 and 4999

  Scenario: Halt on an idle focuser succeeds as a no-op
    Given focuser device 0 is connected
    When I halt focuser device 0
    Then focuser device 0 reports IsMoving as false

  Scenario: Temperature reports the live sensor reading
    Given focuser device 0 is connected
    Then focuser device 0 reports Temperature as 20

  Scenario: StepSize is not implemented
    Given focuser device 0 is connected
    When I query step size on focuser device 0
    Then the move is rejected with ASCOM NOT_IMPLEMENTED

  Scenario: TempComp and TempCompAvailable report false
    Then focuser device 0 reports TempComp as false
    And focuser device 0 reports TempCompAvailable as false

  Scenario: SetTempComp is rejected as not implemented
    When I try to set temp comp to true on focuser device 0
    Then the move is rejected with ASCOM NOT_IMPLEMENTED

  Scenario: Move while disconnected is rejected
    When I try to move focuser device 0 to position 100
    Then the move is rejected with ASCOM NOT_CONNECTED

  Scenario: Position while disconnected is rejected
    When I query position on focuser device 0
    Then the move is rejected with ASCOM NOT_CONNECTED
