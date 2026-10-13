@serial
Feature: Device claims -- without a usb_devices list
  With no `usb_devices` key, zwo-camera serves every camera the USB scan can
  place, numbered in port order (U10). It takes the same passive scan and
  one-to-one join a list does, and only then opens the cameras the join
  placed, each briefly and uninitialised, for its serial. A camera the join
  cannot place -- one matching records it cannot pair one-to-one, a model the
  ASI SDK does not know that nothing pairs, a known model with no working
  record -- is refused: never opened, never registered, and logged. So is a
  placed camera that cannot be opened for its serial, without failing the
  start. A failed scan registers no camera and opens none; the service
  re-scans in the background (after 10 s, 20 s, 40 s and every 60 s from
  there) and rebuilds itself on the first scan that succeeds (U6).

  `zwo-camera doctor` reports what the no-list default would serve under
  usb-devices.implicit (U11): the numbers port order gives when more than one
  camera is placed, nothing for one, a warning for each camera it refuses, and
  a failure naming the collector's error when the scan failed. It opens
  nothing, so it cannot see a camera whose open would fail.

  The simulation fabricates one camera, ASI2600MM-Pro-Simulated, serial
  0a1b2c3d4e5f6071. By default its scan holds one working record for it,
  03c3:2600 on port simulated-usbv3-0:1; the hidden --usb-inventory flag
  replaces it with a staged document, and the hidden --simulation-unopenable
  flag makes every open of the camera fail with the SDK's general error (U7).
  Port order across several cameras, and that a camera the join cannot place
  is never opened, are pinned in unit tests, since the simulation fabricates
  one camera and opening it leaves no trace a scenario can read.

  Scenario: The camera the scan places is served at number 0
    Given the configuration JSON {}
    When the zwo-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"

  Scenario: A camera the scan finds no record for is not registered
    Given the staged USB inventory:
      """
      { "usb": [] }
      """
    And the configuration JSON {}
    When the zwo-camera service starts
    Then no ASCOM camera devices are registered
    And the service is healthy

  Scenario: A camera matching two identical records is refused, never guessed
    Given the staged USB inventory:
      """
      {
        "usb": [
          { "vendor": "03c3", "product": "2600", "model": "ASI2600MM-Pro-Simulated", "port": "simulated-usbv3-0:1" },
          { "vendor": "03c3", "product": "2600", "model": "ASI2600MM-Pro-Simulated", "port": "simulated-usbv3-0:2" }
        ]
      }
      """
    And the configuration JSON {}
    When the zwo-camera service starts
    Then no ASCOM camera devices are registered

  Scenario: A placed camera that cannot be opened is not registered, and the service starts
    Given the configuration JSON {}
    When the zwo-camera service starts with a simulated camera that cannot be opened
    Then no ASCOM camera devices are registered
    And the service is healthy

  Scenario: A failed USB scan registers no camera until a re-scan succeeds
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {}
    When the zwo-camera service starts
    Then no ASCOM camera devices are registered
    When the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:1" } ] }
      """
    Then within 30 seconds camera device 0 reports the UniqueID "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"

  Scenario: doctor says nothing about the one camera the no-list default serves
    Given the configuration JSON {}
    When the doctor subcommand runs on the configuration
    Then the doctor exits with code 0
    And the doctor reports no usb-devices.implicit check

  Scenario: doctor warns of a camera the no-list default refuses
    Given the staged USB inventory:
      """
      {
        "usb": [
          { "vendor": "03c3", "product": "2600", "model": "ASI2600MM-Pro-Simulated", "port": "simulated-usbv3-0:1" },
          { "vendor": "03c3", "product": "2600", "model": "ASI2600MM-Pro-Simulated", "port": "simulated-usbv3-0:2" }
        ]
      }
      """
    And the configuration JSON {}
    When the doctor subcommand runs on the configuration
    Then the doctor exits with code 0
    And the doctor's usb-devices.implicit check warns saying "cannot be told apart"

  Scenario: doctor fails usb-devices.implicit when the USB scan failed
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {}
    When the doctor subcommand runs on the configuration
    Then the doctor exits with code 1
    And the doctor's usb-devices.implicit check fails saying "the USB scan failed: powershell.exe did not answer within 10 s"
