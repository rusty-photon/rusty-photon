Feature: Switch error handling
  Every switch operation on a disconnected device reports NOT_CONNECTED, as
  the ASCOM specification requires, and before the id is looked at. An id
  outside 0-25 reports INVALID_VALUE. A write to DC1 or to a telemetry row
  reports NOT_IMPLEMENTED, as does a write to a dew row the channel's mode
  makes read-only, naming the mode switch to change first. A value outside a
  switch's published range reports INVALID_VALUE without anything reaching
  the hub; a value between steps is rounded to the nearest step. A write the hub does not
  carry out — the readback frame does not show it, twice — reports
  INVALID_OPERATION naming the switch, the value asked for and the value
  the hub reports. All switches are synchronous (CanAsync false), so
  SetAsync, SetAsyncValue and StateChangeComplete report NOT_IMPLEMENTED and
  write nothing, while CancelAsync, which ASCOM makes mandatory, succeeds as
  a no-op.

  Scenario Outline: Reads and writes report NOT_CONNECTED while disconnected
    Given a running Gemini hub server
    When I try to <operation>
    Then the last error code should be NOT_CONNECTED

    Examples:
      | operation                                 |
      | get switch 1 value                        |
      | get switch 1 boolean                      |
      | set switch 1 boolean to true              |
      | set switch 11 value to 50.0               |
      | query can_write for switch 1              |
      | query can_async for switch 1              |
      | query state_change_complete for switch 1  |
      | call cancel_async on switch 1             |
      | call set_async on switch 1                |
      | call set_async_value on switch 1          |

  Scenario: An out-of-range id while disconnected reports NOT_CONNECTED first
    Given a running Gemini hub server
    When I try to query can_async for switch 26
    Then the last error code should be NOT_CONNECTED

  Scenario: Switch 26 is out of range for every operation
    Given a running Gemini hub server with the switch connected
    Then all operations on switch 26 should fail

  Scenario: Every out-of-range id is rejected
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    Then operations on invalid switch IDs 26, 27, 100, 999 should all fail

  Scenario: An out-of-range id reports INVALID_VALUE on read and on write
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I try to get switch 99 value
    Then the last error code should be INVALID_VALUE
    When I try to set switch 99 value to 0.0
    Then the last error code should be INVALID_VALUE

  Scenario: Writing DC1 is not implemented
    Given a running Gemini hub server with the switch connected
    When I try to set switch 0 boolean to false
    Then the last error code should be NOT_IMPLEMENTED
    And switch 0 value should be 1.0

  Scenario Outline: Writing a telemetry row is not implemented
    Given a running Gemini hub server with the switch connected
    When I try to set switch <id> value to 0.0
    Then the last error code should be NOT_IMPLEMENTED

    Examples:
      | id | switch                  |
      | 17 | input voltage           |
      | 20 | temperature             |
      | 23 | lens temperature        |
      | 24 | ambient sensor attached |
      | 25 | lens probe attached     |

  Scenario: Writing the duty outside Manual mode is not implemented and names the mode switch
    Given a running Gemini hub server with both dew channels in Auto and the switch connected
    When I try to set switch 12 value to 50.0
    Then the last error code should be NOT_IMPLEMENTED
    And the last error message should contain "DEW7 Mode"

  Scenario: Writing the enabled flag in Manual mode is not implemented and names the mode switch
    Given a running Gemini hub server with the switch connected
    When I try to set switch 14 boolean to false
    Then the last error code should be NOT_IMPLEMENTED
    And the last error message should contain "DEW7 Mode"

  Scenario: Auto is refused while a sensor is unplugged and nothing is sent
    Given a running Gemini hub server with both sensors unplugged and the switch connected
    When I wait for the switch data to be available
    And I try to set switch 15 value to 0.0
    Then the last error code should be INVALID_OPERATION
    And the last error message should contain "DEW6"
    And switch 15 value should be 1.0

  Scenario Outline: A value outside a switch's range is rejected
    Given a running Gemini hub server with the switch connected
    When I try to set switch <id> value to <value>
    Then the last error code should be INVALID_VALUE

    Examples:
      | id | value | switch                  |
      | 1  | 2.0   | DC2, above its range    |
      | 1  | -1.0  | DC2, below its range    |
      | 11 | 101.0 | DEW6 Duty, above 100    |
      | 11 | -1.0  | DEW6 Duty, below 0      |
      | 15 | 3.0   | DEW6 Mode, no such mode |
      | 5  | NaN   | USB A, not a number     |

  Scenario: A rejected value never reaches the hub
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I try to set switch 11 value to 101.0
    Then the last error code should be INVALID_VALUE
    And switch 11 value should be 100.0

  Scenario: A write the hub refuses is reported with both values after a second look
    Given a running Gemini hub server with DC3 tripped and refusing to switch and the switch connected
    When I wait for the switch data to be available
    And I try to set switch 2 boolean to false
    Then the last error code should be INVALID_OPERATION
    And the last error message should contain "DC3"
    And the last error message should contain "asked for 0"
    And the last error message should contain "reports 1"
    And switch 2 value should be 1.0

  Scenario: Asynchronous writes are not offered on any switch
    Given a running Gemini hub server with the switch connected
    Then can_async should return false for all 26 switches
    And state_change_complete should answer NOT_IMPLEMENTED for all 26 switches
    And cancel_async should succeed for all 26 switches

  Scenario: SetAsync and SetAsyncValue are refused and leave the output on
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    And I try to call set_async on switch 2
    Then the last error code should be NOT_IMPLEMENTED
    When I try to call set_async_value on switch 2
    Then the last error code should be NOT_IMPLEMENTED
    And switch 2 value should be 1.0

  Scenario Outline: Asynchronous operations reject an out-of-range id with INVALID_VALUE
    Given a running Gemini hub server with the switch connected
    When I try to <operation>
    Then the last error code should be INVALID_VALUE

    Examples:
      | operation                                 |
      | query can_async for switch 26             |
      | query state_change_complete for switch 26 |
      | call cancel_async on switch 26            |
      | call set_async on switch 26               |
      | call set_async_value on switch 26         |
