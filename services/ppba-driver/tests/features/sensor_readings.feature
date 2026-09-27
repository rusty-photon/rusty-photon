Feature: Sensor Readings
  As an ASCOM client
  I want to read sensor values from the PPBA
  So that I can monitor environmental conditions and power stats

  Read-only switches 6-15 carry the PPBA's power statistics and sensor data.
  Most come off the wire already in their published units. The current in
  the PA reply does not: the device sends a raw sense count from 0 to 1024,
  which the driver divides by 65 so that switch 11 publishes Amps.

  Against the mock the wire frame is fixed, with a current count of 130, so
  the current scenario asserts an exact value. A missing or wrong divisor
  has to fail there. A reading outside a switch's own published range fails
  the range scenario, whichever switch it lands on.

  Scenario: Voltage is in valid range
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 10 value should be approximately 12.5

  Scenario: Total current is scaled from the raw sense count by 65
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 11 value should be 2.0

  Scenario: Every switch reports a value inside the range it publishes
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then every switch value should lie between its published minimum and maximum

  Scenario: Temperature is in valid range
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 12 value should be approximately 25.0

  Scenario: Humidity is in valid range
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 13 value should be in range 0.0 to 100.0

  Scenario: Average current from power stats
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 6 value should be non-negative

  Scenario: Amp hours from power stats
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 7 value should be non-negative

  Scenario: Watt hours from power stats
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 8 value should be non-negative

  Scenario: Uptime from power stats
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 9 value should be non-negative

  Scenario: DewA PWM precision is 128
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 2 value should be 128.0

  Scenario: DewB PWM precision is 64
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 3 value should be 64.0

  Scenario: Boolean switch values are 0.0 or 1.0
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 0 value should be 0.0 or 1.0

  Scenario: PWM switch values are in 0-255 range
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 2 value should be in range 0.0 to 255.0

  Scenario: Voltage sensor is positive
    Given a running PPBA server with the switch connected
    When I wait for the switch data to be available
    Then switch 10 value should be positive
