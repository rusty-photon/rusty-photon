@serial
Feature: Setup pages
  zwo-focuser configures itself through the two HTML pages the Alpaca
  Management API defines, served by the service itself: /setup for the
  whole server and /setup/v1/focuser/{n}/setup for each EAF it serves. The
  server page holds the server block, the Security section and the list of
  the EAFs the start enumerated, each linking to its page. An EAF's page
  shows what the driver knows about it and edits its devices override,
  keyed by its SDK serial: the first save that names it creates the
  override, a save that clears both names deletes it, and the override of
  an EAF that is not connected is left as the file has it. The overrides
  are edited on the EAF pages alone, never on the server page. With no EAF
  connected the server page is still served. A field the command line pins
  is shown pinned and never persisted. The config.get, config.apply and
  config.schema actions are gone.

  The simulation backend presents one EAF-Simulated focuser, serial
  2a3b4c5d6e7f8091.

  Scenario: The server page holds the server block and links to the focuser's page
    Given a running zwo-focuser service with the simulation backend
    When I open the server setup page
    Then the page shows an input named "server.bind_address" with value "0.0.0.0"
    And the page names "server.tls" as edited in the configuration file
    And the page does not name "devices" as edited in the configuration file
    And the page links to the setup page of device "focuser" 0

  Scenario: A focuser's page shows what the driver knows and creates its override
    Given a running zwo-focuser service with the simulation backend
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I open the setup page of device "focuser" 0
    Then the page shows an input named "override.name" with value ""
    And the page lists the facts:
      | Model                  | EAF-Simulated                      |
      | Travel limit (MaxStep) | 60000                              |
      | UniqueID               | ZWO:EAF-Simulated:2a3b4c5d6e7f8091 |
    When I submit the setup page of device "focuser" 0 setting "override.name" to "Main Focuser"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "Main Focuser" at "/devices/2a3b4c5d6e7f8091/name"
    And after the reload the setup page of device "focuser" 0 shows "override.name" as "Main Focuser"
    And focuser device 0 reports the Name "Main Focuser"

  Scenario: Clearing both names deletes the focuser's override and keeps a disconnected EAF's
    Given a running zwo-focuser service started from the configuration {"devices": {"2a3b4c5d6e7f8091": {"name": "Main"}, "0000000000000000": {"name": "Spare"}}}
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I open the setup page of device "focuser" 0
    Then the page shows an input named "override.name" with value "Main"
    When I submit the setup page of device "focuser" 0 setting "override.name" to ""
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file has nothing at "/devices/2a3b4c5d6e7f8091"
    And the configuration file holds "Spare" at "/devices/0000000000000000/name"

  Scenario: With no EAF connected the server page is still served
    Given the zwo-focuser service running with an empty simulation backend
    When I open the server setup page
    Then the response status is 200
    And the page says "This server serves no device."

  Scenario: The port the command line pins is shown pinned and never persisted
    Given a running zwo-focuser service with its port pinned by the command line
    When I open the server setup page
    Then the input named "server.port" shows the pinned port and is disabled
    When I submit the server setup page setting "server.port" to "11999"
    Then the page reports the configuration was saved and nothing changed
    And the configuration file holds 0 at "/server/port"

  Scenario: The config actions are gone
    Given a running zwo-focuser service with the simulation backend
    When the supported actions are queried on focuser device 0
    Then the supported actions are empty
    When the action "config.get" is called on focuser device 0
    Then the call should fail with an action-not-implemented error

  Scenario: A save from another site is refused and the file is untouched
    Given a running zwo-focuser service with the simulation backend
    When I submit the server setup page from the origin "http://evil.example" setting "server.bind_address" to "127.0.0.1"
    Then the response status is 403
    And the configuration file has nothing at "/server/bind_address"

  Scenario: The server's port moves from the page, which names its new address
    Given a running zwo-focuser service with the simulation backend
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: With auth configured the pages answer 401 without a credential and render with one
    Given the zwo-focuser service is running with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the response status is 200
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash
