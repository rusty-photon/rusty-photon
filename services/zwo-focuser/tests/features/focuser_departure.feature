@serial
Feature: An EAF that leaves the bus
  An EAF whose cable or power is cut while it is connected keeps its SDK
  handle, and Connected is the driver's own record of that handle (C5). The
  EAF SDK says when the EAF has gone: from the departure on, every call that
  needs the session answers EAF_ERROR_REMOVED. A failure that says so marks
  the session lost; any other failure asks once more on the same session,
  and stands when the EAF is still there. A lost session answers Connected
  as false, and every member that needs a session answers NOT_CONNECTED,
  MaxStep and MaxIncrement included although they are served from cache. The
  call whose failure found the departure answers NOT_CONNECTED too.

  Only a failure asks. Until a call reaches the SDK, a departed EAF still
  reads connected. Disconnecting a departed EAF succeeds. Reconnecting it
  fails while it is gone and connects afresh, finding the EAF by its serial,
  once it is back; the lost session is never taken back. Against the
  simulation backend the EAF leaves the bus while a departure file the suite
  controls exists, and returns when it is removed.

  Background:
    Given the zwo-focuser service running with a simulated focuser that can leave the bus
    And focuser device 0 is connected

  Scenario: A departed focuser still reads connected until a call reaches it
    When focuser device 0 leaves the bus
    Then focuser device 0 reports Connected as true
    And focuser device 0 reports MaxStep as 60000

  Scenario: The first call that reaches a departed focuser reports the disconnect
    When focuser device 0 leaves the bus
    And I query position on focuser device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And focuser device 0 reports Connected as false

  Scenario: Once the departure is known every member that needs a session refuses
    When focuser device 0 leaves the bus
    And I query position on focuser device 0
    Then reading these members from focuser device 0 is rejected with ASCOM NOT_CONNECTED:
      | member       |
      | Position     |
      | IsMoving     |
      | Temperature  |
      | MaxStep      |
      | MaxIncrement |
    And moving focuser device 0 to position 100 is rejected with ASCOM NOT_CONNECTED
    And halting focuser device 0 is rejected with ASCOM NOT_CONNECTED

  Scenario: A failure on a focuser still on the bus leaves it connected
    When I move focuser device 0 to position 1000
    And I try to move focuser device 0 to position 2000
    Then the call is rejected with ASCOM INVALID_OPERATION
    And focuser device 0 reports Connected as true

  Scenario: A departed focuser disconnects cleanly
    When focuser device 0 leaves the bus
    And I query position on focuser device 0
    And I disconnect focuser device 0
    Then focuser device 0 reports Connected as false

  Scenario: A departed focuser reconnects only once it is back
    When focuser device 0 leaves the bus
    And I query position on focuser device 0
    And I try to connect focuser device 0
    Then the call is rejected with ASCOM NOT_CONNECTED
    And focuser device 0 reports Connected as false
    When focuser device 0 returns to the bus
    And I connect focuser device 0
    Then focuser device 0 reports Connected as true
    And focuser device 0 reports a Position between 0 and 60000

  Scenario: Reconnecting a departed focuser that is back opens a fresh session
    When focuser device 0 leaves the bus
    And I query position on focuser device 0
    And focuser device 0 returns to the bus
    And I connect focuser device 0
    Then focuser device 0 reports Connected as true
    And focuser device 0 reports a Position between 0 and 60000
