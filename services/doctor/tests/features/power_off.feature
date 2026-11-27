Feature: Powered-off devices
  A rig on the daily power cycle keeps its devices off whenever nothing
  needs them, so a device missing in the day is the normal case there.
  Before doctor reports a device missing, it reads the power-box output
  that rp's power map puts the device on, from the box's own driver. An
  output that reads off makes the check ok, naming the device as powered
  off; an output that reads on, or one doctor cannot read, leaves the
  verdict standing and says so. The reading covers a device missing from
  the bus or its serial node, a per-service doctor's check that marks a
  device absent, and an active camera driver's placeholder. A check about
  a whole service is ok only when every roster device on the service is
  powered off. Doctor asks a power box nothing when no device is missing.
  A device is on a service when its alpaca_url joins a service whose
  config doctor scans. The config tokens {power_box} and {stub_endpoint}
  stand for the stub power box's URL and the stub management endpoint's.

  Scenario: A device missing from the bus and its node, on an output that reads off, is powered off
    Given platform facts with an enabled unit "rusty-photon-dsd-fp2"
    And a config file "dsd-fp2.json" containing:
      """
      {}
      """
    And hardware facts with an empty but readable USB inventory
    And a stub power box whose output "Quad 12V Output" reads off
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cover_calibrators": [
            { "id": "flat-panel", "alpaca_url": "http://localhost:11119", "device_number": 0 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "Quad 12V Output", "devices": ["flat-panel"], "daily": true } ] } ] } }
      """
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.usb-device" for service "dsd-fp2"
    And that check's detail mentions "powered off, not missing"
    And that check's detail mentions "Quad 12V Output"
    And that check's detail mentions "reads off, and feeds flat-panel"
    And the report contains an "ok" check named "hardware.serial-node" for service "dsd-fp2"
    And that check's detail mentions "serial device /dev/ttyACM0 does not exist"
    And doctor exits with code 0

  Scenario: On Windows, a COM port missing from a listing that was read, on an output that reads off, is powered off
    Given Windows platform facts with an enabled unit "rusty-photon-dsd-fp2"
    And a config file "dsd-fp2.json" containing:
      """
      {}
      """
    And hardware facts with an empty but readable USB inventory
    And hardware facts with an empty but readable COM-port listing
    And a stub power box whose output "Quad 12V Output" reads off
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cover_calibrators": [
            { "id": "flat-panel", "alpaca_url": "http://localhost:11119", "device_number": 0 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "Quad 12V Output", "devices": ["flat-panel"], "daily": true } ] } ] } }
      """
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.serial-node" for service "dsd-fp2"
    And that check's detail mentions "serial port COM3 is not among the host's COM ports"
    And that check's detail mentions "reads off, and feeds flat-panel"

  Scenario: A fix run's final report reads the power of a missing device too
    Given platform facts with an enabled unit "rusty-photon-dsd-fp2"
    And a config file "dsd-fp2.json" containing:
      """
      {}
      """
    And hardware facts with an empty but readable USB inventory
    And a stub power box whose output "Quad 12V Output" reads off
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cover_calibrators": [
            { "id": "flat-panel", "alpaca_url": "http://localhost:11119", "device_number": 0 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "Quad 12V Output", "devices": ["flat-panel"], "daily": true } ] } ] } }
      """
    When I run doctor with --fix and --json
    Then the report contains an "ok" check named "hardware.usb-device" for service "dsd-fp2"
    And that check's detail mentions "powered off, not missing"

  Scenario: A device missing on an output that reads on fails as a missing device does
    Given platform facts with an enabled unit "rusty-photon-dsd-fp2"
    And a config file "dsd-fp2.json" containing:
      """
      {}
      """
    And hardware facts with an empty but readable USB inventory
    And a stub power box whose output "Quad 12V Output" reads on
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cover_calibrators": [
            { "id": "flat-panel", "alpaca_url": "http://localhost:11119", "device_number": 0 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "Quad 12V Output", "devices": ["flat-panel"], "daily": true } ] } ] } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.usb-device" for service "dsd-fp2"
    And that check's detail mentions "reads on, so flat-panel should be there"
    And doctor exits with code 1

  Scenario: A device missing on an output doctor cannot read fails, naming why
    Given platform facts with an enabled unit "rusty-photon-dsd-fp2"
    And a config file "dsd-fp2.json" containing:
      """
      {}
      """
    And hardware facts with an empty but readable USB inventory
    And a stub power box that answers HTTP 500
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cover_calibrators": [
            { "id": "flat-panel", "alpaca_url": "http://localhost:11119", "device_number": 0 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "Quad 12V Output", "devices": ["flat-panel"], "daily": true } ] } ] } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.usb-device" for service "dsd-fp2"
    And that check's detail mentions "doctor could not read flat-panel"
    And that check's detail mentions "answered HTTP 500"
    And doctor exits with code 1

  Scenario: A device the power map puts on no output keeps its verdict untouched
    Given platform facts with an enabled unit "rusty-photon-dsd-fp2"
    And a config file "dsd-fp2.json" containing:
      """
      {}
      """
    And hardware facts with an empty but readable USB inventory
    And a stub power box whose output "Quad 12V Output" reads off
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cover_calibrators": [
            { "id": "flat-panel", "alpaca_url": "http://localhost:11119", "device_number": 0 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "Quad 12V Output", "devices": [], "daily": true } ] } ] } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "hardware.usb-device" for service "dsd-fp2"
    And that check's detail does not mention "output"
    And the stub power box was asked nothing

  Scenario: A rig whose devices are present sends its power box nothing
    Given platform facts with an enabled unit "rusty-photon-dsd-fp2"
    And a config file "dsd-fp2.json" containing:
      """
      {}
      """
    And hardware facts with a USB device "2e8a:000a" reporting product string "FP2"
    And hardware facts with a character device "/dev/ttyACM0" owned by uid 0 gid 20 with mode "0660"
    And a stub power box whose output "Quad 12V Output" reads off
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cover_calibrators": [
            { "id": "flat-panel", "alpaca_url": "http://localhost:11119", "device_number": 0 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "Quad 12V Output", "devices": ["flat-panel"], "daily": true } ] } ] } }
      """
    When I run doctor with --json
    Then the report contains an "ok" check named "hardware.usb-device" for service "dsd-fp2"
    And that check's detail mentions "is present"
    And the stub power box was asked nothing

  Scenario: A per-service doctor's missing device is judged by the output its devices sit on
    Given a stub per-service doctor for "zwo-camera" whose report holds:
      """
      { "name": "usb-devices.resolve", "status": "warn",
        "detail": "device 1 (1-4.9) would be a placeholder: no working camera is enumerated on 1-4.9",
        "absent": { "device_type": "Camera", "device_number": 1 } }
      """
    And platform facts where unit "rusty-photon-zwo-camera" is installed but stopped, with the stub binary
    And a config file "zwo-camera.json" containing:
      """
      {}
      """
    And a stub power box whose output "USB Hub" reads off
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cameras": [
            { "id": "main-cam", "alpaca_url": "http://localhost:11122", "device_number": 0 },
            { "id": "guide-cam", "alpaca_url": "http://localhost:11122", "device_number": 1 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "USB Hub", "devices": ["guide-cam"], "daily": true } ] } ] } }
      """
    When I run doctor with --json
    Then the report contains an "ok" check named "usb-devices.resolve" for service "zwo-camera"
    And that check's detail mentions "reads off, and feeds guide-cam"

  Scenario: An active camera driver's placeholder on an output that reads off is powered off
    Given a stub management endpoint serving a camera and a placeholder for port "1-4.9"
    And a config file "qhy-camera.json" pointing at the stub endpoint
    And platform facts where unit "rusty-photon-qhy-camera" is installed and active
    And a stub power box whose output "USB Hub" reads off
    And a config file "rp.json" containing:
      """
      { "equipment": {
          "cameras": [
            { "id": "main-cam", "alpaca_url": "{stub_endpoint}", "device_number": 0 },
            { "id": "guide-cam", "alpaca_url": "{stub_endpoint}", "device_number": 1 } ],
          "switches": [
            { "id": "ppba", "alpaca_url": "{power_box}", "device_number": 0,
              "outputs": [ { "name": "USB Hub", "devices": ["guide-cam"], "daily": true } ] } ] } }
      """
    When I run doctor with --json
    Then the report has exactly one "ok" check named "service.devices" whose detail mentions "Camera #1 is a placeholder for 1-4.9"
    And that check's detail mentions "reads off, and feeds guide-cam"
    And that check's detail mentions "restarts qhy-camera once the output is on, places a camera that was only off"
    And the report has no checks named "service.devices" with status "warn"
