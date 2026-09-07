Feature: Switch error handling
  Every switch operation on a disconnected device reports NOT_CONNECTED, as
  the ASCOM specification requires. An id outside 0-38 reports INVALID_VALUE,
  a write to a read-only switch reports NOT_IMPLEMENTED, and a value outside
  a switch's published range reports INVALID_VALUE without anything reaching
  the device.

  Scenario Outline: Reads and writes report NOT_CONNECTED while disconnected
    Given a running UPBv2 server
    When I try to <operation>
    Then the last error code should be NOT_CONNECTED

    Examples:
      | operation                        |
      | get switch 0 value               |
      | get switch 0 boolean             |
      | set switch 0 boolean to true     |
      | set switch 4 value to 128.0      |
      | query can_write for switch 0     |
      | query can_async for switch 0     |
      | query state_change_complete for switch 0 |
      | call cancel_async on switch 0    |
      | call set_async on switch 0       |
      | call set_async_value on switch 0 |

  Scenario: Switch 39 is out of range for every operation
    Given a running UPBv2 server with the switch connected
    Then all operations on switch 39 should fail

  Scenario: Metadata queries reject an out-of-range id
    Given a running UPBv2 server with the switch connected
    Then switch 99 name query should fail
    And switch 99 description query should fail
    And switch 99 min value query should fail
    And switch 99 max value query should fail
    And switch 99 step query should fail

  Scenario: Every out-of-range id is rejected
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then operations on invalid switch IDs 39, 40, 100, 999 should all fail

  Scenario: An out-of-range id reports INVALID_VALUE on read
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    And I try to get switch 99 value
    Then the last error code should be INVALID_VALUE

  Scenario: An out-of-range id reports INVALID_VALUE on write
    Given a running UPBv2 server with the switch connected
    When I try to set switch 99 value to 0.0
    Then the last error code should be INVALID_VALUE

  Scenario Outline: A dew duty outside 0-255 is rejected
    Given a running UPBv2 server with the switch connected
    When I try to set switch 4 value to <duty>
    Then the last error code should be INVALID_VALUE

    Examples:
      | duty   |
      | 300.0  |
      | -1.0   |
      | -10.0  |

  Scenario: A 12V output rejects a value above its boolean range
    Given a running UPBv2 server with the switch connected
    When I try to set switch 0 value to 2.0
    Then the last operation should have failed

  Scenario Outline: Writing a read-only switch reports NOT_IMPLEMENTED
    Given a running UPBv2 server with the switch connected
    When I try to set switch <id> value to 0.0
    Then the last error code should be NOT_IMPLEMENTED

    Examples:
      | id | switch                   |
      | 14 | input voltage            |
      | 20 | 12V output 1 current     |
      | 27 | 12V output 1 overcurrent |
      | 34 | auto-dew channels        |
      | 38 | uptime                   |

  Scenario: Asynchronous writes are not offered on any switch
    Given a running UPBv2 server with the switch connected
    Then can_async should return false for all 39 switches

  Scenario: Every write completes synchronously
    Given a running UPBv2 server with the switch connected
    Then state_change_complete should return true for all 39 switches

  Scenario: Cancelling a write is accepted and does nothing
    Given a running UPBv2 server with the switch connected
    Then cancel_async should succeed for all 39 switches

  Scenario Outline: Asynchronous operations reject an out-of-range id
    Given a running UPBv2 server with the switch connected
    When I try to <operation>
    Then the last operation should have failed

    Examples:
      | operation                                |
      | query state_change_complete for switch 39 |
      | call cancel_async on switch 39           |
      | call set_async on switch 39              |
      | call set_async_value on switch 39        |

  Scenario: can_async rejects an out-of-range id while disconnected
    Given a running UPBv2 server
    When I try to query can_async for switch 39
    Then the last operation should have failed
