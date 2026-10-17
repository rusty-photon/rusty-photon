Feature: Setup pages
  The driver configures itself through the two HTML pages the Alpaca
  Management API defines, served by the driver itself: /setup for the whole
  server and /setup/v1/covercalibrator/0/setup for the device. Both answer
  GET with a form filled from the effective configuration and POST with a
  save that overlays the submitted fields onto that page's section of the
  configuration file, validates, persists, and reloads the driver in
  process. A save of one page never alters the other page's section. A
  field pinned by a command-line override is shown disabled and never
  persisted. No password hash reaches a page. A POST that does not come
  from the page itself is refused with 403 and the file is untouched. With
  auth configured the pages answer 401 without a credential and render with
  one. The config.get, config.apply and config.schema actions are gone:
  SupportedActions is empty.

  Scenario: The server page shows the current configuration and links to the device page
    Given a running FP2 service
    When I open the server setup page
    Then the page shows an input named "serial.port" with value "/dev/mock"
    And the page shows an input named "serial.baud_rate" with value "115200"
    And the page shows no input named "cover_calibrator.max_brightness"
    And the page links to the setup page of device "covercalibrator" 0

  Scenario: The device page shows only the device's own section
    Given a running FP2 service
    When I open the setup page of device "covercalibrator" 0
    Then the page shows an input named "cover_calibrator.max_brightness" with value "4096"
    And the page shows an input named "cover_calibrator.unique_id" with value "dsd-fp2-bdd"
    And the page shows no input named "serial.port"

  Scenario: A valid save is persisted, reloaded, and served back after the wait
    Given a running FP2 service
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "covercalibrator" 0 setting "cover_calibrator.max_brightness" to "2048"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds 2048 at "/cover_calibrator/max_brightness"
    And the configuration file holds "/dev/mock" at "/serial/port"
    And after the reload the setup page of device "covercalibrator" 0 shows "cover_calibrator.max_brightness" as "2048"

  Scenario: An invalid save names the field and leaves the file untouched
    Given a running FP2 service
    When I submit the server setup page setting "serial.baud_rate" to "0"
    Then the page answers 422 with the error "must be greater than 0" on "serial.baud_rate"
    And the configuration file holds 115200 at "/serial/baud_rate"

  Scenario: A device-page validation error is shown beside its field
    Given a running FP2 service
    When I submit the setup page of device "covercalibrator" 0 setting "cover_calibrator.min_brightness" to "9999"
    Then the page answers 422 with the error "must be <= cover_calibrator.max_brightness" on "cover_calibrator.min_brightness"
    And the configuration file holds 250 at "/cover_calibrator/min_brightness"

  Scenario: An override-pinned field is shown pinned and never persisted
    Given a running FP2 service with the serial port pinned to "/dev/ttyACM9" by a command-line override
    When I open the server setup page
    Then the input named "serial.port" shows "/dev/ttyACM9" and is disabled
    When I submit the server setup page setting "serial.baud_rate" to "9600"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds 9600 at "/serial/baud_rate"
    And the configuration file holds "/dev/mock" at "/serial/port"

  Scenario: A save that changes nothing needs no reload
    Given a running FP2 service
    When I submit the server setup page setting "serial.baud_rate" to "115200"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A cross-site POST is refused
    Given a running FP2 service
    When I submit the server setup page from the origin "https://elsewhere.example" setting "serial.baud_rate" to "9600"
    Then the response status is 403
    And the configuration file holds 115200 at "/serial/baud_rate"

  Scenario: A server port change answers with the new address
    Given a running FP2 service
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: No password hash reaches a page, and the pages are behind the credential
    Given a running FP2 service with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash

  Scenario: The configuration actions are gone
    Given a running FP2 service
    When the supported actions are queried
    Then the supported actions are empty
