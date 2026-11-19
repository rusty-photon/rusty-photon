@serial
Feature: Cooling
  Cooling is gated on the SDK Cooler control: CanSetCCDTemperature and
  CanGetCoolerPower are true only when the control is present, and the
  related getters return NOT_IMPLEMENTED otherwise (K1). When cooling is
  supported, CCDTemperature reads the current sensor temperature (K2),
  SetCCDTemperature validates the target against [-273.15, 80] and reads it
  back (K3), and CoolerOn / CoolerPower map to the SDK PWM controls (K4).
  CoolerOn reports what survived a reconnect's init: a cooler still drawing
  power after it reads on, and one the init switched off reads off (K4).
  A reading no sensor can have, below -273.15 or above 100 degrees Celsius
  or not a number, is a failed read and not a temperature: CCDTemperature
  answers INVALID_VALUE, as for a read the SDK refuses, and the camera stays
  connected while it is still on the bus. SetCCDTemperature, which with no
  target set reads back the reading, refuses it the same way (K3). CoolerOn
  with no target set takes the reading as its setpoint only within -100 to
  80 degrees, and engages at 0 degrees for any other reading (K4). The
  simulated QHY178M-Simulated camera has a cooler, and while a temperature
  file the suite controls exists, its sensor reads the number written in it.

  Background:
    Given the qhy-camera service running with a simulated camera whose temperature reading the suite can set
    And camera device 0 is connected

  Scenario: A cooled camera advertises temperature control
    Then camera device 0 reports CanSetCCDTemperature as true
    And camera device 0 reports CanGetCoolerPower as true

  Scenario: The current CCD temperature is readable
    Then camera device 0 reports a finite CCDTemperature

  Scenario: A valid target temperature is accepted and read back
    When I set the target CCD temperature to -10.0 on camera device 0
    Then camera device 0 reports SetCCDTemperature as -10.0

  Scenario Outline: An out-of-range target temperature is rejected
    When I try to set the target CCD temperature to <target> on camera device 0
    Then the set is rejected with ASCOM INVALID_VALUE

    Examples:
      | target  |
      | -300.0  |
      | 100.0   |

  Scenario: Turning the cooler on is reflected in CoolerOn
    When I turn the cooler on for camera device 0
    Then camera device 0 reports CoolerOn as true
    And camera device 0 reports a CoolerPower between 0 and 100

  Scenario: A cooler still regulating through a reconnect is still reported on
    When I set the target CCD temperature to -10.0 on camera device 0
    And I turn the cooler on for camera device 0
    And I wait for the cooler on camera device 0 to draw power
    And I disconnect camera device 0
    And I connect camera device 0
    Then camera device 0 reports CoolerOn as true
    And camera device 0 reports SetCCDTemperature as -10.0

  Scenario Outline: A reading no sensor can have is a failed read, not a temperature
    When the sensor of camera device 0 reads <reading>
    And I try to read CCDTemperature from camera device 0
    Then the call is rejected with ASCOM INVALID_VALUE
    And camera device 0 reports Connected as true

    Examples:
      | reading |
      | -5005.8 |
      | -273.16 |
      | 100.1   |
      | NaN     |

  Scenario Outline: The widest readings a camera can give are temperatures
    When the sensor of camera device 0 reads <reading>
    Then camera device 0 reports CCDTemperature as <reading>

    Examples:
      | reading |
      | -273.15 |
      | 100.0   |

  Scenario: With no target set, a refused reading is not read back as the setpoint
    When the sensor of camera device 0 reads -5005.8
    And I try to read SetCCDTemperature from camera device 0
    Then the call is rejected with ASCOM INVALID_VALUE

  Scenario Outline: With no target set, a reading that cannot be a setpoint engages the cooler at 0 degrees
    When the sensor of camera device 0 reads <reading>
    And I turn the cooler on for camera device 0
    Then camera device 0 reports CoolerOn as true
    And camera device 0 reports SetCCDTemperature as 0.0

    Examples:
      | reading |
      | -5005.8 |
      | -200.0  |
      | 80.1    |

  Scenario: With no target set, a reading on the edge of the setpoint band is the setpoint
    When the sensor of camera device 0 reads -100.0
    And I turn the cooler on for camera device 0
    Then camera device 0 reports CoolerOn as true
    And camera device 0 reports SetCCDTemperature as -100.0
