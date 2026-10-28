Feature: Switch metadata
  The Advanced 3 exposes 26 switches on one ASCOM Switch device, ordered the
  way the box's panel is. Id 0 is DC1, the always-on 12 V output, read-only
  and always 1. Ids 1-4 are the switched outputs DC2-DC5 and 5-10 the USB
  ports A-F. Each dew channel is three switches with fixed ranges: ids 11
  and 12 the manual duty (0-100), 13 and 14 the enabled flag, 15 and 16 the
  mode (0 Auto, 1 Manual, 2 Switch). Ids 17-25 are read-only telemetry:
  input voltage, output current, output power, the AHT20's temperature,
  humidity and dewpoint, the DS18B20's lens temperature, and the two
  sensors' attached flags.

  Which dew switches are writable follows the channel's mode: the duty in
  Manual only, the enabled flag in Auto and Switch only. The mode switch is
  always writable.

  Switch names are the one part of the table an operator owns.
  `switch.labels` in the config replaces the built-in name of a connector —
  DC1 to DC5, USB A to F, DEW6, DEW7 — and a dew label follows its
  connector into all three of its rows. Descriptions never change, so a
  labelled switch is still identifiable as the connector it is.

  Scenario: Device static name comes from config
    Given a running Gemini hub server with switch name "Test Hub"
    Then the switch device static name should be "Test Hub"

  Scenario: Device unique ID comes from config
    Given a running Gemini hub server with switch unique ID "custom-id-123"
    Then the switch device unique ID should be "custom-id-123"

  Scenario: Device description comes from config
    Given a running Gemini hub server with switch description "Custom description"
    When I connect the switch device
    Then the switch device description should be "Custom description"

  Scenario: Driver info names the device and the firmware the hub reported
    Given a running Gemini hub server with the switch connected
    Then the switch device driver info should contain "Advanced 3"
    And the switch device driver info should contain "firmware 3.0.8"

  Scenario: Driver version is reported
    Given a running Gemini hub server with the switch connected
    Then the switch device driver version should not be empty

  Scenario: Max switch is 26
    Given a running Gemini hub server
    Then the switch device max switch should be 26

  Scenario: Every switch has a name, a description, a usable range and a positive step
    Given a running Gemini hub server with the switch connected
    Then all 26 switches should have non-empty names
    And all 26 switches should have non-empty descriptions
    And all switches should have min less than max and positive step

  Scenario Outline: Switch names match the published table
    Given a running Gemini hub server with the switch connected
    Then switch <id> name should be "<name>"

    Examples:
      | id | name                    |
      | 0  | DC1                     |
      | 1  | DC2                     |
      | 4  | DC5                     |
      | 5  | USB A                   |
      | 10 | USB F                   |
      | 11 | DEW6 Duty               |
      | 12 | DEW7 Duty               |
      | 13 | DEW6 Enabled            |
      | 14 | DEW7 Enabled            |
      | 15 | DEW6 Mode               |
      | 16 | DEW7 Mode               |
      | 17 | Input Voltage           |
      | 18 | Output Current          |
      | 19 | Output Power            |
      | 20 | Temperature             |
      | 21 | Humidity                |
      | 22 | Dewpoint                |
      | 23 | Lens Temperature        |
      | 24 | Ambient Sensor Attached |
      | 25 | Lens Probe Attached     |

  Scenario Outline: Switch ranges and steps match the published table
    Given a running Gemini hub server with the switch connected
    Then switch <id> min value should be <min>
    And switch <id> max value should be <max>
    And switch <id> step should be <step>

    Examples: connectors
      | id | min | max   | step |
      | 0  | 0.0 | 1.0   | 1.0  |
      | 1  | 0.0 | 1.0   | 1.0  |
      | 10 | 0.0 | 1.0   | 1.0  |
      | 11 | 0.0 | 100.0 | 1.0  |
      | 13 | 0.0 | 1.0   | 1.0  |
      | 15 | 0.0 | 2.0   | 1.0  |

    Examples: telemetry, with the sensor ceilings of the AHT20 (85) and the DS18B20 (125)
      | id | min    | max   | step |
      | 17 | 0.0    | 19.0  | 0.1  |
      | 18 | 0.0    | 15.0  | 0.01 |
      | 19 | 0.0    | 180.0 | 0.01 |
      | 20 | -127.0 | 85.0  | 0.01 |
      | 21 | 0.0    | 100.0 | 0.01 |
      | 22 | -127.0 | 85.0  | 0.01 |
      | 23 | -127.0 | 125.0 | 0.01 |
      | 24 | 0.0    | 1.0   | 1.0  |
      | 25 | 0.0    | 1.0   | 1.0  |

  Scenario: DC1 is read-only and the switched connectors are writable
    Given a running Gemini hub server with the switch connected
    Then switch 0 should not be writable
    And switches 1 through 10 should be writable
    And switches 15 through 16 should be writable
    And switches 17 through 25 should not be writable

  Scenario: In Manual mode the duty is writable and the enabled flag is not
    Given a running Gemini hub server with the switch connected
    Then switches 11 through 12 should be writable
    And switches 13 through 14 should not be writable

  Scenario: In Auto mode the enabled flag is writable and the duty is not
    Given a running Gemini hub server with both dew channels in Auto and the switch connected
    Then switches 13 through 14 should be writable
    And switches 11 through 12 should not be writable

  Scenario: In Switch mode the enabled flag is writable and the duty is not
    Given a running Gemini hub server with both dew channels in Switch and the switch connected
    Then switches 13 through 14 should be writable
    And switches 11 through 12 should not be writable

  Scenario: The gate is per channel
    Given a running Gemini hub server with DEW6 in Auto and DEW7 in Manual and the switch connected
    Then switch 13 should be writable
    And switch 11 should not be writable
    And switch 14 should not be writable
    And switch 12 should be writable

  Scenario: Switch 25 is the last valid id and 26 is out of range
    Given a running Gemini hub server with the switch connected
    Then switch 25 name should be queryable
    And querying switch 26 name should fail

  Scenario Outline: An operator label renames a connector, and a dew label its three rows
    Given a running Gemini hub server with the switch connected and these operator labels
      | switch | label              |
      | DC2    | Mount              |
      | DEW6   | Guide scope heater |
      | USB A  | Imaging camera     |
      | DC1    | Hub fan            |
    Then switch <id> name should be "<name>"

    Examples: the labelled connectors
      | id | name           |
      | 0  | Hub fan        |
      | 1  | Mount          |
      | 5  | Imaging camera |

    Examples: the labelled dew channel's three rows
      | id | name                       |
      | 11 | Guide scope heater Duty    |
      | 13 | Guide scope heater Enabled |
      | 15 | Guide scope heater Mode    |

    Examples: switches left unlabelled keep the published name
      | id | name        |
      | 2  | DC3         |
      | 12 | DEW7 Duty   |
      | 20 | Temperature |

  Scenario: A label leaves the description naming the physical connector
    Given a running Gemini hub server with the switch connected and these operator labels
      | switch | label |
      | DC2    | Mount |
    Then switch 1 name should be "Mount"
    And switch 1 description should be "Switches the 12 V output DC2"

  Scenario: A label on a telemetry row fails the load naming the key
    Given a Gemini hub server config with the switch labels {"Humidity": "Sky"}
    When I try to start the configured Gemini hub server
    Then the server refuses to start
    And the refusal names "Humidity"

  Scenario: A label on a dew row rather than its connector fails the load naming the key
    Given a Gemini hub server config with the switch labels {"DEW6 Duty": "Heater"}
    When I try to start the configured Gemini hub server
    Then the server refuses to start
    And the refusal names "DEW6 Duty"

  Scenario: Renaming a switch is not implemented
    Given a running Gemini hub server
    When I try to set switch 1 name to "New Name"
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Device identity is always populated
    Given a running Gemini hub server
    Then the switch device static name should not be empty
    And the switch device unique ID should not be empty
