@serial @wip
Feature: Device claims -- the usb_devices list
  An optional `usb_devices` list pins each USB port to an Alpaca device
  number. With a list, svbony-camera takes the host's USB scan -- passive,
  nothing opened -- enumerates the SDK before any open, places each SDK
  camera on a port by a one-to-one join, and registers exactly the list, in
  device-number order (U1, U2, U4). A camera on a listed port is served at
  its entry's number, with the entry's name and the UniqueID it always had
  (U5); a camera on an unlisted port is not registered at all.

  A listed number whose camera cannot be served is a placeholder (U4). It
  reads Connected as false, refuses Connected = true with ASCOM error 0x540
  naming the reason, carries the same reason in its Description, answers to
  the UniqueID placeholder:svbony-camera:<usb_port>, refuses every camera
  member with NOT_CONNECTED, and still serves the config actions. Its
  reason is the first that applies: the scan failed; a record on the port
  that is not a working device; a camera the join cannot tell apart from
  another (U3); a working record no SDK camera is placed on; otherwise no
  working camera on the port. A failed scan is never a startup failure: the
  service re-scans in the background 10 s, 20 s and 40 s after the start and
  every 60 s after that, and reloads itself on the first scan that succeeds
  (U6). An empty list registers nothing. A list that breaks a rule -- numbers
  not running 0..N-1, a port listed twice, a blank or padded port, a devices
  override beside it -- refuses the start, and config.apply answers it with
  field errors and persists nothing (U9).

  `svbony-camera doctor --devices` lists every SVBony camera the join placed
  on a port, in port order, with the number the running configuration
  serves it at, and prints the usb_devices block to paste: with no list,
  every placed camera numbered in port order with its devices override's
  name; with a list, the list itself (U8).

  The simulation build never scans the host (U7). By default its scan holds
  one working record for the simulated camera, f266:9a0a on port
  simulated-usbv3-0:1; the hidden --usb-inventory flag replaces it with a
  staged document. The simulated camera is SV605CC-Simulated, CameraSN
  SVB0123456789AB, publishing no USB serial.

  Scenario: A camera on a listed port is served at its number with the entry's name
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name         |
      | 0             | simulated-usbv3-0:1 | Main Imaging |
    When the svbony-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "SVBONY:SV605CC-Simulated:SVB0123456789AB"
    And camera device 0 reports the Name "Main Imaging"
    When I connect camera device 0
    Then camera device 0 reports Connected as true

  Scenario: A listed port with no camera is a placeholder, and the other numbers keep their cameras
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
      | 1             | simulated-usbv3-0:1 |
    When the svbony-camera service starts
    Then the server registers 2 Camera devices
    And camera device 0 reports the UniqueID "placeholder:svbony-camera:simulated-usbv3-0:9"
    And camera device 0 reports the Name "SVBony camera on simulated-usbv3-0:9 (placeholder)"
    And camera device 0 reports Connected as false
    And camera device 1 reports the UniqueID "SVBONY:SV605CC-Simulated:SVB0123456789AB"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "no working camera is enumerated on simulated-usbv3-0:9"
    And the rejection says "reload or restart svbony-camera"
    And camera device 0 reports a Description containing "no working camera is enumerated on simulated-usbv3-0:9"
    And camera device 0 reports Connected as false

  Scenario: A placeholder serves the config actions and refuses every camera member
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
    When the svbony-camera service starts
    And the supported actions are queried on camera device 0
    Then the supported actions should include config.get, config.apply, and config.schema
    When config.get is called
    Then the config lists the USB port "simulated-usbv3-0:9" as device 0
    And reading these members from camera device 0 is rejected with ASCOM NOT_CONNECTED:
      | member           |
      | CameraXSize      |
      | CameraState      |
      | BinX             |
      | Gain             |
      | CCDTemperature   |
      | CanAbortExposure |
    When I disconnect camera device 0
    Then camera device 0 reports Connected as false

  Scenario: A camera on an unlisted port is not registered
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:2 |
    When the svbony-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "placeholder:svbony-camera:simulated-usbv3-0:2"

  Scenario: A listed port whose camera the SDK does not report names what the bus shows there
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the svbony-camera service starts with an empty simulation backend
    And I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "simulated-usbv3-0:1 holds SVBONY SV605CC-Simulated (f266:9a0a), which the SVBony SDK does not report as a camera"

  Scenario: A listed port holding a record that is not a working device gives that record's reason
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
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the svbony-camera service starts
    And I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "its idProduct could not be read: No such device (os error 19)"

  Scenario: Two cameras the bus cannot tell apart are refused, never guessed
    Given the staged USB inventory:
      """
      {
        "usb": [
          { "vendor": "f266", "product": "9a0a", "model": "SVBONY SV605CC-Simulated", "port": "simulated-usbv3-0:1" },
          { "vendor": "f266", "product": "9a0a", "model": "SVBONY SV605CC-Simulated", "port": "simulated-usbv3-0:2" }
        ]
      }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
      | 1             | simulated-usbv3-0:2 |
    When the svbony-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:svbony-camera:simulated-usbv3-0:1"
    And camera device 1 reports the UniqueID "placeholder:svbony-camera:simulated-usbv3-0:2"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "cannot be told apart"

  Scenario: A failed USB scan holds every listed number with a placeholder naming the error
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
      | 1             | simulated-usbv3-0:2 |
    When the svbony-camera service starts
    Then the server registers 2 Camera devices
    And camera device 0 reports the UniqueID "placeholder:svbony-camera:simulated-usbv3-0:1"
    When I try to connect camera device 1
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "the USB scan failed: powershell.exe did not answer within 10 s"

  Scenario: The service reloads itself once a failed USB scan succeeds
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the svbony-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:svbony-camera:simulated-usbv3-0:1"
    When the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "f266", "product": "9a0a", "port": "simulated-usbv3-0:1" } ] }
      """
    Then within 30 seconds camera device 0 reports the UniqueID "SVBONY:SV605CC-Simulated:SVB0123456789AB"

  Scenario: An empty list registers no camera
    Given the configuration JSON {"usb_devices": []}
    When the svbony-camera service starts
    Then no ASCOM camera devices are registered
    And the service is healthy

  Scenario Outline: A list that breaks a rule refuses the start, and doctor names what is wrong
    Given the configuration JSON <config>
    When the svbony-camera service is started
    Then the service refuses to start
    When the doctor subcommand runs on the configuration
    Then the doctor's config.full-shape check fails saying <message>

    Examples:
      | config                                                                                                                                      | message                                                    |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 2, "usb_port": "b"}]}                                             | device numbers must run 0..N-1, and 1 is missing           |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 0, "usb_port": "b"}]}                                             | usb_devices[1]: device_number 0 is also usb_devices[0]'s   |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 1, "usb_port": "a"}]}                                             | usb_devices[1]: usb_port a is also usb_devices[0]'s        |
      | {"usb_devices": [{"device_number": 0, "usb_port": ""}]}                                                                                     | usb_devices[0]: usb_port is blank                          |
      | {"usb_devices": [{"device_number": 0, "usb_port": " a"}]}                                                                                   | usb_devices[0]: usb_port has leading or trailing whitespace |
      | {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:1"}], "devices": {"SVB0123456789AB": {"name": "Main"}}}                | devices.SVB0123456789AB: move its fields into the usb_devices entry |

  Scenario: config.apply persists a valid list and applies it through a reload
    Given a running svbony-camera service with the simulation backend
    When config.apply sets usb_devices to [{"device_number": 0, "usb_port": "simulated-usbv3-0:1"}]
    Then the apply status should be applying
    And the reload list should include usb_devices

  Scenario: config.apply refuses an empty list
    Given a running svbony-camera service with the simulation backend
    When config.apply sets usb_devices to []
    Then the apply status should be invalid
    And the apply errors name usb_devices saying "an empty list registers no camera"

  Scenario: config.apply names the entry that breaks a rule and persists nothing
    Given a running svbony-camera service with the simulation backend
    When config.apply sets usb_devices to [{"device_number": 0, "usb_port": "a"}, {"device_number": 0, "usb_port": "b"}]
    Then the apply status should be invalid
    And the apply errors name usb_devices.1.device_number saying "device_number 0 is also usb_devices[0]'s"
    When config.get is called
    Then the config has no usb_devices list

  Scenario: doctor --devices lists the camera by port and prints the list to paste
    Given the configuration JSON {}
    When doctor --devices runs
    Then the doctor exits with code 0
    And the devices listing shows these cameras:
      | Port                | Model             | SDK id          | USB serial | Device |
      | simulated-usbv3-0:1 | SV605CC-Simulated | SVB0123456789AB | —          | 0      |
    And the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:1" } ]
      """

  Scenario: doctor --devices carries a devices override into the block, so pasting keeps the name
    Given the configuration JSON {"devices": {"SVB0123456789AB": {"name": "Main Imaging"}}}
    When doctor --devices runs
    Then the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:1", "name": "Main Imaging" } ]
      """

  Scenario: doctor --devices reproduces a configured list and marks a camera it leaves out
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name       |
      | 0             | simulated-usbv3-0:9 | Guide port |
    When doctor --devices runs
    Then the doctor exits with code 0
    And the devices listing shows these cameras:
      | Port                | Model             | SDK id          | USB serial | Device     |
      | simulated-usbv3-0:1 | SV605CC-Simulated | SVB0123456789AB | —          | not listed |
    And the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:9", "name": "Guide port" } ]
      """

  Scenario: doctor --devices names cameras the bus cannot tell apart, and places neither
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
    When doctor --devices runs
    Then the doctor exits with code 0
    And the devices listing shows no camera
    And the doctor output says "cannot be told apart"

  Scenario: doctor --devices fails when the USB scan failed
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {}
    When doctor --devices runs
    Then the doctor exits with code 1
    And the doctor output says "the USB scan failed: powershell.exe did not answer within 10 s"
