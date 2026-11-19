Feature: Serving without the QHYCCD SDK library
  On Windows the QHYCCD SDK is the operator's qhyccd.dll, which QHY's
  All-in-One pack installs and no package ships. A service without it has not
  failed to start: it serves without the SDK, opens and enumerates nothing,
  says why, and keeps serving. With no usb_devices list it registers no
  camera. With a list, every listed number is a placeholder, and every
  declared wheel a placeholder FilterWheel, whose connect is refused with
  ASCOM error 0x540 naming the missing library (PF4). Health and the setup
  pages are served either way.

  The service re-checks in the background after waits of 10 s, 20 s, 40 s,
  60 s and every 60 s after that. Once the library loads, a QHY camera is
  working on the bus, and the SDK initializes, it rebuilds itself from the
  configuration it was serving, and serves its cameras with no restart
  (PF6).

  The simulation build has no qhyccd.dll to miss, so a hidden flag stands one
  in: its library loads only once a file exists at the path the flag names.
  Until it does, the reason is "the simulated QHYCCD SDK library is not
  installed" (PF5).

  Scenario: Without the SDK library the service serves no camera and stays healthy
    Given the QHY SDK library is not installed
    When the qhy-camera service starts
    Then no ASCOM camera devices are registered
    And the service is healthy

  Scenario: Without the SDK library every listed number is a placeholder naming it
    Given the QHY SDK library is not installed
    And the configuration JSON {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:1", "filter_wheel_number": 0}]}
    When the qhy-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "placeholder:qhy-camera:simulated-usbv3-0:1"
    And filterwheel device 0 reports the UniqueID "placeholder:qhy-camera:CFW-simulated-usbv3-0:1"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "the simulated QHYCCD SDK library is not installed"
    When I try to connect filterwheel device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "the simulated QHYCCD SDK library is not installed"

  Scenario: The service serves its camera once the SDK library is installed, with no restart
    Given the QHY SDK library is not installed
    When the qhy-camera service starts
    Then no ASCOM camera devices are registered
    When the QHY SDK library is installed
    Then within 30 seconds camera device 0 reports the UniqueID "SIM-QHY178M"

  Scenario: A listed camera is served once the SDK library is installed
    Given the QHY SDK library is not installed
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the qhy-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:qhy-camera:simulated-usbv3-0:1"
    When the QHY SDK library is installed
    Then within 30 seconds camera device 0 reports the UniqueID "SIM-QHY178M"
