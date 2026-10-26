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
  rejected with the rule named above the form. The power-up pose and the
  park target are dropdowns of the named poses on the mount's page, the
  park target without ap_park_0. The park position SetPark writes at
  runtime is named on the page as edited in the file, not offered as an
  input. No password hash reaches a page. A POST that does not come from
  the page itself is refused with 403.
  With auth configured the pages answer 401 without a credential and
  render with one. The driver has no custom Actions: SupportedActions is
  empty.

  The mount's page links to the recovery page, served at its path with
  ?recover. Its GET shows what the mount is doing, a Stop, the poses and
  a box to tick, and changes nothing. Its POST runs the command its form
  names. The Stop halts every motion, parked or not, and answers once
  both axes report stopped: 409 while the mount is not connected. The
  recovery runs on a still mount, parked or not, with the box ticked:
  422 without the box, 409 while the mount is not connected or is
  moving. Either answers 403 from another site. A refused command runs
  nothing; one that ran redirects to the page, which says what it did,
  once, on the address that redirect named.

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
    And the page shows a dropdown named "mount.tracking_rate" offering "sidereal" with "sidereal" chosen
    And the page shows a dropdown named "mount.unpark_from_ap_position" offering "ap_park_0, ap_park_1, ap_park_2, ap_park_3, ap_park_4, ap_park_5" with "ap_park_0" chosen
    And the page shows a dropdown named "mount.preferred_ap_park" offering "ap_park_1, ap_park_2, ap_park_3, ap_park_4, ap_park_5" with "ap_park_3" chosen
    And the page names "mount.park_ra_ticks" as edited in the configuration file
    And the page links to the mount's recovery
    And the page shows no input named "transport:usb.port"

  Scenario: The park target saved on the mount's page is persisted
    Given a running star-adventurer service
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "telescope" 0 setting "mount.preferred_ap_park" to "ap_park_2"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "ap_park_2" at "/mount/preferred_ap_park"

  Scenario: The park target refuses ap_park_0 beside its dropdown, the file untouched
    Given a running star-adventurer service
    When I submit the setup page of device "telescope" 0 setting "mount.preferred_ap_park" to "ap_park_0"
    Then the page answers 422 with the error "must be one of the listed values" on "mount.preferred_ap_park"
    And the configuration file holds "ap_park_3" at "/mount/preferred_ap_park"

  Scenario: The recovery page shows the poses and the mount's motion, and changes nothing
    Given a running star-adventurer service
    When I connect the device
    And I open the mount's recovery page
    Then the response status is 200
    And the page shows a dropdown named "pose" offering "ap_park_0, ap_park_1, ap_park_2, ap_park_3, ap_park_4, ap_park_5" with "ap_park_0" chosen
    And the page says "Motion: stopped"
    And the mount should not have received an encoder-seed command
    And AtPark should be false

  Scenario: The recovery page shows a tracking mount as tracking
    Given a running star-adventurer service
    When I connect the device
    And I enable tracking
    And I open the mount's recovery page
    Then the page says "Motion: tracking at the sidereal rate"
    And Tracking should be true

  Scenario: A command's result shows once, on the address its redirect named
    Given a running star-adventurer service
    When I connect the device
    And I press Stop on the mount's recovery page
    Then the page says "both axes reported stopped"
    When I open the mount's recovery page with the query "recover&ran=1"
    Then the page does not say "reported stopped"
    When I open the mount's recovery page with the query "recover=stopped"
    Then the page does not say "reported stopped"

  Scenario: The page's Stop halts a tracking mount and shows it stopped
    Given a running star-adventurer service
    When I connect the device
    And I enable tracking
    And I press Stop on the mount's recovery page
    Then the response status is 200
    And the page says "both axes reported stopped"
    And the page says "Motion: stopped"
    And Tracking should be false
    And the mount should have received command :L1
    And the mount should have received command :L2

  Scenario: The page's Stop leaves a parked mount parked
    Given a star-adventurer service configured with park_ra_ticks 0 and park_dec_ticks 0
    When I connect the device
    And I park the mount
    And I press Stop on the mount's recovery page
    Then the response status is 200
    And the page says "Motion: parked"
    And AtPark should be true

  Scenario: The page's Stop while the mount is not connected is refused with 409
    Given a running star-adventurer service
    When I press Stop on the mount's recovery page
    Then the response status is 409
    And the page says "The mount is not connected"
    And the page says "Motion: not connected"

  Scenario: The page's Stop from another site is refused with 403, and the mount keeps tracking
    Given a running star-adventurer service
    When I connect the device
    And I enable tracking
    And I press Stop on the mount's recovery page from the origin "http://evil.example"
    Then the response status is 403
    And Tracking should be true

  Scenario: A recovery without the box ticked is refused, and nothing runs
    Given a star-adventurer service configured with park_ra_ticks 0 and park_dec_ticks 0
    When I connect the device
    And I park the mount
    And I recover the mount from "ap_park_3" on its setup page without ticking the box
    Then the page answers 422 with the error "Tick the box" on "confirm"
    And the mount should not have received an encoder-seed command
    And AtPark should be true

  Scenario: A recovery while the mount is tracking is refused with 409, and nothing runs
    Given a running star-adventurer service
    When I connect the device
    And I enable tracking
    And I recover the mount from "ap_park_3" on its setup page, confirming it is there
    Then the response status is 409
    And the page says "the mount is tracking; stop it first"
    And the mount should not have received an encoder-seed command
    And Tracking should be true

  Scenario: A recovery while the mount is not connected is refused with 409
    Given a running star-adventurer service
    When I recover the mount from "ap_park_3" on its setup page, confirming it is there
    Then the response status is 409
    And the page says "not connected"

  Scenario: A recovery from another site is refused with 403, and nothing runs
    Given a star-adventurer service configured with park_ra_ticks 0 and park_dec_ticks 0
    When I connect the device
    And I park the mount
    And I recover the mount from "ap_park_3" on its setup page from the origin "http://evil.example"
    Then the response status is 403
    And the mount should not have received an encoder-seed command
    And AtPark should be true

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

  Scenario: The driver has no custom actions
    Given a running star-adventurer service
    When the supported actions are queried
    Then the supported actions are empty
    When the action "UnparkFromApPosition" is called
    Then the call should fail with an action-not-implemented error
    When the action "config.get" is called
    Then the call should fail with an action-not-implemented error
