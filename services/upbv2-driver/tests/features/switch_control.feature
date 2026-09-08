Feature: Switch control
  Switches 0-13 are writable: four 12V outputs, three dew channels, the
  3-12V variable output and six USB ports. Switches 14-38 are read-only.

  A dew channel the device drives itself is read-only for as long as
  auto-dew controls it. The gate is per channel, not global: with auto-dew
  set to control channel B only, channels A and C stay writable. This driver
  never writes auto-dew — it is set in the Pegasus Astro software and only
  reported here, on read-only switch 34.

  Scenario: Reading a 12V output reports its state
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch 0 boolean should be true

  Scenario: Switching a 12V output off is reflected on the next read
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    And I set switch 0 boolean to false
    Then switch 0 boolean should be false

  Scenario Outline: Every 12V output can be switched independently
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    And I set switch <id> boolean to true
    Then switch <id> boolean should be true

    Examples:
      | id |
      | 0  |
      | 1  |
      | 2  |
      | 3  |

  Scenario Outline: Every USB port can be switched independently
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    And I set switch <id> boolean to false
    Then switch <id> boolean should be false

    Examples:
      | id |
      | 8  |
      | 9  |
      | 10 |
      | 11 |
      | 12 |
      | 13 |

  Scenario Outline: Dew channels accept a PWM duty when auto-dew is off
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    And I set switch <id> value to <duty>
    Then switch <id> value should be <duty>

    Examples:
      | id | duty  |
      | 4  | 200.0 |
      | 5  | 100.0 |
      | 6  | 255.0 |

  Scenario: Switches 0 through 13 are writable with auto-dew off
    Given a running UPBv2 server with the switch connected
    Then switches 0 through 13 should be writable

  Scenario: Switches 14 through 38 are read-only
    Given a running UPBv2 server with the switch connected
    Then switches 14 through 38 should not be writable

  Scenario: Auto-dew on one channel makes only that channel read-only
    Given a running UPBv2 server with auto-dew controlling channel B
    Then switch 5 should not be writable
    And switch 4 should be writable
    And switch 6 should be writable

  Scenario: Auto-dew on all channels makes all three dew channels read-only
    Given a running UPBv2 server with auto-dew controlling all channels
    Then switch 4 should not be writable
    And switch 5 should not be writable
    And switch 6 should not be writable

  Scenario: Auto-dew leaves the outputs, USB ports and variable output writable
    Given a running UPBv2 server with auto-dew controlling all channels
    Then switches 0 through 3 should be writable
    And switch 7 should be writable
    And switches 8 through 13 should be writable

  Scenario: Writing a dew channel under auto-dew control is rejected
    Given a running UPBv2 server with auto-dew controlling channel B
    When I try to set switch 5 value to 100.0
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: The rejection names the Pegasus software as the place to turn auto-dew off
    Given a running UPBv2 server with auto-dew controlling channel B
    When I try to set switch 5 value to 100.0
    Then the last error message should contain "Pegasus"

  Scenario: The variable output accepts a voltage in range
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    And I set switch 7 value to 5.0
    Then switch 7 value should be 5.0

  Scenario Outline: The variable output rejects a voltage outside 3-12
    Given a running UPBv2 server with the switch connected
    When I try to set switch 7 value to <volts>
    Then the last error code should be INVALID_VALUE

    Examples:
      | volts |
      | 0.0   |
      | 2.0   |
      | 13.0  |
      | 24.0  |

  Scenario: Auto-dew state is readable but not writable
    Given a running UPBv2 server with auto-dew controlling all channels
    When I wait for the switch data to be available
    Then switch 34 value should be 1.0
    And switch 34 should not be writable

  Scenario: Writing a read-only sensor switch is rejected
    Given a running UPBv2 server with the switch connected
    When I try to set switch 14 value to 12.0
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Every switch is queryable when connected
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then all 39 switches should be queryable for name, description, min, max, step, value, and can_write
