Feature: Switch control
  Every write is confirmed against a fresh status frame. A set command
  draws no reply of its own, so the driver sends it together with a status
  request, discards whatever the hub streamed before, and compares the one
  field the write addressed with what it asked for. A read straight after a
  write therefore reflects the hub, and a readback that does not show the
  write is asked once more before it is believed.

  The dew channels are three switches each. Which of them a write may touch
  follows the channel's mode: the duty (ids 11, 12) in Manual only, the
  enabled flag (ids 13, 14) in Auto and Switch only, and a write to a row
  the mode makes read-only is NOT_IMPLEMENTED naming the mode switch to
  change first. The mode switch (ids 15, 16) takes 0 Auto, 1 Manual, 2
  Switch; Auto is refused with INVALID_OPERATION, naming the missing sensor,
  while either sensor is unplugged, and nothing reaches the hub.

  Scenario: Reading a switched output reports its state
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    Then switch 1 boolean should be true

  Scenario Outline: Every switched 12 V output can be turned off and back on
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I set switch <id> boolean to false
    Then switch <id> boolean should be false
    When I set switch <id> boolean to true
    Then switch <id> boolean should be true

    Examples:
      | id | output |
      | 1  | DC2    |
      | 2  | DC3    |
      | 3  | DC4    |
      | 4  | DC5    |

  Scenario Outline: Every USB port can be switched independently
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I set switch <id> boolean to false
    Then switch <id> boolean should be false
    And switch 1 boolean should be true

    Examples:
      | id | port  |
      | 5  | USB A |
      | 6  | USB B |
      | 7  | USB C |
      | 8  | USB D |
      | 9  | USB E |
      | 10 | USB F |

  Scenario: Switching one output leaves the others as they were
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I set switch 2 boolean to false
    Then switch 2 value should be 0.0
    And switch 1 value should be 1.0
    And switch 3 value should be 1.0
    And switch 5 value should be 1.0

  Scenario: A write's readback is the post-write frame even with streamed frames queued ahead of it
    Given a running Gemini hub server with two streamed frames queued ahead of every reply and the switch connected
    When I wait for the switch data to be available
    And I set switch 1 boolean to false
    Then switch 1 boolean should be false
    When I set switch 11 value to 40.0
    Then switch 11 value should be 40.0

  Scenario Outline: A value between steps is rounded to the nearest step
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I set switch <id> value to <written>
    Then switch <id> value should be <read>

    Examples: the quarter points ConformU writes to a 0-1 row
      | id | written | read |
      | 1  | 0.25    | 0.0  |
      | 1  | 0.5     | 1.0  |
      | 1  | 0.75    | 1.0  |

    Examples: a duty and a mode between steps
      | id | written | read |
      | 11 | 50.5    | 51.0 |
      | 11 | 0.25    | 0.0  |
      | 15 | 1.5     | 2.0  |

  Scenario Outline: The manual duty takes a whole percentage in Manual mode
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I set switch <id> value to <duty>
    Then switch <id> value should be <duty>

    Examples:
      | id | duty  |
      | 11 | 0.0   |
      | 11 | 35.0  |
      | 12 | 100.0 |

  Scenario: The enabled flag is refused in Manual mode naming the mode switch
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I try to set switch 13 boolean to false
    Then the last error code should be NOT_IMPLEMENTED
    And the last error message should contain "DEW6 Mode"
    And the last error message should contain "Manual"
    And switch 13 value should be 1.0

  Scenario: In Auto mode the enabled flag toggles the loop and the duty is refused
    Given a running Gemini hub server with both dew channels in Auto and the switch connected
    When I wait for the switch data to be available
    And I set switch 13 boolean to false
    Then switch 13 boolean should be false
    When I set switch 13 boolean to true
    Then switch 13 boolean should be true
    When I try to set switch 11 value to 50.0
    Then the last error code should be NOT_IMPLEMENTED
    And the last error message should contain "DEW6 Mode"
    And the last error message should contain "Auto"

  Scenario: In Switch mode the enabled flag is plain DC on or off
    Given a running Gemini hub server with both dew channels in Switch and the switch connected
    When I wait for the switch data to be available
    And I set switch 14 boolean to false
    Then switch 14 boolean should be false
    And switch 13 boolean should be true

  Scenario: The mode cycles through Switch, Auto and Manual and moves the gates with it
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I set switch 15 value to 2.0
    Then switch 15 value should be 2.0
    And switch 13 should be writable
    And switch 11 should not be writable
    When I set switch 15 value to 0.0
    Then switch 15 value should be 0.0
    And switch 13 should be writable
    When I set switch 15 value to 1.0
    Then switch 15 value should be 1.0
    And switch 11 should be writable
    And switch 13 should not be writable
    And switch 16 value should be 1.0

  Scenario: A boolean write to the mode switch picks its endpoints
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I set switch 16 boolean to true
    Then switch 16 value should be 2.0
    When I set switch 16 boolean to false
    Then switch 16 value should be 0.0

  Scenario: Auto is refused while the lens probe is unplugged, naming it
    Given a running Gemini hub server with the DS18B20 unplugged and the switch connected
    When I wait for the switch data to be available
    And I try to set switch 15 value to 0.0
    Then the last error code should be INVALID_OPERATION
    And the last error message should contain "DEW6"
    And the last error message should contain "DS18B20"
    And switch 15 value should be 1.0

  Scenario: Auto is refused while the ambient sensor is unplugged, naming it
    Given a running Gemini hub server with the AHT20 unplugged and the switch connected
    When I wait for the switch data to be available
    And I try to set switch 16 boolean to false
    Then the last error code should be INVALID_OPERATION
    And the last error message should contain "DEW7"
    And the last error message should contain "AHT20"

  Scenario: Manual and Switch stay open to a channel whose sensor is unplugged
    Given a running Gemini hub server with the DS18B20 unplugged and the switch connected
    When I wait for the switch data to be available
    And I set switch 15 value to 2.0
    Then switch 15 value should be 2.0
    When I set switch 15 value to 1.0
    Then switch 15 value should be 1.0

  Scenario: Every switch is queryable when connected
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    Then all 26 switches should be queryable for name, description, min, max, step, value, and can_write
