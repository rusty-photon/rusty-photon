@serial
Feature: Device claims -- the usb_devices list
  An optional `usb_devices` list pins each USB port to an Alpaca device
  number. With a list, zwo-camera takes the host's USB scan -- passive,
  nothing opened -- enumerates the SDK before any open, places each SDK
  camera on a port by a one-to-one join on the product id, and only then
  opens the cameras placed on listed ports, each briefly and uninitialised,
  for its serial. It registers exactly the list, in device-number order (U1,
  U2, U4). A camera on a listed port is served at its entry's number, with
  the entry's name and the UniqueID it always had (U5); a camera on an
  unlisted port is neither opened nor registered. ZWO's focuser and filter
  wheel share the vendor id 03c3; the ASI SDK's own table of camera product
  ids keeps them out of the join (U2).

  A listed number whose camera cannot be served is a placeholder (U4). It
  reads Connected as false, refuses Connected = true with ASCOM error 0x540
  naming the reason, carries the same reason in its Description, answers to
  the UniqueID placeholder:zwo-camera:<usb_port>, refuses every camera
  member with NOT_CONNECTED, and still serves the config actions. Its reason
  is the first that applies: the scan failed; a record on the port that is
  not a working device; a camera the join cannot tell apart from another
  (U3); a working record no SDK camera is placed on; no working camera on the
  port; or the camera placed there could not be opened for its identity
  (U5). Without a list, a camera that cannot be opened still fails the start
  (C0). A failed scan is never a startup failure: the service re-scans in the
  background after waits of 10 s, 20 s and 40 s and every 60 s after that,
  and rebuilds itself on the first scan that succeeds, from the configuration
  it serves (U6). An empty list
  registers nothing. A list that breaks a rule -- numbers not running 0..N-1,
  a port listed twice, a blank or padded port, a devices override beside it
  -- refuses the start, and config.apply answers it with field errors and
  persists nothing (U9).

  `zwo-camera doctor --devices` lists every ASI camera the join placed on a
  port, in port order, with the number the running configuration serves it
  at, and prints the usb_devices block to paste: with no list, every placed
  camera numbered in port order; with a list, the list itself (U8). It opens
  nothing, so it never shows a ZWO SDK id and cannot tell which camera a
  devices override names.

  The simulation build never scans the host (U7). By default its scan holds
  one working record for the simulated camera, 03c3:2600 on port
  simulated-usbv3-0:1; the hidden --usb-inventory flag replaces it with a
  staged document, and the hidden --simulation-unopenable flag makes every
  open of the camera fail with the SDK's general error. The simulated camera
  is ASI2600MM-Pro-Simulated, serial 0a1b2c3d4e5f6071, publishing no USB
  serial. Since the simulation fabricates one camera, the look-alikes here
  are one camera matching two identical records; two SDK cameras of one
  model sharing records are pinned in the join's unit tests.

  Scenario: A camera on a listed port is served at its number with the entry's name
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name         |
      | 0             | simulated-usbv3-0:1 | Main Imaging |
    When the zwo-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"
    And camera device 0 reports the Name "Main Imaging"
    When I connect camera device 0
    Then camera device 0 reports Connected as true

  Scenario: A listed port with no camera is a placeholder, and the other numbers keep their cameras
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
      | 1             | simulated-usbv3-0:1 |
    When the zwo-camera service starts
    Then the server registers 2 Camera devices
    And camera device 0 reports the UniqueID "placeholder:zwo-camera:simulated-usbv3-0:9"
    And camera device 0 reports the Name "ZWO camera on simulated-usbv3-0:9 (placeholder)"
    And camera device 0 reports Connected as false
    And camera device 1 reports the UniqueID "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "no working camera is enumerated on simulated-usbv3-0:9"
    And the rejection says "reload or restart zwo-camera"
    And camera device 0 reports a Description containing "no working camera is enumerated on simulated-usbv3-0:9"
    And camera device 0 reports Connected as false
    When I connect camera device 1
    Then camera device 1 reports Connected as true

  Scenario: A placeholder serves the config actions and refuses every camera member
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
    When the zwo-camera service starts
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
    When the zwo-camera service starts
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "placeholder:zwo-camera:simulated-usbv3-0:2"

  Scenario: A listed port whose camera the SDK does not report names what the bus shows there
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the zwo-camera service starts with an empty simulation backend
    And I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "simulated-usbv3-0:1 holds ASI2600MM-Pro-Simulated (03c3:2600), which the ZWO SDK does not report as a camera"

  Scenario: A listed port holding a ZWO focuser is a placeholder, and the camera elsewhere is not guessed onto it
    Given the staged USB inventory:
      """
      {
        "usb": [
          { "vendor": "03c3", "product": "1f10", "model": "ZWO EAF", "port": "simulated-usbv2-0:3" },
          { "vendor": "03c3", "product": "2600", "model": "ASI2600MM-Pro-Simulated", "port": "simulated-usbv3-0:1" }
        ]
      }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv2-0:3 |
      | 1             | simulated-usbv3-0:1 |
    When the zwo-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:zwo-camera:simulated-usbv2-0:3"
    And camera device 1 reports the UniqueID "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "simulated-usbv2-0:3 holds ZWO EAF (03c3:1f10), which the ZWO SDK does not report as a camera"

  Scenario: A listed port holding a record that is not a working device gives that record's reason
    Given the staged USB inventory:
      """
      {
        "usb": [],
        "usb_faults": [
          {
            "record": "2-1",
            "vendor": "03c3",
            "location": "simulated-usbv3-0:1",
            "reason": "its idProduct could not be read: No such device (os error 19)"
          }
        ]
      }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the zwo-camera service starts
    And I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "its idProduct could not be read: No such device (os error 19)"

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
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
      | 1             | simulated-usbv3-0:2 |
    When the zwo-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:zwo-camera:simulated-usbv3-0:1"
    And camera device 1 reports the UniqueID "placeholder:zwo-camera:simulated-usbv3-0:2"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "cannot be told apart"

  Scenario: A listed camera that cannot be opened is a placeholder naming the SDK's error
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the zwo-camera service starts with a simulated camera that cannot be opened
    Then the server registers 1 Camera device
    And camera device 0 reports the UniqueID "placeholder:zwo-camera:simulated-usbv3-0:1"
    When I try to connect camera device 0
    Then the connect is rejected with ASCOM error 0x540
    And the rejection says "could not open the camera placed on simulated-usbv3-0:1"
    And the rejection says "general error"
    And camera device 0 reports a Description containing "could not open the camera placed on simulated-usbv3-0:1"

  Scenario: Without a list, a camera that cannot be opened still fails the start
    Given the configuration JSON {}
    When the zwo-camera service is started with a simulated camera that cannot be opened
    Then the service refuses to start saying general error

  Scenario: A failed USB scan holds every listed number with a placeholder naming the error
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
      | 1             | simulated-usbv3-0:2 |
    When the zwo-camera service starts
    Then the server registers 2 Camera devices
    And camera device 0 reports the UniqueID "placeholder:zwo-camera:simulated-usbv3-0:1"
    When I try to connect camera device 1
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
    When the zwo-camera service starts
    Then camera device 0 reports the UniqueID "placeholder:zwo-camera:simulated-usbv3-0:1"
    When the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:1" } ] }
      """
    Then within 30 seconds camera device 0 reports the UniqueID "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"

  Scenario: The rebuild after a failed USB scan applies no edit made to the file since the start
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
    When the zwo-camera service starts
    And the configuration file is edited by hand to:
      """
      { "usb_devices": [ { "device_number": 0, "usb_port": "simulated-usbv3-0:1", "name": "Edited" } ] }
      """
    And the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:1" } ] }
      """
    Then within 30 seconds camera device 0 reports the UniqueID "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"
    And camera device 0 reports the Name "Main"

  Scenario: An empty list registers no camera
    Given the configuration JSON {"usb_devices": []}
    When the zwo-camera service starts
    Then no ASCOM camera devices are registered
    And the service is healthy

  Scenario Outline: A list that breaks a rule refuses the start, and doctor names what is wrong
    Given the configuration JSON <config>
    When the zwo-camera service is started
    Then the service refuses to start saying <message>
    When the doctor subcommand runs on the configuration
    Then the doctor's config.full-shape check fails saying <message>

    Examples:
      | config                                                                                                                                       | message                                                              |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 2, "usb_port": "b"}]}                                              | device numbers must run 0..N-1, and 1 is missing                     |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 0, "usb_port": "b"}]}                                              | usb_devices[1]: device_number 0 is also usb_devices[0]'s             |
      | {"usb_devices": [{"device_number": 0, "usb_port": "a"}, {"device_number": 1, "usb_port": "a"}]}                                              | usb_devices[1]: usb_port a is also usb_devices[0]'s                  |
      | {"usb_devices": [{"device_number": 0, "usb_port": ""}]}                                                                                      | usb_devices[0]: usb_port is blank                                    |
      | {"usb_devices": [{"device_number": 0, "usb_port": " a"}]}                                                                                    | usb_devices[0]: usb_port has leading or trailing whitespace          |
      | {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:1"}], "devices": {"0a1b2c3d4e5f6071": {"name": "Main"}}}                | devices.0a1b2c3d4e5f6071: move its fields into the usb_devices entry |

  Scenario: config.apply persists a valid list and applies it through a reload
    Given a running zwo-camera service with the simulation backend
    When config.apply sets usb_devices to [{"device_number": 0, "usb_port": "simulated-usbv3-0:1"}]
    Then the apply status should be applying
    And the reload list should include usb_devices

  Scenario: config.apply refuses an empty list
    Given a running zwo-camera service with the simulation backend
    When config.apply sets usb_devices to []
    Then the apply status should be invalid
    And the apply errors name usb_devices saying "an empty list registers no camera"

  Scenario: config.apply names the entry that breaks a rule and persists nothing
    Given a running zwo-camera service with the simulation backend
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
      | Port                | Model                   | SDK id | USB serial | Device |
      | simulated-usbv3-0:1 | ASI2600MM-Pro-Simulated | —      | —          | 0      |
    And the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:1" } ]
      """

  Scenario: doctor --devices names a devices override it cannot match without an open, and says to delete the map
    Given the configuration JSON {"devices": {"0a1b2c3d4e5f6071": {"name": "Main Imaging"}}}
    When doctor --devices runs
    Then the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:1" } ]
      """
    And the doctor output says "The devices override 0a1b2c3d4e5f6071 cannot be matched to a camera without opening one"
    And the doctor output says "Delete the devices map from zwo-camera.json when you paste this block"

  Scenario: doctor --devices reproduces a configured list and marks a camera it leaves out
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name       |
      | 0             | simulated-usbv3-0:9 | Guide port |
    When doctor --devices runs
    Then the doctor exits with code 0
    And the devices listing shows these cameras:
      | Port                | Model                   | SDK id | USB serial | Device     |
      | simulated-usbv3-0:1 | ASI2600MM-Pro-Simulated | —      | —          | not listed |
    And the paste-ready usb_devices block is:
      """
      [ { "device_number": 0, "usb_port": "simulated-usbv3-0:9", "name": "Guide port" } ]
      """

  Scenario: doctor --devices lists no ZWO focuser as a camera
    Given the staged USB inventory:
      """
      {
        "usb": [
          { "vendor": "03c3", "product": "1f10", "model": "ZWO EAF", "port": "simulated-usbv2-0:3" },
          { "vendor": "03c3", "product": "2600", "model": "ASI2600MM-Pro-Simulated", "port": "simulated-usbv3-0:1" }
        ]
      }
      """
    And the configuration JSON {}
    When doctor --devices runs
    Then the doctor exits with code 0
    And the devices listing shows these cameras:
      | Port                | Model                   | SDK id | USB serial | Device |
      | simulated-usbv3-0:1 | ASI2600MM-Pro-Simulated | —      | —          | 0      |
    And the doctor output does not say "1f10"

  Scenario: doctor --devices names a camera matching two identical records, and places it nowhere
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
      | Port                | Model                   | SDK id | USB serial | Device |
      | simulated-usbv3-0:1 | ASI2600MM-Pro-Simulated | —      | —          | 1      |
    And the doctor output prints no usb_devices block

  Scenario: doctor --devices names a broken list even when the USB scan also failed
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {"usb_devices": [{"device_number": 1, "usb_port": "simulated-usbv3-0:1"}]}
    When doctor --devices runs
    Then the doctor exits with code 1
    And the doctor output says "device numbers must run 0..N-1, and 0 is missing"
    And the doctor output says "the USB scan failed: powershell.exe did not answer within 10 s"

  Scenario: doctor --devices fails when the USB scan failed
    Given the staged USB inventory:
      """
      { "usb_unavailable": "powershell.exe did not answer within 10 s" }
      """
    And the configuration JSON {}
    When doctor --devices runs
    Then the doctor exits with code 1
    And the doctor output says "the USB scan failed: powershell.exe did not answer within 10 s"
