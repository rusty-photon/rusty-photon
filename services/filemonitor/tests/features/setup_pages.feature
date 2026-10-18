Feature: Setup pages
  The safety monitor configures itself through the two HTML pages the
  Alpaca Management API defines, served by the service itself: /setup for
  the whole server and /setup/v1/safetymonitor/0/setup for the device. Both
  answer GET with a form filled from the configuration file and POST with a
  save that overlays the submitted fields onto that page's section of the
  file, validates, persists, and reloads the service in process. A save of
  one page never alters the other page's section. A duration is shown as
  humantime spells it: the file's 60s as 1m. The parsing rules are a list
  of objects, which no form control renders: the server page names them as
  edited in the configuration file, and a rule whose regex does not compile
  refuses every save, named by its index above the form, until the file is
  fixed by hand. The binary takes no command-line overrides, so no field is
  ever pinned. No password hash reaches a page. A POST that does not come
  from the page itself is refused with 403 and the file is untouched. With
  auth configured the pages answer 401 without a credential and render
  with one. The config.get, config.apply and config.schema actions are
  gone: SupportedActions is empty.

  Scenario: The server page shows the file, parsing and server sections and links to the device page
    Given filemonitor is running
    When I open the server setup page
    Then the page shows an input named "file.polling_interval" with value "1m"
    And the page shows an input named "server.port" with value "0"
    And the page names "parsing.rules" as edited in the configuration file
    And the page shows no input named "device.name"
    And the page links to the setup page of device "safetymonitor" 0

  Scenario: The device page shows only the device's own section
    Given filemonitor is running
    When I open the setup page of device "safetymonitor" 0
    Then the page shows an input named "device.name" with value "Test"
    And the page shows an input named "device.unique_id" with value "test-001"
    And the page shows no input named "file.path"

  Scenario: A valid save is persisted, reloaded, and served back after the wait
    Given filemonitor is running
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the server setup page setting "file.polling_interval" to "30s"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "30s" at "/file/polling_interval"
    And the configuration file holds "test-001" at "/device/unique_id"
    And after the reload the server setup page shows "file.polling_interval" as "30s"

  Scenario: An invalid save names the field and leaves the file untouched
    Given filemonitor is running
    When I submit the server setup page setting "file.polling_interval" to "0s"
    Then the page answers 422 with the error "must be greater than 0" on "file.polling_interval"
    And the configuration file holds "60s" at "/file/polling_interval"

  Scenario: A device-page validation error is shown beside its field
    Given filemonitor is running
    When I submit the setup page of device "safetymonitor" 0 setting "device.unique_id" to ""
    Then the page answers 422 with the error "must not be empty" on "device.unique_id"
    And the configuration file holds "test-001" at "/device/unique_id"

  Scenario: A rule whose regex does not compile refuses every save, named by its index
    The rules are edited in the file. The service starts with the bad rule,
    which never matches, and the page refuses to persist until the file is
    fixed, so a typo cannot be saved over.
    Given a regex rule with pattern "(unclosed" that evaluates to safe
    And filemonitor is running with these rules
    When I submit the server setup page setting "file.polling_interval" to "30s"
    Then the page reports the submitted values were rejected naming "parsing.rules.0.pattern"
    And the configuration file holds "60s" at "/file/polling_interval"

  Scenario: A save that changes nothing needs no reload
    Given filemonitor is running
    When I submit the server setup page setting "file.polling_interval" to "60s"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A cross-site POST is refused
    Given filemonitor is running
    When I submit the server setup page from the origin "https://elsewhere.example" setting "file.polling_interval" to "30s"
    Then the response status is 403
    And the configuration file holds "60s" at "/file/polling_interval"

  Scenario: A server port change answers with the new address
    Given filemonitor is running
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: No password hash reaches a page, and the pages are behind the credential
    Given filemonitor is running with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash

  Scenario: The configuration actions are gone
    Given filemonitor is running
    When the supported actions are queried
    Then the supported actions are empty
    When the action "config.get" is called
    Then the call should fail with an action-not-implemented error
