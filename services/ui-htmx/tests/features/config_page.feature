Feature: driver configuration page
  The BFF serves a configuration page for a real driver, backed by the driver's
  own `config.get` / `config.schema` / `config.apply` ASCOM actions over HTTP.
  The form is generated from the driver's JSON Schema (`config.schema`). Every
  device target is derived from rp's equipment roster (#569) — these scenarios
  spawn the driver, a real rp with the driver rostered as focuser
  "qhy-focuser", and the BFF, and open `/config/rp:focusers:qhy-focuser`.
  Opening the page renders a form filled with the driver's current effective
  configuration; a field pinned by a command-line override is shown disabled.
  Submitting the form calls `config.apply` on the driver: an unchanged
  submission reports the configuration was saved with no reload needed; a valid
  change triggers the driver's in-process reload, the page reports it is reconnecting,
  and once the driver has reloaded the page serves the new value; an invalid
  change re-renders the form with the driver's field-level error and the
  submitted value preserved; and an unreachable driver surfaces an error banner.
  The driver is qhy-focuser: the one with mock hardware that still carries the
  config actions while the drivers move to their own setup pages, family by
  family.

  Scenario: The config page renders the driver's current configuration
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    When I open the qhy-focuser config page
    Then the page shows the value "/dev/ttyACM0"
    And the page shows the value "60000"
    And the rendered output matches the "config_page_current_config" snapshot

  Scenario: A serial-port override is shown read-only
    Given a qhy-focuser driver running with the serial port pinned to "/dev/ttyACM9" by a command-line override
    When I open the qhy-focuser config page
    Then the serial.port field is disabled
    And the page explains the field is pinned by a command-line override
    And the rendered output matches the "config_page_override_pinned" snapshot

  Scenario: Unique ID is read-only until unlocked, then editable
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    When I open the qhy-focuser config page
    Then the focuser.unique_id field is disabled
    And the page offers to unlock focuser.unique_id to edit it
    When I open the qhy-focuser config page with focuser.unique_id unlocked
    Then the focuser.unique_id field is editable
    And the rendered output matches the "config_card_unlocked" snapshot

  Scenario: A valid change is applied and the page reports the driver is reloading
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    When I submit the config form setting max_step to 30000
    Then the page reports the driver is reloading
    And the page polls the device's status route every 1s for reconnection
    And the rendered output matches the "config_card_reloading" snapshot

  Scenario: The reloaded driver's new configuration is served back through the page
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    And the driver's bound port is pinned so a reload keeps the same address
    When I submit the config form setting max_step to 30000
    And I poll the reconnect status until max_step 30000 is served
    Then the page shows the value "30000"

  Scenario: An unchanged submission reports the configuration was saved without a reload
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    When I submit the config form without changing anything
    Then the page reports the configuration was saved without a reload
    And the page shows the value "60000"

  Scenario: An invalid change re-renders the form with a field error
    Given a qhy-focuser driver running with serial.port "/dev/ttyACM0" and max_step 60000
    When I submit the config form setting baud_rate to 0
    Then the form shows the validation error "must be greater than 0" on serial.baud_rate
    And the submitted baud_rate value 0 is preserved
    And the rendered output matches the "config_card_invalid" snapshot

  Scenario: An unreachable driver surfaces an error
    Given the BFF is pointed at a qhy-focuser driver that is not running
    When I open the qhy-focuser config page
    Then the page shows a driver error
