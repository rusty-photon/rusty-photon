@serial
Feature: Device claims -- without a usb_devices list
  With no `usb_devices` key, qhy-camera serves every camera the USB scan can
  place, numbered in port order, with the filter wheel its probe finds (U11).
  It takes the same passive scan and one-to-one join a list does, and only
  then runs the CFW probe -- open, ask whether a CFW is plugged, close, never
  InitQHYCCD -- on the cameras the join placed. Their wheels are numbered in
  their cameras' port order. A camera the join cannot place -- one matching
  records it cannot pair one-to-one, a model the driver does not know that
  nothing pairs, a known model with no working record -- is refused: never
  opened, never registered, and logged. A failed scan registers no camera and
  opens none; the service re-scans in the background (after 10 s, 20 s, 40 s
  and every 60 s from there) and rebuilds itself on the first scan that
  succeeds (U6).

  `qhy-camera doctor` reports what the no-list default would serve under
  usb-devices.implicit (U12): the numbers port order gives when more than one
  camera is placed, nothing for one, a warning for each camera it refuses, and
  a failure naming the collector's error when the scan failed. It opens
  nothing, so it never probes for a wheel.

  The simulation fabricates one camera, SDK id SIM-QHY178M and model
  QHY178M-Simulated, with a 7-slot CFW. By default its scan holds one working
  record for it, 1618:f178 on port simulated-usbv3-0:1; the hidden
  --usb-inventory flag replaces it with a staged document (U7). Port order
  across several cameras is pinned in the join's unit tests, since the
  simulation fabricates one.

  Scenario: The camera the scan places is served at number 0 with the wheel its probe finds
    Given the configuration JSON {}
    When the qhy-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "SIM-QHY178M"
    And the server registers 1 FilterWheel device
    And filterwheel device 0 reports the UniqueID "CFW-SIM-QHY178M"

  Scenario: A camera the scan finds no record for is neither opened nor registered
    Given the staged USB inventory:
      """
      { "usb": [] }
      """
    And the configuration JSON {}
    When the qhy-camera service starts
    Then no ASCOM camera devices are registered
    And the server registers 0 FilterWheel devices
    And the service is healthy

  Scenario: A camera matching two identical records is refused, never guessed
    Given the staged USB inventory:
      """
      {
        "usb": [
          { "vendor": "1618", "product": "f178", "model": "QHY178M-Simulated", "port": "simulated-usbv3-0:1" },
          { "vendor": "1618", "product": "f178", "model": "QHY178M-Simulated", "port": "simulated-usbv3-0:2" }
        ]
      }
      """
    And the configuration JSON {}
    When the qhy-camera service starts
    Then no ASCOM camera devices are registered
    And the server registers 0 FilterWheel devices

  Scenario: A failed USB scan registers no camera until a re-scan succeeds
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {}
    When the qhy-camera service starts
    Then no ASCOM camera devices are registered
    When the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "1618", "product": "f178", "port": "simulated-usbv3-0:1" } ] }
      """
    Then within 30 seconds camera device 0 reports the UniqueID "SIM-QHY178M"

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
          { "vendor": "1618", "product": "f178", "model": "QHY178M-Simulated", "port": "simulated-usbv3-0:1" },
          { "vendor": "1618", "product": "f178", "model": "QHY178M-Simulated", "port": "simulated-usbv3-0:2" }
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
