Feature: Observing conditions
  The ObservingConditions device serves the AHT20's temperature, humidity
  and dewpoint, each as a sliding-window mean over the frames the driver
  polls. AveragePeriod is the window in hours, 0 to 24; 0 is ASCOM's "not
  averaging" and is served from the newest sample. A window that has
  emptied reads VALUE_NOT_SET, and so do all three readings from the first
  frame whose AHT20 attached flag reads 0: an unplugged sensor is reported
  at once, not after a mean of earlier samples has aged out. The lens
  probe is not an ObservingConditions property; it is switch 23. Every
  other ObservingConditions property the hub lacks reports NOT_IMPLEMENTED.

  Scenario: OC device static name from config
    Given a running Gemini hub server with OC name "Test Weather Station"
    Then the OC device static name should be "Test Weather Station"

  Scenario: OC device unique ID from config
    Given a running Gemini hub server with OC unique ID "custom-weather-001"
    Then the OC device unique ID should be "custom-weather-001"

  Scenario: OC device description contains Environmental
    Given a running Gemini hub server with the OC device connected
    Then the OC device description should contain "Environmental"

  Scenario: OC device driver info names the device and the interface
    Given a running Gemini hub server with the OC device connected
    Then the OC device driver info should contain "Advanced 3"
    And the OC device driver info should contain "ObservingConditions"

  Scenario: OC device driver version is not empty
    Given a running Gemini hub server with the OC device connected
    Then the OC device driver version should not be empty

  Scenario: Default average period reflects configured averaging_period
    Given a running Gemini hub server with the OC device connected
    Then the average period should be approximately 0.0833 hours

  Scenario Outline: The average period is accepted across its range and reads back as set
    Given a running Gemini hub server with the OC device connected
    When I set the average period to <hours> hours
    Then the average period should be <hours> hours

    Examples:
      | hours |
      | 0.0   |
      | 0.5   |
      | 2.0   |
      | 24.0  |

  Scenario Outline: An average period outside 0 to 24 hours, or not a number, is rejected
    Given a running Gemini hub server with the OC device connected
    When I try to set the average period to <hours> hours
    Then the last error code should be INVALID_VALUE

    Examples:
      | hours |
      | -1.0  |
      | 25.0  |
      | NaN   |

  Scenario: Average period transitions from instantaneous to 1 hour and back
    Given a running Gemini hub server with the OC device connected
    When I set the average period to 0.0 hours
    Then the average period should be 0.0 hours
    When I set the average period to 1.0 hours
    Then the average period should be 1.0 hours
    When I set the average period to 0.0 hours
    Then the average period should be 0.0 hours

  Scenario: Temperature is the AHT20's ambient temperature
    Given a running Gemini hub server with the OC device connected
    When I wait for the OC data to be available
    Then the temperature should be approximately 24.47

  Scenario: Humidity is the AHT20's relative humidity
    Given a running Gemini hub server with the OC device connected
    When I wait for the OC data to be available
    Then the humidity should be approximately 67.76

  Scenario: Dewpoint is the hub's computation from the AHT20
    Given a running Gemini hub server with the OC device connected
    When I wait for the OC data to be available
    Then the dewpoint should be approximately 18.12

  Scenario Outline: The three readings report NOT_CONNECTED when disconnected
    Given a running Gemini hub server
    When I try to read the <reading>
    Then the last error code should be NOT_CONNECTED

    Examples:
      | reading     |
      | temperature |
      | humidity    |
      | dewpoint    |

  Scenario Outline: An unplugged ambient sensor reads VALUE_NOT_SET from the first frame
    Given a running Gemini hub server with the AHT20 unplugged and the OC device connected
    When I try to read the <reading>
    Then the last error code should be VALUE_NOT_SET

    Examples:
      | reading     |
      | temperature |
      | humidity    |
      | dewpoint    |

  Scenario: An unplugged lens probe leaves the ambient readings untouched
    Given a running Gemini hub server with the DS18B20 unplugged and the OC device connected
    When I wait for the OC data to be available
    Then the temperature should be approximately 24.47

  Scenario Outline: Sensor descriptions name the AHT20
    Given a running Gemini hub server with the OC device connected
    Then sensor description for "<sensor>" should contain "AHT20"

    Examples:
      | sensor      |
      | temperature |
      | humidity    |
      | dewpoint    |

  Scenario: Sensor description is case insensitive
    Given a running Gemini hub server with the OC device connected
    Then sensor description for "Temperature" and "TEMPERATURE" should match

  Scenario: Sensor description for a sensor the hub lacks returns NOT_IMPLEMENTED
    Given a running Gemini hub server with the OC device connected
    When I try to get sensor description for "pressure"
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Sensor description for empty string returns INVALID_VALUE
    Given a running Gemini hub server with the OC device connected
    When I try to get sensor description for ""
    Then the last error code should be INVALID_VALUE

  Scenario: Sensor description for an unknown sensor returns INVALID_VALUE
    Given a running Gemini hub server with the OC device connected
    When I try to get sensor description for "foobar"
    Then the last error code should be INVALID_VALUE

  Scenario: Sensor description returns NOT_CONNECTED when disconnected
    Given a running Gemini hub server
    When I try to get sensor description for "temperature"
    Then the last error code should be NOT_CONNECTED

  Scenario Outline: Time since last update is fresh for every reading and for the empty name
    Given a running Gemini hub server with the OC device connected
    When I wait for the OC data to be available
    Then time since last update for "<sensor>" should be less than 5.0 seconds

    Examples:
      | sensor      |
      | temperature |
      | humidity    |
      | dewpoint    |
      | Temperature |
      | TEMPERATURE |
      |             |

  Scenario: Time since last update for the empty name reports no data while the ambient sensor is unplugged
    Given a running Gemini hub server with the AHT20 unplugged and the OC device connected
    Then time since last update for "" should be unavailable
    And time since last update for "temperature" should be unavailable

  Scenario: Time since last update for a sensor the hub lacks returns NOT_IMPLEMENTED
    Given a running Gemini hub server with the OC device connected
    When I try to get time since last update for "pressure"
    Then the last error code should be NOT_IMPLEMENTED

  Scenario: Time since last update for an unknown sensor returns INVALID_VALUE
    Given a running Gemini hub server with the OC device connected
    When I try to get time since last update for "foobar"
    Then the last error code should be INVALID_VALUE

  Scenario: Time since last update returns NOT_CONNECTED when disconnected
    Given a running Gemini hub server
    When I try to get time since last update for "temperature"
    Then the last error code should be NOT_CONNECTED

  Scenario: Every sensor the hub lacks reports NOT_IMPLEMENTED on description and time since last update
    Given a running Gemini hub server with the OC device connected
    Then sensor description should return NOT_IMPLEMENTED for all unimplemented sensors
    And time since last update should return NOT_IMPLEMENTED for all unimplemented sensors

  Scenario: Refresh returns NOT_CONNECTED when disconnected
    Given a running Gemini hub server
    When I try to refresh the OC device
    Then the last error code should be NOT_CONNECTED

  Scenario: Refresh succeeds when connected
    Given a running Gemini hub server with the OC device connected
    Then refreshing the OC device should succeed

  Scenario: Background polling refreshes the sensor cache
    # The test config sets polling_interval to 200ms. After a 500ms wait
    # at least two poll cycles must have fired, so the newest sample in
    # the temperature window cannot be older than one polling interval.
    # If the poll loop were dead, time_since_last_update would grow
    # without bound and this assertion would fail.
    Given a running Gemini hub server with the OC device connected
    When I wait for the OC data to be available
    And I wait 500 milliseconds
    Then time since last update for "temperature" should be less than 0.4 seconds

  Scenario Outline: Properties the hub lacks are not implemented
    Given a running Gemini hub server
    When I try to read <property>
    Then the last error code should be NOT_IMPLEMENTED

    Examples:
      | property        |
      | cloud cover     |
      | pressure        |
      | rain rate       |
      | sky brightness  |
      | sky quality     |
      | sky temperature |
      | star FWHM       |
      | wind direction  |
      | wind gust       |
      | wind speed      |

  Scenario: Average period returns NOT_CONNECTED when disconnected
    Given a running Gemini hub server
    When I try to read the average period
    Then the last error code should be NOT_CONNECTED

  Scenario: Set average period returns NOT_CONNECTED when disconnected
    Given a running Gemini hub server
    When I try to set the average period to 1.0 hours
    Then the last error code should be NOT_CONNECTED
