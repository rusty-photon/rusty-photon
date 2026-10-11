Feature: Equipment page
  /equipment renders rp's equipment roster: the authoritative device list from
  rp's config (GET /api/config) joined by id with live connection state
  (GET /api/equipment), a capability tier per device from a live probe of the
  device's own Alpaca server, and add/edit/remove of roster entries performed
  as read-modify-write on rp's config (PUT /api/config). rp has no in-process
  reload, so the page always shows the roster rp is RUNNING: a mutation
  persists to rp's config file and renders the restart callout, and the list
  reflects it only after rp's next start. An empty form input means "unset —
  rp's default applies", never an empty string. A device whose
  supportedactions advertises config.get is "managed" and gets a
  roster-derived config page at /config/rp:{kind}:{id} — no hand-added BFF
  drivers entry needed.

  Scenario: The roster lists a configured device with its live connection state
    Given a running dsd-fp2 driver registered in rp's roster as cover calibrator "flat-panel"
    And a BFF pointed at rp
    When I open the equipment page
    Then the roster section "Cover calibrators" lists "flat-panel"
    And the roster row for "flat-panel" shows a connected LED

  Scenario: A config-actions-capable device is managed and serves a roster-derived config page
    Given a running dsd-fp2 driver registered in rp's roster as cover calibrator "flat-panel"
    And a BFF pointed at rp
    When I open the equipment page
    Then the roster row for "flat-panel" carries the "managed" tier
    When I open the config page for "rp:cover_calibrators:flat-panel"
    Then the page shows an input named "cover_calibrator.max_brightness" with value "4096"

  Scenario: Adding a device stores it in rp's config and reports the restart callout
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the add-equipment form for "cover_calibrators"
    And I submit the equipment form with:
      | field         | value              |
      | id            | new-flat           |
      | alpaca_url    | http://127.0.0.1:1 |
      | device_number | 0                  |
    Then the page reports the changes take effect when rp is restarted
    And rp's config file on disk contains the string "new-flat"
    And the roster section "Cover calibrators" does not yet list "new-flat"

  Scenario: A camera's dark-library cooler grid renders as checkboxes and stores the checked rungs
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the add-equipment form for "cameras"
    Then the form offers a "cooler_targets_c" checkbox for value "-10"
    When I submit the equipment form checking "cooler_targets_c" values "-10, 5" with:
      | field         | value              |
      | id            | cooled-cam         |
      | alpaca_url    | http://127.0.0.1:1 |
      | device_number | 0                  |
    Then the page reports the changes take effect when rp is restarted
    And rp's config file JSON at "/equipment/cameras/0/cooler_targets_c" equals "[-10,5]"

  Scenario: A camera's identity pin is offered on the form and stored in rp's config
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the add-equipment form for "cameras"
    Then the page shows an input named "unique_id" with value ""
    When I submit the equipment form with:
      | field         | value              |
      | id            | pinned-cam         |
      | alpaca_url    | http://127.0.0.1:1 |
      | device_number | 0                  |
      | unique_id     | QHY600M-a1b2c3d4   |
    Then the page reports the changes take effect when rp is restarted
    And rp's config file on disk contains the string "QHY600M-a1b2c3d4" exactly once

  Scenario: An added entry with a duplicate id is rejected on the form
    Given a running dsd-fp2 driver registered in rp's roster as cover calibrator "flat-panel"
    And a BFF pointed at rp
    When I open the add-equipment form for "cover_calibrators"
    And I submit the equipment form with:
      | field         | value              |
      | id            | flat-panel         |
      | alpaca_url    | http://127.0.0.1:1 |
      | device_number | 0                  |
    Then the equipment form shows a problem mentioning "already exists"
    And rp's config file on disk contains the string "flat-panel" exactly once

  Scenario: Editing a device updates its entry in rp's config
    Given a running dsd-fp2 driver registered in rp's roster as cover calibrator "flat-panel"
    And a BFF pointed at rp
    When I open the edit-equipment form for cover calibrator "flat-panel"
    And I submit the equipment form with:
      | field      | value                |
      | alpaca_url | http://127.0.0.1:999 |
    Then the page reports the changes take effect when rp is restarted
    And rp's config file on disk contains the string "http://127.0.0.1:999"

  Scenario: Removing a device deletes it from rp's config
    Given a running dsd-fp2 driver registered in rp's roster as cover calibrator "flat-panel"
    And a BFF pointed at rp
    When I remove cover calibrator "flat-panel" from the roster
    Then the page reports the changes take effect when rp is restarted
    And rp's config file on disk does not contain the string "flat-panel"
    And the roster section "Cover calibrators" lists "flat-panel"

