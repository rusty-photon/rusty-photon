Feature: Switch Control
  Switches 0 to 5 are the PPBA's controls; 6 to 15 are read-only readings.
  The two dew heaters, switches 2 and 3, are writable only while auto-dew
  (switch 5) is off. With auto-dew on they report CanWrite false, and both
  SetSwitch and SetSwitchValue refuse them with NOT_IMPLEMENTED, the code
  ASCOM requires from any switch whose CanWrite is false. The driver reads
  auto-dew from the device (PA) before it decides; the refusal comes before
  the value is range-checked, sends no heater command, and tells the
  operator to turn auto-dew off at switch 5.

  Scenario: Get boolean switch value when connected
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 0 value should be 1.0

  Scenario: Get switch boolean when connected
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 0 boolean should be true

  Scenario: Set boolean switch value
    Given a running PPBA server with the switch connected
    Then setting switch 0 boolean to true should succeed

  Scenario: Switches 0 to 5 are writable with auto-dew off
    Given a running PPBA server with the switch connected
    Then switches 0 through 5 should be writable

  Scenario: Switches 6 to 15 are read-only
    Given a running PPBA server with the switch connected
    Then switches 6 through 15 should not be writable

  Scenario: Auto-dew enabled makes dew heaters not writable
    Given a running PPBA server with auto-dew enabled
    Then switch 2 should not be writable
    And switch 3 should not be writable

  Scenario: Auto-dew enabled leaves other switches writable
    Given a running PPBA server with auto-dew enabled
    Then switch 0 should be writable
    And switch 1 should be writable
    And switch 4 should be writable
    And switch 5 should be writable

  Scenario Outline: A dew heater under auto-dew refuses every write as not implemented
    Given a running PPBA server with auto-dew enabled
    When I try to <write>
    Then the last error code should be NOT_IMPLEMENTED

    Examples:
      | write                        |
      | set switch 2 value to 100.0  |
      | set switch 2 boolean to true |
      | set switch 3 value to 100.0  |
      | set switch 3 boolean to true |

  Scenario: A dew heater under auto-dew is refused before its value is range-checked
    Given a running PPBA server with auto-dew enabled
    When I try to set switch 2 value to 300.0
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: The auto-dew refusal names switch 5 as the way out
    Given a running PPBA server with auto-dew enabled
    When I try to set switch 2 value to 100.0
    Then the last error message should contain "Disable auto-dew (switch 5) first"

  Scenario: A refused dew-heater write sends no heater command, so heater A keeps its 128
    Given a running PPBA server with auto-dew enabled
    When I try to set switch 2 value to 200.0
    Then switch 2 value should be 128.0

  Scenario: Setting Dew Heater A PWM succeeds when auto-dew is off
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    And I set switch 2 value to 200.0
    Then switch 2 value should be 200.0

  Scenario: Setting Dew Heater B PWM succeeds when auto-dew is off
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    And I set switch 3 value to 100.0
    Then switch 3 value should be 100.0

  Scenario: USB hub set uses special PU command path
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    And I set switch 4 value to 1.0
    Then switch 4 value should be 1.0

  Scenario: Auto-dew toggle uses PD command and refreshes
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then setting switch 5 value to 1.0 should succeed

  Scenario: Read-only sensor write is rejected
    Given a running PPBA server with the switch connected
    When I try to set switch 10 value to 12.0
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: All switches are queryable when connected
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then all 16 switches should be queryable for name, description, min, max, step, value, and can_write

  Scenario: All writable switches identified correctly
    Given a running PPBA server with the switch connected
    Then switches 0 through 5 should be writable
    And switches 6 through 15 should not be writable
