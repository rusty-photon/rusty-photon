@serial
Feature: Device claims -- the usb_devices list
  An optional `usb_devices` list pins each USB port to an Alpaca device
  number. With a list, qhy-camera takes the host's USB scan -- passive,
  nothing opened -- enumerates the SDK before any open (each camera's id and
  model), places each SDK camera on a port by a one-to-one join on the product
  id, and registers exactly the list, in device-number order (U1, U2, U4).
  Nothing is opened at startup or on a reload: a QHY camera's identity is its
  SDK id, read without an open, and a listed camera's filter wheel is declared
  rather than probed. A camera on a listed port is served at its entry's
  number, with the entry's name and the UniqueID it always had (U5); a camera
  on an unlisted port is neither opened nor registered.

  A listed number whose camera cannot be served is a placeholder (U4). It
  reads Connected as false, refuses Connected = true with ASCOM error 0x540
  naming the reason, carries the same reason in its Description, answers to
  the UniqueID placeholder:qhy-camera:<usb_port>, refuses every camera member
  with NOT_CONNECTED, and still serves the config actions. Its reason is the
  first that applies: the scan failed; a record on the port that is not a
  working device; a camera the join cannot tell apart from another (U3); a
  working record no SDK camera is placed on; or no working camera on the
  port. A failed scan is never a startup failure: the service re-scans in the
  background after waits of 10 s, 20 s and 40 s and every 60 s after that, and
  rebuilds itself on the first scan that succeeds (U6). An empty list
  registers nothing. A list that breaks a rule -- numbers not running 0..N-1,
  a port listed twice, a blank or padded port, wheel numbers not running
  0..M-1, filter names with no wheel, a devices override beside it -- refuses
  the start, and config.apply answers it with field errors and persists
  nothing (U9).

  An entry's `filter_wheel_number` declares the CFW on that camera and pins it
  to a FilterWheel number of its own, so the wheels keep their numbers when a
  camera before them is a placeholder; an entry without one registers no
  wheel, whatever is plugged in (U10). A declared wheel whose camera is a
  placeholder is a placeholder FilterWheel with the same reason and the
  UniqueID placeholder:qhy-camera:CFW-<usb_port>.

  `qhy-camera doctor --devices` lists every QHY camera the join placed on a
  port, in port order, with its model, its SDK id and the number the running
  configuration serves it at, and prints the usb_devices block to paste: with
  no list, every placed camera numbered in port order, carrying its devices
  override's names and declaring the wheel of a camera whose override names
  its filters; with a list, the list itself (U8). It opens nothing, so it
  cannot see a filter wheel, and says so. `qhy-camera doctor` judges a list
  from its own scan (U12): usb-devices.resolve is ok when every listed port
  resolves, warns for each number a placeholder would hold, and fails if the
  scan failed; usb-devices.unlisted names each placed camera no entry lists.
  --devices closes its listing with usb-devices.resolve, the one verdict
  the listing does not show. Without a list, see
  usb_devices_no_list.feature.

  The simulation build never scans the host (U7). By default its scan holds
  one working record for the simulated camera, 1618:f178 on port
  simulated-usbv3-0:1; the hidden --usb-inventory flag replaces it with a
  staged document. The simulated camera's SDK id is SIM-QHY178M and its model
  QHY178M-Simulated; it has a 7-slot CFW, and publishes no USB serial. Since
  the simulation fabricates one camera, the look-alikes here are one camera
  matching two identical records; two SDK cameras of one model sharing records
  are pinned in the join's unit tests.

  Scenario: A camera on a listed port is served at its number with the entry's name
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name         |
      | 0             | simulated-usbv3-0:1 | Main Imaging |
    When the qhy-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "SIM-QHY178M"
    And camera device 0 reports the Name "Main Imaging"
    When I connect camera device 0
    Then camera device 0 reports Connected as true

  Scenario: A listed camera is not opened before a client connects it
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    And the simulated camera has left the bus
    When the qhy-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "SIM-QHY178M"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM NOT_CONNECTED

  Scenario: A listed port with no camera is a placeholder, and the other numbers keep their cameras
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
      | 1             | simulated-usbv3-0:1 |
    When the qhy-camera service starts
    Then the server registers 2 Camera devices
    And camera device 0 reports the UniqueID "placeholder:qhy-camera:simulated-usbv3-0:9"
    And camera device 0 reports the Name "QHY camera on simulated-usbv3-0:9 (placeholder)"
    And camera device 0 reports Connected as false
    And camera device 1 reports the UniqueID "SIM-QHY178M"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "no working camera is enumerated on simulated-usbv3-0:9"
    And the rejection says "reload or restart qhy-camera"
    And camera device 0 reports a Description containing "no working camera is enumerated on simulated-usbv3-0:9"
    And camera device 0 reports Connected as false
    When I connect camera device 1
    Then camera device 1 reports Connected as true

  Scenario: A placeholder serves the config actions and refuses every camera member
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
    When the qhy-camera service starts
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
    When the qhy-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "placeholder:qhy-camera:simulated-usbv3-0:2"

  Scenario: A listed port whose camera the SDK does not report names what the bus shows there
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the qhy-camera service starts with an empty simulation backend
    And I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "simulated-usbv3-0:1 holds QHY178M-Simulated (1618:f178), which the QHY SDK does not report as a camera"

  Scenario: A listed port holding a camera whose firmware is not loaded names the loader's record
    Given the staged USB inventory:
      """
      {
        "usb": [
          { "vendor": "1618", "product": "c178", "model": "Q178", "port": "simulated-usbv2-0:3" },
          { "vendor": "1618", "product": "f178", "model": "QHY178M-Simulated", "port": "simulated-usbv3-0:1" }
        ]
      }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv2-0:3 |
      | 1             | simulated-usbv3-0:1 |
    When the qhy-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:qhy-camera:simulated-usbv2-0:3"
    And camera device 1 reports the UniqueID "SIM-QHY178M"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "simulated-usbv2-0:3 holds Q178 (1618:c178), which the QHY SDK does not report as a camera"

  Scenario: A listed port holding a record that is not a working device gives that record's reason
    Given the staged USB inventory:
      """
      {
        "usb": [],
        "usb_faults": [
          {
            "record": "2-1",
            "vendor": "1618",
            "location": "simulated-usbv3-0:1",
            "reason": "its idProduct could not be read: No such device (os error 19)"
          }
        ]
      }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the qhy-camera service starts
    And I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "its idProduct could not be read: No such device (os error 19)"

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
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
      | 1             | simulated-usbv3-0:2 |
    When the qhy-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:qhy-camera:simulated-usbv3-0:1"
    And camera device 1 reports the UniqueID "placeholder:qhy-camera:simulated-usbv3-0:2"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "cannot be told apart"

  Scenario: A listed camera's declared wheel is served at its number with the entry's filter names
    Given the configuration JSON {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:1", "filter_wheel_number": 0, "filter_names": ["L", "R", "G", "B", "Ha", "OIII", "SII"]}]}
    When the qhy-camera service starts
    Then the server registers 1 FilterWheel device
    And filterwheel device 0 reports the UniqueID "CFW-SIM-QHY178M"
    When I connect filterwheel device 0
    Then filterwheel device 0 reports the filter names L, R, G, B, Ha, OIII, SII

  Scenario: A listed camera with no declared wheel registers none, though a CFW is plugged in
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the qhy-camera service starts
    Then the server registers 1 Camera device
    And the server registers 0 FilterWheel devices

  Scenario: Declared wheels keep their numbers when a camera before them is a placeholder
    Given the configuration JSON {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:9", "filter_wheel_number": 0}, {"device_number": 1, "usb_port": "simulated-usbv3-0:1", "filter_wheel_number": 1}]}
    When the qhy-camera service starts
    Then the server registers 2 FilterWheel devices
    And filterwheel device 0 reports the UniqueID "placeholder:qhy-camera:CFW-simulated-usbv3-0:9"
    And filterwheel device 0 reports the Name "QHY filter wheel on simulated-usbv3-0:9 (placeholder)"
    And filterwheel device 0 reports Connected as false
    And filterwheel device 1 reports the UniqueID "CFW-SIM-QHY178M"
    When I try to connect filterwheel device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "no working camera is enumerated on simulated-usbv3-0:9"
    And filterwheel device 0 reports a Description containing "no working camera is enumerated on simulated-usbv3-0:9"
    When I connect filterwheel device 1
    Then filterwheel device 1 reports Connected as true

  Scenario: Wheels are numbered by their filter_wheel_number, not by their cameras' order
    Given the configuration JSON {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:9", "filter_wheel_number": 1}, {"device_number": 1, "usb_port": "simulated-usbv3-0:1", "filter_wheel_number": 0}]}
    When the qhy-camera service starts
    Then filterwheel device 0 reports the UniqueID "CFW-SIM-QHY178M"
    And filterwheel device 1 reports the UniqueID "placeholder:qhy-camera:CFW-simulated-usbv3-0:9"

  Scenario: A failed USB scan holds every listed number with a placeholder naming the error
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:1", "filter_wheel_number": 0}, {"device_number": 1, "usb_port": "simulated-usbv3-0:2"}]}
    When the qhy-camera service starts
    Then the server registers 2 Camera devices
    And camera device 0 reports the UniqueID "placeholder:qhy-camera:simulated-usbv3-0:1"
    And filterwheel device 0 reports the UniqueID "placeholder:qhy-camera:CFW-simulated-usbv3-0:1"
    When I try to connect camera device 1
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "the USB scan failed: powershell.exe did not answer within 10 s"
    When I try to connect filterwheel device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "the USB scan failed: powershell.exe did not answer within 10 s"

  Scenario: The service rebuilds itself once a failed USB scan succeeds
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the qhy-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:qhy-camera:simulated-usbv3-0:1"
    When the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "1618", "product": "f178", "port": "simulated-usbv3-0:1" } ] }
      """
    Then within 30 seconds camera device 0 reports the UniqueID "SIM-QHY178M"

  Scenario: An empty list registers no camera
    Given the configuration JSON {"usb_devices": []}
    When the qhy-camera service starts
    Then no ASCOM camera devices are registered
    And the service is healthy

  Scenario Outline: A list that breaks a rule refuses the start, and doctor names what is wrong
    Given the configuration JSON <config>
    When the qhy-camera service is started
    Then the service refuses to start saying <message>
    When the doctor subcommand runs on the configuration
    Then the doctor's config.full-shape check fails saying <message>

    Examples:
      | config                                                                                                                                                       | message                                                              |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 2, "usb_port": "b"}]}                                                              | device numbers must run 0..N-1, and 1 is missing                     |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 0, "usb_port": "b"}]}                                                              | usb_devices[1]: device_number 0 is also usb_devices[0]'s             |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 1, "usb_port": "a"}]}                                                              | usb_devices[1]: usb_port a is also usb_devices[0]'s                  |
      | {"usb_devices": [{"device_number": 0, "usb_port": ""}]}                                                                                                      | usb_devices[0]: usb_port is blank                                    |
      | {"usb_devices": [{"device_number": 0, "usb_port": " a"}]}                                                                                                    | usb_devices[0]: usb_port has leading or trailing whitespace          |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a", "filter_wheel_number": 1}]}                                                                           | filter wheel numbers must run 0..M-1, and 0 is missing               |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a", "filter_wheel_number": 0}, {"device_number": 1, "usb_port": "b", "filter_wheel_number": 0}]}          | usb_devices[1]: filter_wheel_number 0 is also usb_devices[0]'s       |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a", "filter_names": ["L"]}]}                                                                              | usb_devices[0]: filter_names needs a filter_wheel_number             |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a", "filter_wheel_number": 0, "filter_names": ["L", ""]}]}                                                | usb_devices[0]: filter_names[1] is empty                             |
      | {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:1"}], "devices": {"SIM-QHY178M": {"name": "Main"}}}                                     | devices.SIM-QHY178M: move its fields into the usb_devices entry      |

  Scenario: config.apply persists a valid list and applies it through a reload
    Given a running qhy-camera service with the simulation backend
    When config.apply sets usb_devices to [{"device_number": 0, "usb_port": "simulated-usbv3-0:1", "filter_wheel_number": 0}]
    Then the apply status should be applying
    And the reload list should include usb_devices

  Scenario: config.apply refuses an empty list
    Given a running qhy-camera service with the simulation backend
    When config.apply sets usb_devices to []
    Then the apply status should be invalid
    And the apply errors name usb_devices saying "an empty list registers no camera"

  Scenario: config.apply names the entry that breaks a rule and persists nothing
    Given a running qhy-camera service with the simulation backend
    When config.apply sets usb_devices to [{"device_number": 0, "usb_port": "a", "filter_wheel_number": 0}, {"device_number": 1, "usb_port": "b", "filter_wheel_number": 0}]
    Then the apply status should be invalid
    And the apply errors name usb_devices.1.filter_wheel_number saying "filter_wheel_number 0 is also usb_devices[0]'s"
    When config.get is called
    Then the config has no usb_devices list

  Scenario: doctor --devices lists the camera by port and prints the list to paste
    Given the configuration JSON {}
    When doctor --devices runs
    Then the doctor exits with code 0
    And the devices listing shows these cameras:
      | Port                | Model             | SDK id      | USB serial | Device |
      | simulated-usbv3-0:1 | QHY178M-Simulated | SIM-QHY178M | —          | 0      |
    And the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:1" } ]
      """
    And the doctor output says "--devices opens no camera, so it cannot see a filter wheel"

  Scenario: doctor --devices carries a devices override's names, and declares the wheel whose filters it names
    Given the configuration JSON {"devices": {"SIM-QHY178M": {"name": "Main Imaging", "filter_names": ["L", "R", "G", "B", "Ha", "OIII", "SII"]}}}
    When doctor --devices runs
    Then the doctor exits with code 0
    And the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:1", "name": "Main Imaging", "filter_wheel_number": 0, "filter_names": ["L", "R", "G", "B", "Ha", "OIII", "SII"] } ]
      """

  Scenario: doctor --devices reproduces a configured list and marks a camera it leaves out
    Given the configuration JSON {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:9", "name": "Guide port", "filter_wheel_number": 0}]}
    When doctor --devices runs
    Then the doctor exits with code 0
    And the devices listing shows these cameras:
      | Port                | Model             | SDK id      | USB serial | Device     |
      | simulated-usbv3-0:1 | QHY178M-Simulated | SIM-QHY178M | —          | not listed |
    And the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:9", "name": "Guide port", "filter_wheel_number": 0 } ]
      """

  Scenario: doctor --devices names a camera matching two identical records, and places it nowhere
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
    When doctor --devices runs
    Then the doctor exits with code 0
    And the devices listing shows no camera
    And the doctor output says "cannot be told apart"

  Scenario: doctor --devices still lists the ports when the configured list breaks a rule
    Given the configuration JSON {"usb_devices": [{"device_number": 1, "usb_port": "simulated-usbv3-0:1"}]}
    When doctor --devices runs
    Then the doctor exits with code 1
    And the doctor output says "device numbers must run 0..N-1, and 0 is missing"
    And the devices listing shows these cameras:
      | Port                | Model             | SDK id      | USB serial | Device |
      | simulated-usbv3-0:1 | QHY178M-Simulated | SIM-QHY178M | —          | 1      |
    And the doctor output prints no usb_devices block

  Scenario: doctor --devices fails when the USB scan failed
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {}
    When doctor --devices runs
    Then the doctor exits with code 1
    And the doctor output says "the USB scan failed: powershell.exe did not answer within 10 s"

  Scenario: doctor finds every listed port resolving to a camera
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the doctor subcommand runs on the configuration
    Then the doctor exits with code 0
    And the doctor's usb-devices.resolve check is ok saying "1 listed port resolves, no placeholders"
    And the doctor reports no usb-devices.unlisted check

  Scenario: doctor warns of a listed number a placeholder would hold, and names the camera left out
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
    When the doctor subcommand runs on the configuration
    Then the doctor exits with code 0
    And the doctor's usb-devices.resolve check warns saying "device 0 (simulated-usbv3-0:9) would be a placeholder: no working camera is enumerated on simulated-usbv3-0:9"
    And the doctor's usb-devices.unlisted check is ok saying "simulated-usbv3-0:1 (QHY178M-Simulated) is on the bus and not listed"

  Scenario: doctor fails usb-devices.resolve when the USB scan failed
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the doctor subcommand runs on the configuration
    Then the doctor exits with code 1
    And the doctor's usb-devices.resolve check fails saying "the USB scan failed: powershell.exe did not answer within 10 s"

  Scenario: doctor --devices closes the listing with usb-devices.resolve
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
    When doctor --devices runs
    Then the doctor exits with code 0
    And the doctor output says "warn usb-devices.resolve: device 0 (simulated-usbv3-0:9) would be a placeholder"
