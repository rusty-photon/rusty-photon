Feature: Setup pages
  The driver configures itself through the HTML pages the Alpaca Management
  API defines, served by the driver itself: /setup for the whole server,
  /setup/v1/switch/0/setup for the switch and
  /setup/v1/observingconditions/0/setup for the observing conditions. Each
  answers GET with a form filled from the configuration file and POST with a
  save that overlays the submitted fields onto that page's section of the
  file, validates, persists, and reloads the driver in process. A save of one
  page never alters another page's section. A field pinned by a command-line
  override is shown disabled and never persisted. The switch labels, a map,
  are named on the switch's page as edited in the configuration file. No
  password hash reaches a page. A POST that does not come from the page
  itself is refused with 403 and the file is untouched. With auth configured
  the pages answer 401 without a credential and render with one. The
  config.get, config.apply and config.schema actions are gone:
  SupportedActions is empty on both devices.

  Scenario: The server page shows the current configuration and links to both device pages
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And I open the server setup page
    Then the page shows an input named "serial.port" with value "/dev/mock"
    And the page shows an input named "serial.baud_rate" with value "9600"
    And the page shows no input named "switch.name"
    And the page shows no input named "observingconditions.averaging_period"
    And the page links to the setup page of device "switch" 0
    And the page links to the setup page of device "observingconditions" 0

  Scenario: A device page shows only its own device's section
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And I open the setup page of device "switch" 0
    Then the page shows an input named "switch.unique_id" with value "ppba-switch-001"
    And the page shows no input named "observingconditions.unique_id"
    And the page shows no input named "serial.port"
    When I open the setup page of device "observingconditions" 0
    Then the page shows an input named "observingconditions.averaging_period" with value "5m"
    And the page shows no input named "switch.unique_id"

  Scenario: The switch labels are edited in the configuration file, and the page says so
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And I open the setup page of device "switch" 0
    Then the page shows no input named "switch.labels"
    And the page names "switch.labels" as edited in the configuration file

  Scenario: A valid save is persisted, reloaded, and served back after the wait
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    Given the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "switch" 0 setting "switch.name" to "Renamed Switch"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "Renamed Switch" at "/switch/name"
    And the configuration file holds "ppba-observingconditions-001" at "/observingconditions/unique_id"
    And after the reload the setup page of device "switch" 0 shows "switch.name" as "Renamed Switch"

  Scenario: An invalid save names the field and leaves the file untouched
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And I submit the server setup page setting "serial.baud_rate" to "0"
    Then the page answers 422 with the error "must be greater than 0" on "serial.baud_rate"
    And the configuration file holds 9600 at "/serial/baud_rate"

  Scenario: A device-page validation error is shown beside its field
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And I submit the setup page of device "observingconditions" 0 setting "observingconditions.averaging_period" to "25h"
    Then the page answers 422 with the error "must not exceed 24h, the ASCOM AveragePeriod ceiling" on "observingconditions.averaging_period"
    And the configuration file holds "5m" at "/observingconditions/averaging_period"

  Scenario: An override-pinned field is shown pinned and never persisted
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server with the serial port pinned to "/dev/ttyACM9" by a command-line override
    And I open the server setup page
    Then the input named "serial.port" shows "/dev/ttyACM9" and is disabled
    When I submit the server setup page setting "serial.baud_rate" to "19200"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds 19200 at "/serial/baud_rate"
    And the configuration file holds "/dev/mock" at "/serial/port"

  Scenario: A save that changes nothing needs no reload
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And I submit the server setup page setting "serial.baud_rate" to "9600"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A cross-site POST is refused
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And I submit the server setup page from the origin "https://elsewhere.example" setting "serial.baud_rate" to "19200"
    Then the response status is 403
    And the configuration file holds 9600 at "/serial/baud_rate"

  Scenario: A server port change answers with the new address
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: No password hash reaches a page, and the pages are behind the credential
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server with TLS and auth
    And I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash

  Scenario: The configuration actions are gone on both devices
    Given a PPBA server config with switch enabled and OC enabled
    When I start the PPBA server
    And the supported actions are queried on the switch device
    Then the supported actions are empty
    When the supported actions are queried on the observingconditions device
    Then the supported actions are empty
