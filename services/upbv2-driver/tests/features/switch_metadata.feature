Feature: Switch metadata
  The UPBv2 exposes 39 switches on one ASCOM Switch device. Ids 0-13 are
  writable and 14-38 are read-only telemetry. The id layout is fixed and
  contiguous: four 12V outputs, three dew channels, the variable-voltage
  output, six USB ports, then the sensor, per-channel current, overcurrent,
  auto-dew and power-counter readings.

  Switch names are the one part of the table an operator owns. `switch.labels`
  in the config replaces the built-in name of any of the writable ids 0-13,
  and a label follows its port: labelling `12V Output 1` renames that port's
  current reading and overcurrent flag too. Descriptions never change, so a
  labelled switch is still identifiable as the connector it is.

  Scenario: Device static name comes from config
    Given a running UPBv2 server with switch name "Test UPBv2"
    Then the switch device static name should be "Test UPBv2"

  Scenario: Device unique ID comes from config
    Given a running UPBv2 server with switch unique ID "custom-id-123"
    Then the switch device unique ID should be "custom-id-123"

  Scenario: Device description comes from config
    Given a running UPBv2 server with switch description "Custom description"
    When I connect the switch device
    Then the switch device description should be "Custom description"

  Scenario: Driver info names the device
    Given a running UPBv2 server with the switch connected
    Then the switch device driver info should contain "UPBv2"

  Scenario: Driver version is reported
    Given a running UPBv2 server with the switch connected
    Then the switch device driver version should not be empty

  Scenario: Max switch is 39
    Given a running UPBv2 server
    Then the switch device max switch should be 39

  Scenario: Every switch has a name
    Given a running UPBv2 server with the switch connected
    Then all 39 switches should have non-empty names

  Scenario: Every switch has a description
    Given a running UPBv2 server with the switch connected
    Then all 39 switches should have non-empty descriptions

  Scenario: Every switch has a usable range
    Given a running UPBv2 server with the switch connected
    Then all switches should have min less than max and positive step

  Scenario: Every switch has a positive step
    Given a running UPBv2 server with the switch connected
    Then all 39 switches should have positive step values

  Scenario Outline: Switch ranges match the published table
    Given a running UPBv2 server with the switch connected
    Then switch <id> min value should be <min>
    And switch <id> max value should be <max>

    Examples: writable switches
      | id | min | max   |
      | 0  | 0.0 | 1.0   |
      | 3  | 0.0 | 1.0   |
      | 4  | 0.0 | 255.0 |
      | 6  | 0.0 | 255.0 |
      | 7  | 3.0 | 12.0  |
      | 8  | 0.0 | 1.0   |
      | 13 | 0.0 | 1.0   |

    Examples: read-only switches
      | id | min   | max     |
      | 14 | 0.0   | 15.0    |
      | 17 | -40.0 | 60.0    |
      | 18 | 0.0   | 100.0   |
      | 34 | 0.0   | 7.0     |
      | 38 | 0.0   | 99999.0 |

  Scenario Outline: Switch names match the published table
    Given a running UPBv2 server with the switch connected
    Then switch <id> name should be "<name>"

    Examples:
      | id | name                     |
      | 0  | 12V Output 1             |
      | 3  | 12V Output 4             |
      | 4  | Dew Heater A             |
      | 6  | Dew Heater C             |
      | 7  | Variable Output Voltage  |
      | 8  | USB Port 1               |
      | 13 | USB Port 6               |
      | 14 | Input Voltage            |
      | 20 | 12V Output 1 Current     |
      | 27 | 12V Output 1 Overcurrent |
      | 34 | Auto-Dew Channels        |
      | 38 | Uptime                   |

  Scenario: Switch 38 is the last valid id
    Given a running UPBv2 server with the switch connected
    Then switch 38 name should be queryable

  Scenario: Switch 39 is out of range
    Given a running UPBv2 server with the switch connected
    Then querying switch 39 name should fail

  Scenario Outline: An operator label renames a port and the telemetry rows that follow it
    Given a running UPBv2 server with the switch connected and these operator labels
      | switch       | label        |
      | 12V Output 1 | QHY600       |
      | Dew Heater C | Secondary    |
      | USB Port 5   | COM3 Focuser |
    Then switch <id> name should be "<name>"

    Examples: the labelled 12V output, its current reading and its overcurrent flag
      | id | name               |
      | 0  | QHY600             |
      | 20 | QHY600 Current     |
      | 27 | QHY600 Overcurrent |

    Examples: the labelled dew channel, its current reading and its overcurrent flag
      | id | name                  |
      | 6  | Secondary             |
      | 26 | Secondary Current     |
      | 33 | Secondary Overcurrent |

    Examples: the labelled USB port, which has no telemetry rows of its own
      | id | name         |
      | 12 | COM3 Focuser |

    Examples: switches left unlabelled keep the published name
      | id | name                 |
      | 1  | 12V Output 2         |
      | 21 | 12V Output 2 Current |
      | 17 | Temperature          |

  Scenario: A label leaves the description naming the physical port
    Given a running UPBv2 server with the switch connected and these operator labels
      | switch       | label  |
      | 12V Output 1 | QHY600 |
    Then switch 0 name should be "QHY600"
    And switch 0 description should be "Switches the 12V output on port 1"

  Scenario: Renaming a switch is not implemented
    Given a running UPBv2 server
    When I try to set switch 0 name to "New Name"
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Device identity is always populated
    Given a running UPBv2 server
    Then the switch device static name should not be empty
    And the switch device unique ID should not be empty
