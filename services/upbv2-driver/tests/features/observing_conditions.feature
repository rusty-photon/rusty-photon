Feature: Observing Conditions
  As an ASCOM client
  I want to read environmental data from the UPBv2
  So that I can monitor observing conditions

  Scenario: OC device static name from config
    Given a running UPBv2 server with OC name "Test Weather Station"
    Then the OC device static name should be "Test Weather Station"

  Scenario: OC device unique ID from config
    Given a running UPBv2 server with OC unique ID "custom-weather-001"
    Then the OC device unique ID should be "custom-weather-001"

  Scenario: OC device description contains Environmental
    Given a running UPBv2 server with the OC device connected
    Then the OC device description should contain "Environmental"

  Scenario: OC device driver info contains UPBv2 and ObservingConditions
    Given a running UPBv2 server with the OC device connected
    Then the OC device driver info should contain "UPBv2"
    And the OC device driver info should contain "ObservingConditions"

  Scenario: OC device driver version is not empty
    Given a running UPBv2 server with the OC device connected
    Then the OC device driver version should not be empty

  Scenario: Default average period reflects configured averaging_period
    Given a running UPBv2 server with the OC device connected
    Then the average period should be approximately 0.0833 hours

  Scenario: Set average period to 2 hours
    Given a running UPBv2 server with the OC device connected
    When I set the average period to 2.0 hours
    Then the average period should be 2.0 hours

  Scenario: Set average period minimum 0
    Given a running UPBv2 server with the OC device connected
    When I set the average period to 0.0 hours
    Then the average period should be 0.0 hours

  Scenario: Set average period maximum 24
    Given a running UPBv2 server with the OC device connected
    When I set the average period to 24.0 hours
    Then the average period should be 24.0 hours

  Scenario: Set average period too small rejects
    Given a running UPBv2 server with the OC device connected
    When I try to set the average period to -1.0 hours
    Then the last error code should be INVALID_VALUE

  Scenario: Set average period too large rejects
    Given a running UPBv2 server with the OC device connected
    When I try to set the average period to 25.0 hours
    Then the last error code should be INVALID_VALUE

  Scenario: Set average period fractional 0.5 hours
    Given a running UPBv2 server with the OC device connected
    When I set the average period to 0.5 hours
    Then the average period should be approximately 0.5 hours

  Scenario: Average period transitions from instantaneous to 1 hour and back
    Given a running UPBv2 server with the OC device connected
    When I set the average period to 0.0 hours
    Then the average period should be 0.0 hours
    When I set the average period to 1.0 hours
    Then the average period should be 1.0 hours
    When I set the average period to 0.0 hours
    Then the average period should be 0.0 hours

  Scenario: Temperature reading with data
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    Then the temperature should be approximately 25.0

  Scenario: Temperature returns NOT_CONNECTED when disconnected
    Given a running UPBv2 server
    When I try to read the temperature
    Then the last error code should be NOT_CONNECTED

  Scenario: Humidity reading with data
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    Then the humidity should be approximately 60.0

  Scenario: Humidity returns NOT_CONNECTED when disconnected
    Given a running UPBv2 server
    When I try to read the humidity
    Then the last error code should be NOT_CONNECTED

  Scenario: Dewpoint reading with data
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    Then the dewpoint should be approximately 16.5

  Scenario: Dewpoint returns NOT_CONNECTED when disconnected
    Given a running UPBv2 server
    When I try to read the dewpoint
    Then the last error code should be NOT_CONNECTED

  Scenario: Sensor description for temperature
    Given a running UPBv2 server with the OC device connected
    Then sensor description for "temperature" should contain "temperature"

  Scenario: Sensor description for humidity
    Given a running UPBv2 server with the OC device connected
    Then sensor description for "humidity" should contain "humidity"

  Scenario: Sensor description for dewpoint
    Given a running UPBv2 server with the OC device connected
    Then sensor description for "dewpoint" should contain "Dewpoint"

  Scenario: Sensor description is case insensitive
    Given a running UPBv2 server with the OC device connected
    Then sensor description for "Temperature" and "TEMPERATURE" should match

  Scenario: Sensor description for unknown sensor returns NOT_IMPLEMENTED
    Given a running UPBv2 server with the OC device connected
    When I try to get sensor description for "pressure"
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Sensor description for empty string returns INVALID_VALUE
    Given a running UPBv2 server with the OC device connected
    When I try to get sensor description for ""
    Then the last error code should be INVALID_VALUE

  Scenario: Sensor description for truly unknown sensor returns INVALID_VALUE
    Given a running UPBv2 server with the OC device connected
    When I try to get sensor description for "foobar"
    Then the last error code should be INVALID_VALUE

  Scenario: Sensor description returns NOT_CONNECTED when disconnected
    Given a running UPBv2 server
    When I try to get sensor description for "temperature"
    Then the last error code should be NOT_CONNECTED

  Scenario: Time since last update with data
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    Then time since last update for "temperature" should be less than 5.0 seconds

  Scenario: Time since last update for humidity
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    Then time since last update for "humidity" should be less than 5.0 seconds

  Scenario: Time since last update for dewpoint
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    Then time since last update for "dewpoint" should be less than 5.0 seconds

  Scenario: Time since last update is case insensitive
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    Then time since last update for "Temperature" should be less than 5.0 seconds
    And time since last update for "TEMPERATURE" should be less than 5.0 seconds

  Scenario: Time since last update for empty string returns most recent
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    Then time since last update for "" should be less than 5.0 seconds

  Scenario: Time since last update for unknown sensor returns NOT_IMPLEMENTED
    Given a running UPBv2 server with the OC device connected
    When I try to get time since last update for "pressure"
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Time since last update for truly unknown returns INVALID_VALUE
    Given a running UPBv2 server with the OC device connected
    When I try to get time since last update for "foobar"
    Then the last error code should be INVALID_VALUE

  Scenario: Time since last update for unimplemented sensors returns NOT_IMPLEMENTED
    Given a running UPBv2 server with the OC device connected
    Then time since last update should return NOT_IMPLEMENTED for all unimplemented sensors

  Scenario: Time since last update returns NOT_CONNECTED when disconnected
    Given a running UPBv2 server
    When I try to get time since last update for "temperature"
    Then the last error code should be NOT_CONNECTED

  Scenario: Sensor description for unimplemented sensors returns NOT_IMPLEMENTED
    Given a running UPBv2 server with the OC device connected
    Then sensor description should return NOT_IMPLEMENTED for all unimplemented sensors

  Scenario: Refresh returns NOT_CONNECTED when disconnected
    Given a running UPBv2 server
    When I try to refresh the OC device
    Then the last error code should be NOT_CONNECTED

  Scenario: Refresh succeeds when connected
    Given a running UPBv2 server with the OC device connected
    Then refreshing the OC device should succeed

  Scenario: Background polling refreshes the sensor cache
    # The test config sets polling_interval to 200ms. After a 500ms wait
    # at least two poll cycles must have fired, so the most recent
    # `last_update` cannot be older than one polling interval. If the
    # poll loop were dead, time_since_last_update would grow without
    # bound and this assertion would fail.
    Given a running UPBv2 server with the OC device connected
    When I wait for the OC data to be available
    And I wait 500 milliseconds
    Then time since last update for "temperature" should be less than 0.4 seconds

  Scenario: Cloud cover is not implemented
    Given a running UPBv2 server
    When I try to read cloud cover
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Pressure is not implemented
    Given a running UPBv2 server
    When I try to read pressure
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Rain rate is not implemented
    Given a running UPBv2 server
    When I try to read rain rate
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Sky brightness is not implemented
    Given a running UPBv2 server
    When I try to read sky brightness
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Sky quality is not implemented
    Given a running UPBv2 server
    When I try to read sky quality
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Sky temperature is not implemented
    Given a running UPBv2 server
    When I try to read sky temperature
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Star FWHM is not implemented
    Given a running UPBv2 server
    When I try to read star FWHM
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Wind direction is not implemented
    Given a running UPBv2 server
    When I try to read wind direction
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Wind gust is not implemented
    Given a running UPBv2 server
    When I try to read wind gust
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Wind speed is not implemented
    Given a running UPBv2 server
    When I try to read wind speed
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Average period returns NOT_CONNECTED when disconnected
    Given a running UPBv2 server
    When I try to read the average period
    Then the last error code should be NOT_CONNECTED

  Scenario: Set average period returns NOT_CONNECTED when disconnected
    Given a running UPBv2 server
    When I try to set the average period to 1.0 hours
    Then the last error code should be NOT_CONNECTED
