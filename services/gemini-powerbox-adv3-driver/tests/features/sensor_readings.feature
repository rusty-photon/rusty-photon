Feature: Status frame readings
  Every switch value comes from the hub's status frame, published as the
  frame carries it: the parser neither clamps nor range-checks a reading,
  so a value outside a row's declared range is an abnormal condition the
  operator sees rather than a number hidden behind a limit.

  The mock serves the frame captured from the dev box's unit on 2026-10-26:

    *G1111D111111U1A1T1A1M1B1M100C100C 23.25S 24.47T 67.76H 18.12D12.2V 0.02C  0.36P#

  so the scenarios below assert exact values: a field-order or scaling
  regression has to fail here. Rows 20-22 are the AHT20's trio and row 23
  the DS18B20's lens temperature. While a sensor's attached flag (rows 24,
  25) reads 0, its rows read a sentinel instead of a stale number: -127 for
  the temperature rows, 0 for humidity.

  Scenario Outline: Output and port states are read from the frame
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    Then switch <id> value should be <value>

    Examples:
      | id | value | field          |
      | 0  | 1.0   | DC1, always on |
      | 1  | 1.0   | DC2            |
      | 4  | 1.0   | DC5            |
      | 5  | 1.0   | USB A          |
      | 10 | 1.0   | USB F          |

  Scenario Outline: The dew channels' three quantities are read from the frame
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    Then switch <id> value should be <value>

    Examples:
      | id | value | field                    |
      | 11 | 100.0 | DEW6 duty, percent       |
      | 12 | 100.0 | DEW7 duty, percent       |
      | 13 | 1.0   | DEW6 enabled             |
      | 14 | 1.0   | DEW7 enabled             |
      | 15 | 1.0   | DEW6 mode, 1 is Manual   |
      | 16 | 1.0   | DEW7 mode, 1 is Manual   |

  Scenario Outline: Telemetry and sensor readings are reported in their published units
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    Then switch <id> value should be <value>

    Examples:
      | id | value | reading                     |
      | 17 | 12.2  | input voltage, V            |
      | 18 | 0.02  | output current, A           |
      | 19 | 0.36  | output power, W             |
      | 20 | 24.47 | ambient temperature, C      |
      | 21 | 67.76 | humidity, percent           |
      | 22 | 18.12 | dew point, C                |
      | 23 | 23.25 | lens temperature, C         |
      | 24 | 1.0   | AHT20 attached              |
      | 25 | 1.0   | DS18B20 attached            |

  Scenario: Current and power are published as read, not derived from each other
    Given a running Gemini hub server with the switch connected
    When I wait for the switch data to be available
    Then switch 18 value should be 0.02
    And switch 19 value should be 0.36

  Scenario: A reading outside its row's range is published unclamped
    Given a running Gemini hub server with a 24 V supply on the input and the switch connected
    When I wait for the switch data to be available
    Then switch 17 value should be 24.0
    And switch 17 max value should be 19.0

  Scenario: An unplugged ambient sensor reads as its sentinels and a cleared flag
    Given a running Gemini hub server with the AHT20 unplugged and the switch connected
    When I wait for the switch data to be available
    Then switch 24 value should be 0.0
    And switch 20 value should be -127.0
    And switch 21 value should be 0.0
    And switch 22 value should be -127.0
    And switch 23 value should be 23.25
    And switch 25 value should be 1.0

  Scenario: An unplugged lens probe reads as its sentinel and a cleared flag
    Given a running Gemini hub server with the DS18B20 unplugged and the switch connected
    When I wait for the switch data to be available
    Then switch 25 value should be 0.0
    And switch 23 value should be -127.0
    And switch 20 value should be 24.47
    And switch 24 value should be 1.0

  Scenario: The sentinels sit inside the rows' declared ranges
    Given a running Gemini hub server with both sensors unplugged and the switch connected
    When I wait for the switch data to be available
    Then switch 20 value should be -127.0
    And switch 20 min value should be -127.0
    And switch 23 value should be -127.0
    And switch 23 min value should be -127.0
    And switch 21 value should be 0.0
    And switch 21 min value should be 0.0
