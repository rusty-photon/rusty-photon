Feature: Sensor and telemetry readings
  Read-only switches 14-38 carry the UPBv2's sensors and telemetry. The
  environmental readings and the power counters come straight off the wire;
  the per-channel currents do not — the device reports raw sense counts, and
  the driver divides by 480 for the four outputs and dew channels A and B,
  and by 700 for dew channel C, which runs through a different MOSFET.

  Against the mock the wire frame is fixed, so the scenarios below assert
  exact values rather than ranges: a scaling regression has to fail here.

  Scenario Outline: Environmental sensors are reported in their published units
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch <id> value should be <value>

    Examples:
      | id | value | reading        |
      | 14 | 12.5  | input voltage  |
      | 15 | 2.4   | total current  |
      | 16 | 30.0  | power draw     |
      | 17 | 25.0  | temperature    |
      | 18 | 60.0  | humidity       |
      | 19 | 16.5  | dewpoint       |

  Scenario Outline: Per-output currents are scaled from raw sense counts by 480
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch <id> value should be approximately <amps>

    Examples:
      | id | amps | raw |
      | 20 | 1.0  | 480 |
      | 21 | 2.0  | 960 |
      | 22 | 0.0  | 0   |
      | 23 | 0.5  | 240 |

  Scenario Outline: Dew channel A and B currents are scaled by 480
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch <id> value should be approximately <amps>

    Examples:
      | id | amps | raw |
      | 24 | 0.5  | 240 |
      | 25 | 0.2  | 96  |

  Scenario: Dew channel C current is scaled by 700, not 480
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch 26 value should be approximately 0.5

  Scenario Outline: Overcurrent flags read clear on a healthy device
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch <id> value should be 0.0

    Examples:
      | id | channel      |
      | 27 | 12V output 1 |
      | 30 | 12V output 4 |
      | 31 | dew heater A |
      | 33 | dew heater C |

  Scenario: A tripped output reports its own overcurrent flag
    Given a running UPBv2 server reporting overcurrent on 12V output 2
    When I wait for the switch data to be available
    Then switch 28 value should be 1.0
    And switch 27 value should be 0.0
    And switch 29 value should be 0.0

  Scenario Outline: Power counters are reported
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch <id> value should be <value>

    Examples:
      | id | value | counter         |
      | 35 | 1.85  | average current |
      | 36 | 0.42  | amp hours       |
      | 37 | 5.1   | watt hours      |
      | 38 | 1.0   | uptime in hours |

  Scenario: Uptime is converted from wire milliseconds to hours
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch 38 value should be approximately 1.0

  Scenario: Dew duty cycles are reported as raw PWM
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch 4 value should be 128.0
    And switch 5 value should be 64.0
    And switch 6 value should be 0.0

  Scenario: The variable output setpoint is read from the PS reply
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch 7 value should be 12.0

  Scenario: Output states are reported as booleans
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch 0 value should be 1.0
    And switch 2 value should be 0.0

  Scenario: USB port states are reported without shadow state
    Given a running UPBv2 server with the switch connected
    When I wait for the switch data to be available
    Then switch 8 value should be 1.0
    And switch 12 value should be 0.0
