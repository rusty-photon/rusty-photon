Feature: Setup pages
  The driver configures itself through the two HTML pages the Alpaca
  Management API defines, served by the driver itself: /setup for the whole
  server and /setup/v1/focuser/0/setup for the device. Both answer GET with
  a form filled from the configuration file and POST with a save that
  overlays the submitted fields onto that page's section of the file,
  validates, persists, and reloads the driver in process. A save of one page
  never alters the other page's section. A field pinned by a command-line
  override is shown disabled and never persisted. No password hash reaches a
  page. A POST that does not come from the page itself is refused with 403
  and the file is untouched. With auth configured the pages answer 401
  without a credential and render with one. The config.get, config.apply and
  config.schema actions are gone: SupportedActions is empty.

  Scenario: The server page shows the current configuration and links to the device page
    Given a running focuser service
    When I open the server setup page
    Then the page shows an input named "serial.port" with value "/dev/mock"
    And the page shows an input named "serial.baud_rate" with value "9600"
    And the page shows no input named "focuser.max_step"
    And the page links to the setup page of device "focuser" 0

  Scenario: The device page shows only the device's own section
    Given a running focuser service
    When I open the setup page of device "focuser" 0
    Then the page shows an input named "focuser.max_step" with value "64000"
    And the page shows an input named "focuser.speed" with value "0"
    And the page shows no input named "serial.port"

  Scenario: A valid save is persisted, reloaded, and served back after the wait
    Given a running focuser service
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "focuser" 0 setting "focuser.max_step" to "32000"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds 32000 at "/focuser/max_step"
    And the configuration file holds "/dev/mock" at "/serial/port"
    And after the reload the setup page of device "focuser" 0 shows "focuser.max_step" as "32000"

  Scenario: An invalid save names the field and leaves the file untouched
    Given a running focuser service
    When I submit the server setup page setting "serial.baud_rate" to "0"
    Then the page answers 422 with the error "must be greater than 0" on "serial.baud_rate"
    And the configuration file holds 9600 at "/serial/baud_rate"

  Scenario: A device-page validation error is shown beside its field
    Given a running focuser service
    When I submit the setup page of device "focuser" 0 setting "focuser.max_step" to "0"
    Then the page answers 422 with the error "must be greater than 0" on "focuser.max_step"
    And the configuration file holds 64000 at "/focuser/max_step"

  Scenario: An override-pinned field is shown pinned and never persisted
    Given a running focuser service with the serial port pinned to "/dev/ttyACM9" by a command-line override
    When I open the server setup page
    Then the input named "serial.port" shows "/dev/ttyACM9" and is disabled
    When I submit the server setup page setting "serial.baud_rate" to "19200"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds 19200 at "/serial/baud_rate"
    And the configuration file holds "/dev/mock" at "/serial/port"

  Scenario: A save that changes nothing needs no reload
    Given a running focuser service
    When I submit the server setup page setting "serial.baud_rate" to "9600"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A cross-site POST is refused
    Given a running focuser service
    When I submit the server setup page from the origin "https://elsewhere.example" setting "serial.baud_rate" to "19200"
    Then the response status is 403
    And the configuration file holds 9600 at "/serial/baud_rate"

  Scenario: A server port change answers with the new address
    Given a running focuser service
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: No password hash reaches a page, and the pages are behind the credential
    Given a running focuser service with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash

  Scenario: The configuration actions are gone
    Given a running focuser service
    When the supported actions are queried
    Then the supported actions are empty
