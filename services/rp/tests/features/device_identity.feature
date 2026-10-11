@serial
Feature: Device identity pin
  A roster entry addresses its device by position: alpaca_url names the
  Alpaca server, and device_number is the device's position among that
  server's devices of the entry's kind in configureddevices. Every
  equipment entry, of every kind, takes an optional unique_id, the
  UniqueID the device at that position must report. On every establish,
  the startup connect and the reconnect supervisor's re-establish alike,
  rp reads that UniqueID from the configureddevices entry it binds. A pin
  that matches binds. A pin that differs, or one the listed entry cannot
  verify, is refused before Connected=true is sent, and rp never
  re-resolves the entry to another number. Without a pin, the entry
  binds whatever device its server lists there. GET /api/equipment names
  the device each connected entry is bound to by device_name and
  unique_id, and reports both as null while the entry is disconnected.

  Scenario Outline: Every device kind binds only the device that reports its pinned UniqueID
    Given a running Alpaca simulator
    And rp is configured with the simulator's <kind> pinned to <pin>
    When rp starts
    Then the equipment status should show the pinned <kind> <outcome>

    Examples:
      | kind                        | pin                                     | outcome                |
      | camera                      | the UniqueID the simulator lists for it | bound to that UniqueID |
      | camera                      | a UniqueID the simulator does not list  | refused and unbound    |
      | filter wheel                | the UniqueID the simulator lists for it | bound to that UniqueID |
      | filter wheel                | a UniqueID the simulator does not list  | refused and unbound    |
      | focuser                     | the UniqueID the simulator lists for it | bound to that UniqueID |
      | focuser                     | a UniqueID the simulator does not list  | refused and unbound    |
      | cover calibrator            | the UniqueID the simulator lists for it | bound to that UniqueID |
      | cover calibrator            | a UniqueID the simulator does not list  | refused and unbound    |
      | safety monitor              | the UniqueID the simulator lists for it | bound to that UniqueID |
      | safety monitor              | a UniqueID the simulator does not list  | refused and unbound    |
      | switch                      | the UniqueID the simulator lists for it | bound to that UniqueID |
      | switch                      | a UniqueID the simulator does not list  | refused and unbound    |
      | rotator                     | the UniqueID the simulator lists for it | bound to that UniqueID |
      | rotator                     | a UniqueID the simulator does not list  | refused and unbound    |
      | observing conditions device | the UniqueID the simulator lists for it | bound to that UniqueID |
      | observing conditions device | a UniqueID the simulator does not list  | refused and unbound    |
      | dome                        | the UniqueID the simulator lists for it | bound to that UniqueID |
      | dome                        | a UniqueID the simulator does not list  | refused and unbound    |
      | mount                       | the UniqueID the simulator lists for it | bound to that UniqueID |
      | mount                       | a UniqueID the simulator does not list  | refused and unbound    |

  Scenario: The equipment status names the device an unpinned camera is bound to
    Given a running Alpaca simulator
    And rp is configured with a camera on the simulator
    When rp starts
    Then the equipment status should show camera "main-cam" bound to "Alpaca Camera Sim" with UniqueID "3f6c2a51-9b7e-4d08-a3c4-5e1f8b2d7c90"

  Scenario: A pinned camera is refused on reconnect when its number comes back addressing another camera
    Given a stub Alpaca service hosting a camera with UniqueID "QHY600M-imaging"
    And rp is configured with a camera on the stub service
    And the camera "main-cam" is pinned to UniqueID "QHY600M-imaging"
    And an equipment reconnect interval of 500 milliseconds
    And a test webhook receiver subscribed to "equipment_changed"
    When rp starts
    Then the equipment status should show camera "main-cam" bound to UniqueID "QHY600M-imaging"
    When the stub Alpaca service comes back hosting a camera with UniqueID "QHY5III678M-guiding"
    Then an "equipment_changed" event should report the device "main-cam" as disconnected
    And rp has looked the stub's camera up 3 times since it came back without switching it on
    And the equipment status should show the camera as disconnected
    When the stub Alpaca service comes back hosting a camera with UniqueID "QHY600M-imaging"
    Then an "equipment_changed" event should report the device "main-cam" as connected
    And the equipment status should show camera "main-cam" bound to UniqueID "QHY600M-imaging"

  Scenario: An unpinned camera follows its number to whichever camera the server lists there
    Given a stub Alpaca service hosting a camera with UniqueID "QHY600M-imaging"
    And rp is configured with a camera on the stub service
    And an equipment reconnect interval of 500 milliseconds
    When rp starts
    Then the equipment status should show camera "main-cam" bound to UniqueID "QHY600M-imaging"
    When the stub Alpaca service comes back hosting a camera with UniqueID "QHY5III678M-guiding"
    Then the equipment status should show camera "main-cam" bound to UniqueID "QHY5III678M-guiding"
