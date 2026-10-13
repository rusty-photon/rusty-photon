@serial
Feature: Device claims -- without a usb_devices list
  With no `usb_devices` key, svbony-camera serves every camera the USB scan
  can place, numbered in port order (U10). It takes the same passive scan and
  one-to-one join a list does, so a camera is served only when it sits on a
  port the scan can name, and nothing is opened to decide it. A camera the
  join cannot place -- one matching records it cannot pair one-to-one, a model
  the driver does not know that nothing pairs, a known model with no working
  record -- is refused: never registered, and logged. A failed scan registers
  no camera; the service re-scans in the background (after 10 s, 20 s, 40 s
  and every 60 s from there) and rebuilds itself on the first scan that
  succeeds (U6).

  `svbony-camera doctor` reports what the no-list default would serve under
  usb-devices.implicit (U11): the numbers port order gives when more than one
  camera is placed, nothing for one, a warning for each camera it refuses, and
  a failure naming the collector's error when the scan failed.

  The simulation fabricates one camera, SV605CC-Simulated, CameraSN
  SVB0123456789AB. By default its scan holds one working record for it,
  f266:9a0a on port simulated-usbv3-0:1; the hidden --usb-inventory flag
  replaces it with a staged document (U7). Port order across several cameras
  is pinned in the join's unit tests, since the simulation fabricates one.

  Scenario: The camera the scan places is served at number 0
    Given the configuration JSON {}
    When the svbony-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "SVBONY:SV605CC-Simulated:SVB0123456789AB"

  Scenario: A camera the scan finds no record for is not registered
    Given the staged USB inventory:
      """
      { "usb": [] }
      """
    And the configuration JSON {}
    When the svbony-camera service starts
    Then no ASCOM camera devices are registered
    And the service is healthy

  Scenario: A camera whose record is not a working device is not registered
    Given the staged USB inventory:
      """
      {
        "usb": [],
        "usb_faults": [
          {
            "record": "2-1",
            "vendor": "f266",
            "location": "simulated-usbv3-0:1",
            "reason": "its idProduct could not be read: No such device (os error 19)"
          }
        ]
      }
      """
    And the configuration JSON {}
    When the svbony-camera service starts
    Then no ASCOM camera devices are registered

  Scenario: A camera matching two identical records is refused, never guessed
    Given the staged USB inventory:
      """
      {
        "usb": [
          { "vendor": "f266", "product": "9a0a", "model": "SVBONY SV605CC-Simulated", "port": "simulated-usbv3-0:1" },
          { "vendor": "f266", "product": "9a0a", "model": "SVBONY SV605CC-Simulated", "port": "simulated-usbv3-0:2" }
        ]
      }
      """
    And the configuration JSON {}
    When the svbony-camera service starts
    Then no ASCOM camera devices are registered

  Scenario: A failed USB scan registers no camera until a re-scan succeeds
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {}
    When the svbony-camera service starts
    Then no ASCOM camera devices are registered
    When the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "f266", "product": "9a0a", "port": "simulated-usbv3-0:1" } ] }
      """
    Then within 30 seconds camera device 0 reports the UniqueID "SVBONY:SV605CC-Simulated:SVB0123456789AB"

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
          { "vendor": "f266", "product": "9a0a", "model": "SVBONY SV605CC-Simulated", "port": "simulated-usbv3-0:1" },
          { "vendor": "f266", "product": "9a0a", "model": "SVBONY SV605CC-Simulated", "port": "simulated-usbv3-0:2" }
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
