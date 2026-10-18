Feature: Setup pages
  The driver configures itself through the HTML pages the Alpaca Management
  API defines, served by the driver itself: /setup for the whole server,
  /setup/v1/rotator/0/setup for the rotator and /setup/v1/switch/0/setup for
  the status switch. Each answers GET with a form filled from the
  configuration file and POST with a save that overlays the submitted
  fields onto that page's section of the file, validates, persists, and
  reloads the driver in process. A save of one page never alters another
  page's section. A field pinned by a command-line override is shown
  disabled and never persisted. No password hash reaches a page. A POST
  that does not come from the page itself is refused with 403 and the file
  is untouched. With auth configured the pages answer 401 without a
  credential and render with one. The config.get, config.apply and
  config.schema actions are gone: SupportedActions is empty. These
  scenarios run the driver as a process, since a save reloads the process
  that served it, which the suite's in-process server cannot do.

  Scenario: The server page shows the current configuration and links to both device pages
    Given a running pa-falcon-rotator process
    When I open the server setup page
    Then the page shows an input named "serial.port" with value "/dev/mock"
    And the page shows an input named "serial.baud_rate" with value "9600"
    And the page shows no input named "rotator.name"
    And the page shows no input named "switch.name"
    And the page links to the setup page of device "rotator" 0
    And the page links to the setup page of device "switch" 0

  Scenario: A device page shows only its own device's section
    Given a running pa-falcon-rotator process
    When I open the setup page of device "rotator" 0
    Then the page shows an input named "rotator.unique_id" with value "pa-falcon-rotator-001"
    And the page shows no input named "switch.unique_id"
    And the page shows no input named "serial.port"
    When I open the setup page of device "switch" 0
    Then the page shows an input named "switch.unique_id" with value "pa-falcon-rotator-status-001"
    And the page shows no input named "rotator.unique_id"

  Scenario: A valid save is persisted, reloaded, and served back after the wait
    Given a running pa-falcon-rotator process
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "rotator" 0 setting "rotator.name" to "Renamed Rotator"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "Renamed Rotator" at "/rotator/name"
    And the configuration file holds "pa-falcon-rotator-status-001" at "/switch/unique_id"
    And after the reload the setup page of device "rotator" 0 shows "rotator.name" as "Renamed Rotator"

  Scenario: An invalid save names the field and leaves the file untouched
    Given a running pa-falcon-rotator process
    When I submit the server setup page setting "serial.baud_rate" to "0"
    Then the page answers 422 with the error "must be greater than 0" on "serial.baud_rate"
    And the configuration file holds 9600 at "/serial/baud_rate"

  Scenario: A device-page validation error is shown beside its field
    Given a running pa-falcon-rotator process
    When I submit the setup page of device "switch" 0 setting "switch.unique_id" to ""
    Then the page answers 422 with the error "must not be empty (it is the device's stable ASCOM UniqueID)" on "switch.unique_id"
    And the configuration file holds "pa-falcon-rotator-status-001" at "/switch/unique_id"

  Scenario: An override-pinned field is shown pinned and never persisted
    Given a running pa-falcon-rotator process with the serial port pinned to "/dev/ttyACM9" by a command-line override
    When I open the server setup page
    Then the input named "serial.port" shows "/dev/ttyACM9" and is disabled
    When I submit the server setup page setting "serial.baud_rate" to "19200"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds 19200 at "/serial/baud_rate"
    And the configuration file holds "/dev/mock" at "/serial/port"

  Scenario: A save that changes nothing needs no reload
    Given a running pa-falcon-rotator process
    When I submit the server setup page setting "serial.baud_rate" to "9600"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A cross-site POST is refused
    Given a running pa-falcon-rotator process
    When I submit the server setup page from the origin "https://elsewhere.example" setting "serial.baud_rate" to "19200"
    Then the response status is 403
    And the configuration file holds 9600 at "/serial/baud_rate"

  Scenario: A server port change answers with the new address
    Given a running pa-falcon-rotator process
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: No password hash reaches a page, and the pages are behind the credential
    Given a running pa-falcon-rotator process with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash

  Scenario: The configuration actions are gone on both devices
    Given a running pa-falcon-rotator process
    When the supported actions are queried on the rotator device
    Then the supported actions are empty
    When the supported actions are queried on the switch device
    Then the supported actions are empty
