Feature: Setup pages
  The bridge configures itself through the two HTML pages the Alpaca
  Management API defines, served by the service itself: /setup for the
  whole server and /setup/v1/telescope/0/setup for the virtual Telescope.
  Both answer GET with a form filled from the configuration file and POST
  with a save that overlays the submitted fields onto that page's section
  of the file, validates, persists, and reloads the service in process. A
  save of one page never alters the other page's section. The server page
  holds the rp, server, site and spool sections; the Telescope's page holds
  the device section. The credential the bridge presents to rp is an
  optional block the form does not render, so the server page names it as
  edited in the configuration file and its password never reaches a page;
  so is assume_epoch, a choice of two words, on the Telescope's page. That
  credential is bound to rp's address and to the CA that verifies rp
  there: a save that changes either asks first, keeps the credential when
  its password is typed again, and otherwise removes it from the file,
  the saved page saying so and staying on screen through the reload.
  Clearing the altitude floor saves the null that disables it. A value out
  of a field's range is refused naming the field, and the file is left as
  it was. The binary takes no command-line overrides, so no field is ever
  pinned. No password hash reaches a page. A POST that does not come from
  the page itself is refused with 403 and the file is untouched. With auth
  configured the pages answer 401 without a credential and render with
  one.

  Scenario: The server page shows the rp, server, site and spool sections and links to the Telescope's page
    Given the bridge is running
    When I open the server setup page
    Then the page shows an input named "rp.mcp_server_url" with value "http://127.0.0.1:1/mcp"
    And the page shows an input named "site.site_latitude_deg" with value "33.0"
    And the page shows an input named "spool.max_entries" with value "1000"
    And the page shows an input named "server.port" with value "0"
    And the page names "rp.service_auth" as edited in the configuration file
    And the page shows no input named "device.slew_duration"
    And the page links to the setup page of device "telescope" 0

  Scenario: The Telescope's page shows only the device section
    Given the bridge is running
    When I open the setup page of device "telescope" 0
    Then the page shows an input named "device.slew_duration" with value "3s"
    And the page shows an input named "device.report_altitude_floor_deg" with value "10.0"
    And the page names "device.assume_epoch" as edited in the configuration file
    And the page shows no input named "rp.mcp_server_url"

  Scenario: A valid save is persisted, reloaded, and served back after the wait
    Given the bridge is running
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the server setup page setting "site.site_latitude_deg" to "34.05"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds 34.05 at "/site/site_latitude_deg"
    And the configuration file holds "3s" at "/device/slew_duration"
    And after the reload the server setup page shows "site.site_latitude_deg" as "34.05"

  Scenario: Clearing the altitude floor saves the null that disables it
    Given the bridge is running
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "telescope" 0 setting "device.report_altitude_floor_deg" to ""
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds null at "/device/report_altitude_floor_deg"
    And after the reload the setup page of device "telescope" 0 shows "device.report_altitude_floor_deg" as ""

  Scenario: A value out of its range is refused naming the field, and the file is left
    Given the bridge is running
    When I submit the server setup page setting "site.site_latitude_deg" to "95"
    Then the page reports the submitted values were rejected naming "site.site_latitude_deg"
    And the configuration file holds 33.0 at "/site/site_latitude_deg"

  Scenario: A zero replay backoff cap is refused beside its field
    Given the bridge is running
    When I submit the server setup page setting "spool.replay_backoff_max" to "0s"
    Then the page answers 422 with the error "must be greater than 0" on "spool.replay_backoff_max"

  Scenario: A Telescope-page validation error is shown beside its field
    Given the bridge is running
    When I submit the setup page of device "telescope" 0 setting "device.unique_id" to ""
    Then the page answers 422 with the error "must not be empty" on "device.unique_id"

  Scenario: A save that changes nothing needs no reload
    Given the bridge is running
    When I submit the server setup page setting "spool.max_entries" to "1000"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A cross-site POST is refused
    Given the bridge is running
    When I submit the server setup page from the origin "https://elsewhere.example" setting "spool.max_entries" to "500"
    Then the response status is 403
    And the configuration file has nothing at "/spool/max_entries"

  Scenario: A server port change answers with the new address
    Given the bridge is running
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: The password the bridge presents to rp never reaches a page
    Given the bridge presents the username "bridge" and password "rp-secret-on-file" to rp
    And the bridge is running
    When I open the server setup page
    Then the page names "rp.service_auth" as edited in the configuration file
    And the page does not contain "rp-secret-on-file"

  Scenario: Moving rp elsewhere asks, then removes the credential the bridge presents to it and says so
    Given the bridge presents the username "bridge" and password "rp-secret-on-file" to rp
    And the bridge is running
    When I submit the server setup page setting "rp.mcp_server_url" to "http://elsewhere.example:11115/mcp"
    Then the page asks to confirm removing "rp.service_auth"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the page says "Removed from the file: rp.service_auth, since rp.mcp_server_url changed"
    And the saved page stays on the removal instead of reopening the setup page
    And the configuration file has nothing at "/rp/service_auth"

  Scenario: Trusting another CA for rp asks, then removes the credential the bridge presents to it
    Given the bridge presents the username "bridge" and password "rp-secret-on-file" to rp
    And the bridge is running
    When I submit the server setup page setting "rp.ca_cert" to "/etc/rusty-photon/ca.pem"
    Then the page asks to confirm removing "rp.service_auth"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the page says "Removed from the file: rp.service_auth, since rp.ca_cert changed"
    And the configuration file has nothing at "/rp/service_auth"

  Scenario: rp's password typed on the confirmation keeps the bridge's credential for the new address
    Given the bridge presents the username "bridge" and password "rp-secret-on-file" to rp
    And the bridge is running
    When I submit the server setup page setting "rp.mcp_server_url" to "http://rig.example:11115/mcp"
    And I save the confirmation typing "rp-secret-on-file" as the password that keeps "rp.service_auth"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "http://rig.example:11115/mcp" at "/rp/mcp_server_url"
    And the configuration file holds "rp-secret-on-file" at "/rp/service_auth/password"

  Scenario: A save that leaves rp's address and CA keeps the credential the bridge presents to it
    Given the bridge presents the username "bridge" and password "rp-secret-on-file" to rp
    And the bridge is running
    When I submit the server setup page setting "site.site_latitude_deg" to "34.05"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "bridge" at "/rp/service_auth/username"
    And the configuration file holds "rp-secret-on-file" at "/rp/service_auth/password"

  Scenario: No password hash reaches a page, and the pages are behind the credential
    Given the bridge is running with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash
