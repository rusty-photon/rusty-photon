Feature: Setup pages
  The driver configures itself through the two HTML pages the Alpaca
  Management API defines, served by the driver itself: /setup for the whole
  server and /setup/v1/telescope/0/setup for the mount. Both answer GET with
  a form filled from the configuration file and POST with a save that
  overlays the submitted fields onto that page's section of the file,
  validates, persists, and reloads the driver in process. The transport is
  a tagged enum, usb or udp: the server page offers it as a selector with
  the chosen kind's fields, the other kind's fields start from their
  defaults, and a save that switches the kind replaces the whole block. A
  rule on the chosen kind's field is shown beside that field. A field
  pinned by a command-line override is shown disabled and never persisted:
  the transport flag pins the whole block, the port flag the field it lands
  on. A value the mount's own config types refuse at the typed parse is
  rejected with the rule named above the form. The fields the mount writes
  at runtime (the park position, the AP-park choices) are named on its
  page as edited in the file, not offered as inputs. No password hash
  reaches a page. A POST that does not come from the page itself is
  refused with 403.
  With auth configured the pages answer 401 without a credential and
  render with one. The config.get, config.apply and config.schema actions
  are gone: SupportedActions lists the three ApPark actions alone.

  Scenario: The server page offers the transport as a selector and links to the mount's page
    Given a running star-adventurer service
    When I open the server setup page
    Then the transport selector shows "usb" chosen
    And the page shows an input named "transport:usb.port" with value "/dev/mock"
    And the page shows an input named "transport:usb.baud_rate" with value "115200"
    And the page shows an input named "transport:udp.port" with value "11880"
    And the page shows an input named "server.port" with value "0"
    And the page shows no input named "mount.site_latitude_deg"
    And the page links to the setup page of device "telescope" 0

  Scenario: The mount's page shows only its own section, and names what the mount writes itself
    The park position SetPark captures is written by the driver at runtime:
    the page names it rather than carrying an input a stale form would send
    back over the device's own write.
    Given a running star-adventurer service
    When I open the setup page of device "telescope" 0
    Then the page shows an input named "mount.description" with value "Sky-Watcher Star Adventurer GTi German Equatorial Mount"
    And the page shows an input named "mount.auto_flip.meridian_offset_hours" with value "0.0"
    And the page names "mount.park_ra_ticks" as edited in the configuration file
    And the page names "mount.preferred_ap_park" as edited in the configuration file
    And the page shows no input named "transport:usb.port"

  Scenario: A valid save is persisted, reloaded, and served back after the wait
    Given a running star-adventurer service
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "telescope" 0 setting "mount.description" to "Renamed Mount"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "Renamed Mount" at "/mount/description"
    And the configuration file holds "/dev/mock" at "/transport/port"
    And after the reload the setup page of device "telescope" 0 shows "mount.description" as "Renamed Mount"

  Scenario: Switching the transport replaces the block, and the reloaded server offers the new kind
    The udp fields the operator did not type come from the schema's
    defaults the page rendered for the unchosen kind.
    Given a running star-adventurer service
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the server setup page switching the transport to udp at "192.168.4.1" bound to "192.168.4.2"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "udp" at "/transport/kind"
    And the configuration file holds "192.168.4.1" at "/transport/address"
    And the configuration file holds 11880 at "/transport/port"
    And after the reload the server setup page shows "transport:udp.address" as "192.168.4.1"

  Scenario: A rule on the chosen transport's field is shown beside that field
    Given a running star-adventurer service
    When I submit the server setup page setting "transport:usb.port" to ""
    Then the page answers 422 with the error "must not be empty" on "transport:usb.port"
    And the configuration file holds "/dev/mock" at "/transport/port"

  Scenario: An auto-flip offset no flip can happen at is refused beside its field
    Given a running star-adventurer service
    When I submit the setup page of device "telescope" 0 enabling the auto-flip at an offset of "0.95" hours
    Then the page answers 422 with the error "names a point no flip can happen at" on "mount.auto_flip.meridian_offset_hours"
    And the configuration file holds false at "/mount/auto_flip/enabled"

  Scenario: A value the mount's config types refuse is rejected with the rule named
    Given a running star-adventurer service
    When I submit the setup page of device "telescope" 0 setting "mount.tracking_guard_margin_hours" to "5"
    Then the page reports the submitted values were rejected naming "tracking_guard_margin_hours must be in [0, 1] hours"
    And the configuration file holds 0.05 at "/mount/tracking_guard_margin_hours"

  Scenario: A command-line port pins the chosen transport's path and nothing else
    Given a running star-adventurer service with the serial port pinned to "/dev/ttyACM9" by a command-line override
    When I open the server setup page
    Then the input named "transport:usb.port" shows "/dev/ttyACM9" and is disabled
    And the transport selector shows "usb" chosen
    When I submit the server setup page setting "transport:usb.baud_rate" to "9600"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds 9600 at "/transport/baud_rate"
    And the configuration file holds "/dev/mock" at "/transport/port"

  Scenario: A command-line transport pins the whole block as the running service has it
    The file says usb; the flag runs the driver on udp. The page shows the
    running block, pinned, and a save leaves the file's block alone.
    Given a running star-adventurer service with the transport pinned to udp by a command-line override
    When I open the server setup page
    Then the transport selector is pinned to "udp"
    And the input named "transport:udp.address" shows "192.168.4.1" and is disabled
    When I submit the server setup page setting "server.bind_address" to "0.0.0.0"
    Then the page reports the configuration was saved and nothing changed
    And the configuration file holds "usb" at "/transport/kind"

  Scenario: A save that changes nothing needs no reload
    Given a running star-adventurer service
    When I submit the server setup page setting "transport:usb.baud_rate" to "115200"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A cross-site POST is refused
    Given a running star-adventurer service
    When I submit the server setup page from the origin "https://elsewhere.example" setting "transport:usb.baud_rate" to "9600"
    Then the response status is 403
    And the configuration file holds 115200 at "/transport/baud_rate"

  Scenario: A server port change answers with the new address
    Given a running star-adventurer service
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: No password hash reaches a page, and the pages are behind the credential
    Given a running star-adventurer service with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash

  Scenario: The configuration actions are gone and the ApPark actions stay
    Given a running star-adventurer service
    When the supported actions are queried
    Then the supported actions are exactly SetUnparkFromApPosition, SetPreferredApPark and UnparkFromApPosition
    When the action "config.get" is called
    Then the call should fail with an action-not-implemented error
