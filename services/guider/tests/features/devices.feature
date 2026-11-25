Feature: What the guider knows of its devices
  `GET /api/v1/devices` answers from the device poll (docs/services/guider.md
  § Devices and connections, § `GET /api/v1/devices`). Every
  `devices.poll_interval` the poll reads each server's device list, keeping
  the `UniqueID` listed for the device stripped of the whitespace around it,
  and the device's `Connected`. On the first read that finds a device
  connected, and again whenever the device comes back, it reads the
  device's static properties. A device that is not configured is `null`, a
  property that has not been read yet is `null`, and `binning` is the
  binning of the measured frame. `rotator` is always `null`.

  The simulated devices of each scenario are a 320 × 240 camera with 2 µm
  pixels and a mount, each on a loopback server of its own, and the guider
  polls them every 100 ms.

  Scenario: Connected devices are reported with their static properties
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    Then the guider reports the camera connected
    And the guider reports the mount connected
    And the devices answer reports for the camera:
      | field              | value                    |
      | reachable          | true                     |
      | connected          | true                     |
      | unique_id          | "guide-simulator-camera" |
      | sensor_type        | "monochrome"             |
      | pixel_size_um      | 2.0                      |
      | width              | 320                      |
      | height             | 240                      |
      | binning            | 1                        |
      | can_abort_exposure | true                     |
    And the devices answer reports for the mount:
      | field           | value                   |
      | reachable       | true                    |
      | connected       | true                    |
      | unique_id       | "guide-simulator-mount" |
      | can_pulse_guide | true                    |
    And the devices answer reports the rotator as null

  Scenario Outline: The binning reported is the binning of the measured frame
    Given the simulated camera and mount, both connected
    And the simulated camera has a <sensor> sensor
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/camera/binning" is the JSON <configured>
    When the guider is started
    Then the guider reports the camera connected
    And the devices answer reports for the camera:
      | field       | value       |
      | sensor_type | "<name>"    |
      | binning     | <effective> |

    Examples:
      | sensor     | configured | name       | effective |
      | monochrome | 1          | monochrome | 1         |
      | monochrome | 2          | monochrome | 2         |
      | RGGB       | 1          | rggb       | 2         |

  Scenario: The capabilities each device reports are the ones it has
    Given the simulated camera and mount, both connected
    And the simulated camera cannot abort an exposure
    And the simulated mount cannot pulse guide
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    Then the guider reports the camera connected
    And the guider reports the mount connected
    And the devices answer reports for the camera:
      | field              | value |
      | can_abort_exposure | false |
    And the devices answer reports for the mount:
      | field           | value |
      | can_pulse_guide | false |

  Scenario: A device that is not connected is reported with its UniqueID and no static property
    Given the simulated camera and mount, neither connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    Then the guider reports the camera reachable and not connected
    And the devices answer reports for the camera:
      | field              | value                    |
      | reachable          | true                     |
      | connected          | false                    |
      | unique_id          | "guide-simulator-camera" |
      | sensor_type        | null                     |
      | pixel_size_um      | null                     |
      | width              | null                     |
      | height             | null                     |
      | binning            | null                     |
      | can_abort_exposure | null                     |

  Scenario: The static properties are read once a device is found connected
    Given the simulated camera and mount, neither connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    And the guider reports the camera reachable and not connected
    And the simulated camera is connected
    Then the guider reports the camera connected
    And the devices answer reports for the camera:
      | field       | value        |
      | sensor_type | "monochrome" |
      | width       | 320          |
      | height      | 240          |

  Scenario: The UniqueID is reported stripped of the whitespace around it
    Given the simulated camera and mount, both connected
    And the simulated camera is listed with the UniqueID "  padded-guide-camera  "
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    Then the guider reports the camera connected
    And the devices answer reports for the camera:
      | field     | value                |
      | unique_id | "padded-guide-camera" |

  Scenario: A device whose server stops answering keeps what was read of it
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    And the guider reports the camera connected
    And the camera's server stops answering
    Then the guider reports the camera not reachable
    And the devices answer reports for the camera:
      | field       | value                    |
      | reachable   | false                    |
      | connected   | false                    |
      | unique_id   | "guide-simulator-camera" |
      | sensor_type | "monochrome"             |
      | width       | 320                      |

  Scenario: A device that is not configured is null
    Given the simulated camera and mount, both connected
    And a guider configuration naming the camera only and a focal length of 206.265 mm
    When the guider is started
    Then the guider reports the camera connected
    And the devices answer reports the mount as null

  Scenario: A device number its server does not list is reported unreachable with no UniqueID
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    And the guider configuration's "/devices/camera/device_number" is the JSON 1
    When the guider is started
    Then the guider reports the mount connected
    And the devices answer reports for the camera:
      | field       | value |
      | reachable   | false |
      | connected   | false |
      | unique_id   | null  |
      | sensor_type | null  |
