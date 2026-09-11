Feature: Hardware checks (no SDK)
  Doctor judges the device surface without opening any device: serial
  nodes and their permissions, USB presence, udev rules and their group
  resolvability, and the QHY firmware install. One severity rule covers
  the family: a problem fails when the service's unit is enabled (it will
  start at boot and hit it), and warns otherwise. Hardware facts arrive
  through the platform-facts seam; a scenario that stages none gets no
  hardware checks at all — a staged file is its scenario's whole truth.

  Scenario: A missing serial device fails an enabled driver
    Given platform facts with an enabled unit "rusty-photon-ppba-driver"
    And a config file "ppba-driver.json" containing:
      """
      { "serial": { "port": "/dev/ttyUSB7" } }
      """
    And hardware facts staged empty
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.serial-node" for service "ppba-driver"
    And that check's detail mentions "/dev/ttyUSB7"
    And that check's suggestion mentions "/serial/port"
    And doctor exits with code 1

  Scenario: A missing serial device only warns when the unit is disabled
    Given platform facts with a disabled unit "rusty-photon-ppba-driver"
    And hardware facts staged empty
    When I run doctor with --json
    Then the report contains a "warn" check named "hardware.serial-node" for service "ppba-driver"
    And doctor exits with code 0

  Scenario: The catalog default path is checked when the config sets none
    Given platform facts with an enabled unit "rusty-photon-ppba-driver"
    And hardware facts staged empty
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.serial-node" for service "ppba-driver"
    And that check's detail mentions "/dev/ttyUSB0"

  Scenario: A present openable device passes both serial checks
    Given platform facts with an enabled unit "rusty-photon-ppba-driver"
    And the unit "rusty-photon-ppba-driver" confers supplementary group "dialout"
    And hardware facts with a character device "/dev/ttyUSB0" owned by uid 0 gid 20 with mode "0660"
    And hardware facts where host group "dialout" has gid 20
    And hardware facts where the rusty-photon user has uid 990 and gid 990
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.serial-node" for service "ppba-driver"
    And the report contains an "ok" check named "hardware.serial-access" for service "ppba-driver"

  Scenario: Account-level membership opens a node the unit's groups do not
    Given platform facts with an enabled unit "rusty-photon-ppba-driver"
    And the unit "rusty-photon-ppba-driver" confers supplementary group "dialout"
    And hardware facts with a character device "/dev/ttyUSB0" owned by uid 0 gid 46 with mode "0660"
    And hardware facts where host group "dialout" has gid 20
    And hardware facts where host group "plugdev" has gid 46
    And hardware facts where the rusty-photon user has uid 990 and gid 990
    And hardware facts where the rusty-photon user belongs to host group "plugdev"
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.serial-node" for service "ppba-driver"
    And the report contains an "ok" check named "hardware.serial-access" for service "ppba-driver"
    And that check's detail mentions "account-level plugdev group membership"

  Scenario: A device the unit's groups cannot open names the missing membership
    Given platform facts with an enabled unit "rusty-photon-ppba-driver"
    And hardware facts with a character device "/dev/ttyUSB0" owned by uid 0 gid 20 with mode "0660"
    And hardware facts where host group "dialout" has gid 20
    And hardware facts where the rusty-photon user has uid 990 and gid 990
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.serial-access" for service "ppba-driver"
    And that check's detail mentions "gid 20 = group dialout"
    And that check's suggestion mentions "SupplementaryGroups=dialout"
    And doctor exits with code 1

  Scenario: A node owned by a held group with a bad mode points at the mode, not membership
    Given platform facts with an enabled unit "rusty-photon-ppba-driver"
    And hardware facts with a character device "/dev/ttyUSB0" owned by uid 0 gid 990 with mode "0600"
    And hardware facts where host group "rusty-photon" has gid 990
    And hardware facts where the rusty-photon user has uid 990 and gid 990
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.serial-access" for service "ppba-driver"
    And that check's suggestion mentions "ownership or mode"

  Scenario: A UDP-transport mount has no serial device to check
    Given a config file "star-adventurer-gti.json" containing:
      """
      { "transport": { "kind": "udp", "address": "192.168.4.1", "bind_address": "0.0.0.0" } }
      """
    And hardware facts staged empty
    When I run doctor with --json
    Then the report has no checks named "hardware.serial-node"
    And the report has no checks named "hardware.usb-device"

  Scenario: A mount on WiFi is never asked for a USB device, even when one is plugged in
    Given a config file "star-adventurer-gti.json" containing:
      """
      { "transport": { "kind": "udp", "address": "192.168.4.1", "bind_address": "0.0.0.0" } }
      """
    And hardware facts with a USB device "0483:5740" reporting product string "STM32 Virtual ComPort"
    When I run doctor with --json
    Then the report has no checks named "hardware.usb-device"

  Scenario: A mount on USB is found by the vendor and product its microcontroller reports
    Given a config file "star-adventurer-gti.json" containing:
      """
      {}
      """
    And hardware facts with a USB device "0483:5740" reporting product string "STM32 Virtual ComPort"
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.usb-device" for service "star-adventurer-gti"

  Scenario: A mount whose cable is out is reported by vendor and product, with no model to name
    Given a config file "star-adventurer-gti.json" containing:
      """
      {}
      """
    And hardware facts with a USB device "1618:c179" with no product string
    When I run doctor with --json
    Then the report contains a "warn" check named "hardware.usb-device" for service "star-adventurer-gti"
    And that check's detail mentions "0483:5740"

  Scenario: The product string discriminates devices behind a shared bridge chip
    Given a config file "ppba-driver.json" containing:
      """
      {}
      """
    And a config file "pa-falcon-rotator.json" containing:
      """
      {}
      """
    And hardware facts with a USB device "0403:6015" reporting product string "Falcon Rotator"
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.usb-device" for service "pa-falcon-rotator"
    And the report contains a "warn" check named "hardware.usb-device" for service "ppba-driver"
    And that check's detail mentions "0403:6015"
    And that check's detail mentions "PPBA"

  Scenario: The powerbox is recognised by what it announces on the bus, not by its handshake reply
    Given a config file "upbv2-driver.json" containing:
      """
      {}
      """
    And a config file "ppba-driver.json" containing:
      """
      {}
      """
    And hardware facts with a USB device "0403:6015" reporting product string "UPBv2 revA"
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.usb-device" for service "upbv2-driver"
    And the report contains a "warn" check named "hardware.usb-device" for service "ppba-driver"

  Scenario: A board whose descriptor names only its microcontroller is matched on vendor and product alone
    Given a config file "qhy-focuser.json" containing:
      """
      {}
      """
    And hardware facts with a USB device "28e9:018a" reporting product string "GD32-CDC_ACM"
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.usb-device" for service "qhy-focuser"

  Scenario: A board that publishes no product string at all still matches, because no model is declared
    Given a config file "qhy-focuser.json" containing:
      """
      {}
      """
    And hardware facts with a USB device "28e9:018a" with no product string
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.usb-device" for service "qhy-focuser"

  Scenario: Another microcontroller on the bus is not the focuser
    Given a config file "qhy-focuser.json" containing:
      """
      {}
      """
    And hardware facts with a USB device "2e8a:000a" reporting product string "Deep Sky Dad FP2"
    When I run doctor with --json
    Then the report contains a "warn" check named "hardware.usb-device" for service "qhy-focuser"
    And that check's detail mentions "28e9:018a"

  Scenario: An unresolvable GROUP fails the udev rule check because udev drops the line
    Given platform facts with an enabled unit "rusty-photon-qhy-camera"
    And the installed udev rule for "qhy-camera" is the packaged rule
    And hardware facts where host group "dialout" has gid 20
    And hardware facts with a USB device "1618:c179" with no product string
    And hardware facts with a directory at "/lib/firmware/qhy"
    And hardware facts with an executable file at "/usr/local/sbin/fxload"
    And hardware facts with a regular file at "/etc/udev/rules.d/85-qhyccd.rules"
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.udev-rule" for service "qhy-camera"
    And that check's detail mentions "drops the entire rule line"
    And that check's suggestion mentions "groupadd -r rusty-photon"
    And doctor exits with code 1

  Scenario: The packaged udev rule passes when installed intact with resolvable groups
    Given platform facts with an enabled unit "rusty-photon-qhy-camera"
    And the installed udev rule for "qhy-camera" is the packaged rule
    And hardware facts where host group "rusty-photon" has gid 990
    And hardware facts with a USB device "1618:c179" with no product string
    And hardware facts with a directory at "/lib/firmware/qhy"
    And hardware facts with an executable file at "/usr/local/sbin/fxload"
    And hardware facts with a regular file at "/etc/udev/rules.d/85-qhyccd.rules"
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.udev-rule" for service "qhy-camera"
    And the report contains an "ok" check named "hardware.usb-device" for service "qhy-camera"
    And the report contains an "ok" check named "hardware.firmware-helper" for service "qhy-camera"
    And doctor exits with code 0

  Scenario: An operator-edited udev rule warns without failing
    Given platform facts with an enabled unit "rusty-photon-qhy-camera"
    And the installed udev rule for "qhy-camera" is the packaged rule with a local edit appended
    And hardware facts where host group "rusty-photon" has gid 990
    When I run doctor with --json
    Then the report contains a "warn" check named "hardware.udev-rule" for service "qhy-camera"
    And that check's detail mentions "differs from the packaged rule"

  Scenario: A missing udev rule fails an enabled camera service
    Given platform facts with an enabled unit "rusty-photon-zwo-camera"
    And hardware facts staged empty
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.udev-rule" for service "zwo-camera"
    And that check's detail mentions "90-rusty-photon-zwo.rules"

  Scenario: A partial firmware install names what is missing
    Given platform facts with an enabled unit "rusty-photon-qhy-camera"
    And the installed udev rule for "qhy-camera" is the packaged rule
    And hardware facts where host group "rusty-photon" has gid 990
    And hardware facts with a directory at "/lib/firmware/qhy"
    And hardware facts with a regular file at "/etc/udev/rules.d/85-qhyccd.rules"
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.firmware-helper" for service "qhy-camera"
    And that check's detail mentions "/usr/local/sbin/fxload"
    And that check's suggestion mentions "rusty-photon-qhy-firmware-install"

  Scenario: A configured COM port must be present on Windows
    Given Windows platform facts with an enabled unit "rusty-photon-ppba-driver"
    And a config file "ppba-driver.json" containing:
      """
      { "serial": { "port": "COM7" } }
      """
    And hardware facts with present COM ports "COM3, COM4"
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.serial-node" for service "ppba-driver"
    And that check's detail mentions "COM3"

  Scenario: Scenarios without hardware facts run no hardware checks
    Given platform facts with an enabled unit "rusty-photon-ppba-driver"
    When I run doctor with --json
    Then the report has no checks named "hardware.serial-node"

  Scenario: A root-owned data directory fails rp in packaged mode
    Given platform facts with an enabled unit "rusty-photon-rp"
    And a config directory with an existing data directory
    And a config file "rp.json" with session.data_directory pointing at that data directory on port 11115
    And hardware facts where the data directory is owned by uid 0 gid 0 with mode "0755"
    And hardware facts where the rusty-photon user has uid 990 and gid 990
    When I run doctor with --json
    Then the report contains a "fail" check named "rp.data-directory" for service "rp"
    And that check's detail mentions "not writable"
    And that check's suggestion mentions "chown"
