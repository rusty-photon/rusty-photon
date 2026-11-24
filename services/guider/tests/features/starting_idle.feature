Feature: Starting idle
  The guider is idle after every start (docs/services/guider.md § Starting
  idle). It takes no frame and sends no pulse because it started. Finding
  a device starts nothing, and the guider never writes `Connected` on
  either device: `rp` owns the connections, and a device that is not
  connected is reported, not connected. A service whose configuration
  names no camera, no mount and no focal length runs, and says what is
  missing.

  Scenario: The default configuration starts and says what is missing
    Given a guider configuration naming no devices and no focal length
    When the guider is started
    Then the guider is serving
    And /health answers 503 with the message "the configuration names no camera; the configuration names no mount; the configuration names no focal length"
    And the devices answer reports the camera as null
    And the devices answer reports the mount as null
    And the devices answer reports the rotator as null

  Scenario: A start with both devices connected takes no frame and sends no pulse
    Given the simulated camera and mount, both connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    Then the guider reports the camera connected
    And the guider reports the mount connected
    And the simulated camera has started no exposure
    And the simulated mount has received no pulse

  Scenario: The guider never connects a device it finds disconnected
    Given the simulated camera and mount, neither connected
    And a guider configuration naming both devices and a focal length of 206.265 mm
    When the guider is started
    And the guider reports the camera reachable and not connected
    And the guider reports the mount reachable and not connected
    And the mount's server stops answering
    And the guider reports the mount not reachable
    And the mount's server answers again
    And the guider reports the mount reachable and not connected
    Then the guider reports the camera reachable and not connected
    And the simulated camera is not connected
    And the simulated mount is not connected
