@serial
Feature: Setup pages
  zwo-camera configures itself through the two HTML pages the Alpaca
  Management API defines, served by the service itself: /setup for the
  whole server and /setup/v1/camera/{n}/setup for each camera it serves.
  The server page holds the server block, max_adu_reporting (a dropdown of
  its two contracts; a value outside them is refused beside it), the
  Security section and the Cameras section, which edits usb_devices.

  The Cameras section has two modes: every camera the USB scan places,
  numbered in port order, or only the checked cameras. It shows one row per
  port that holds an ASI record now or is listed, saying what the running
  service serves there. With a list, the checked rows keep the order of the
  numbers they have now, and a newly checked row takes the next number. A
  save that would move a served camera to another number, or give a
  dropped camera's number to another, or drop names, answers 409 with the
  change, writing nothing until the operator saves that page again. With
  no list after the save, the numbers are the port order the reload would
  give the bus as the page's own scan finds it, a record new since the
  start taken for a camera. A change of mode carries the names across: to a
  list, each camera's devices override fills its row and the devices map
  goes; to no list, each listed camera's names become an override under its
  serial, which the start read when it opened the camera, and any name the
  change would drop is named in a confirmation -- a placeholder's, and a
  listed camera's whose open failed, since its serial was never read.
  Unchecking every row saves an empty list, and the server page is still
  served.

  A listed camera's page and a placeholder's page show what the driver
  knows and link to the Cameras section. A camera served without a list
  edits its devices override on its own page: the first save that names it
  creates the override, and a save that clears both names deletes it. The
  config.get, config.apply and config.schema actions are gone.

  The simulation build's scan holds one record by default, 03c3:2600 on
  simulated-usbv3-0:1, where the simulated camera (serial 0a1b2c3d4e5f6071)
  is placed. The staged scans here add 03c3:178c on simulated-usbv3-0:2, an
  ASI178MM record no SDK camera is placed on, so a listed
  simulated-usbv3-0:2 is a placeholder.

  Scenario: The server page holds the server block and the Cameras section, and links to the camera's page
    Given a running zwo-camera service with the simulation backend
    When I open the server setup page
    Then the page shows an input named "server.bind_address" with value "0.0.0.0"
    And the page shows a dropdown named "max_adu_reporting" offering "saturation_threshold, container_full_scale" with "saturation_threshold" chosen
    And the Cameras section's mode is "auto"
    And the Cameras section lists the port "simulated-usbv3-0:1" as "camera 0"
    And the page links to the setup page of device "camera" 0

  Scenario: A camera served without a list is named on its own page, which creates its override
    Given a running zwo-camera service with the simulation backend
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I open the setup page of device "camera" 0
    Then the page shows an input named "override.name" with value ""
    And the page says "simulated-usbv3-0:1"
    When I submit the setup page of device "camera" 0 setting "override.name" to "Main Imaging"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "Main Imaging" at "/devices/0a1b2c3d4e5f6071/name"
    And after the reload the setup page of device "camera" 0 shows "override.name" as "Main Imaging"
    And camera device 0 reports the Name "Main Imaging"

  Scenario: Clearing both names on a camera's page deletes its override
    Given the configuration JSON {"devices": {"0a1b2c3d4e5f6071": {"name": "Main"}}}
    When the zwo-camera service starts
    Given the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "camera" 0 setting "override.name" to ""
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file has nothing at "/devices/0a1b2c3d4e5f6071"

  Scenario: Changing to a list keeps the camera's number and carries its names into its row
    Given the configuration JSON {"devices": {"0a1b2c3d4e5f6071": {"name": "Main", "description": "ASI2600MM"}}}
    When the zwo-camera service starts
    Given the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the server setup page setting "usb_devices.mode" to "list"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [{"device_number": 0, "usb_port": "simulated-usbv3-0:1", "name": "Main", "description": "ASI2600MM"}] at "/usb_devices"
    And the configuration file holds {} at "/devices"
    And after the reload the Cameras section lists the port "simulated-usbv3-0:1" as "camera 0"
    And camera device 0 reports the Name "Main"

  Scenario: A listed camera's page shows what the driver knows and links to its row
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
    When the zwo-camera service starts
    And I open the setup page of device "camera" 0
    Then the page says "simulated-usbv3-0:1"
    And the page says "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"
    And the page shows no input named "override.name"
    And the page offers no Save button
    And the page links to the Cameras section of the server page

  Scenario: A placeholder's page names the port it stands in for and why, and links to the Cameras section
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
    When the zwo-camera service starts
    And I open the setup page of device "camera" 0
    Then the page says "simulated-usbv3-0:9"
    And the page says "no working camera is enumerated on simulated-usbv3-0:9"
    And the page offers no Save button
    And the page links to the Cameras section of the server page

  Scenario: Checking a row serves its port at the next number
    Given the staged USB inventory:
      """
      { "usb": [ { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:1" },
                 { "vendor": "03c3", "product": "178c", "port": "simulated-usbv3-0:2" } ] }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the zwo-camera service starts
    And I open the server setup page
    Then the Cameras section's mode is "list"
    And the Cameras section lists the port "simulated-usbv3-0:2" as "not served"
    When I submit the server setup page checking the row for port "simulated-usbv3-0:2"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [{"device_number": 0, "usb_port": "simulated-usbv3-0:1"}, {"device_number": 1, "usb_port": "simulated-usbv3-0:2"}] at "/usb_devices"
    And after the reload the Cameras section lists the port "simulated-usbv3-0:2" as "placeholder 1"

  Scenario: Unchecking a camera moves the numbers after it, once the move is confirmed
    Given the staged USB inventory:
      """
      { "usb": [ { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:1" },
                 { "vendor": "03c3", "product": "178c", "port": "simulated-usbv3-0:2" } ] }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:2 |
      | 1             | simulated-usbv3-0:1 |
    When the zwo-camera service starts
    And I submit the server setup page unchecking the row for port "simulated-usbv3-0:2"
    Then the page asks to confirm that the camera on "simulated-usbv3-0:1" moves from 1 to 0
    And the page warns that the rp camera entries bound to those numbers must be renumbered
    And the configuration file holds "simulated-usbv3-0:2" at "/usb_devices/0/usb_port"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [{"device_number": 0, "usb_port": "simulated-usbv3-0:1"}] at "/usb_devices"
    And within 30 seconds camera device 0 reports the UniqueID "ZWO:ASI2600MM-Pro-Simulated:0a1b2c3d4e5f6071"

  Scenario: A dropped camera's number given to another camera is confirmed first
    Given the staged USB inventory:
      """
      { "usb": [ { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:1" },
                 { "vendor": "03c3", "product": "178c", "port": "simulated-usbv3-0:2" } ] }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the zwo-camera service starts
    And I submit the server setup page unchecking the row for port "simulated-usbv3-0:1" and checking the row for port "simulated-usbv3-0:2"
    Then the page asks to confirm that number 0 passes from the camera on "simulated-usbv3-0:1" to the one on "simulated-usbv3-0:2"
    And the page warns that the rp camera entries bound to those numbers must be renumbered
    And the configuration file holds "simulated-usbv3-0:1" at "/usb_devices/0/usb_port"

  Scenario: Unchecking every row asks before dropping the names, saves an empty list, and the server page is still served
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
    When the zwo-camera service starts
    And I submit the server setup page unchecking the row for port "simulated-usbv3-0:1"
    Then the page asks to confirm dropping the names of "simulated-usbv3-0:1"
    And the configuration file holds "Main" at "/usb_devices/0/name"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [] at "/usb_devices"
    And after the reload the Cameras section lists the port "simulated-usbv3-0:1" as "not served"
    And the running service registers no Camera device

  Scenario: Leaving the list after a record appeared on an earlier port asks to confirm the move
    Given the staged USB inventory:
      """
      { "usb": [ { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:2" } ] }
      """
    And the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:2 |
    When the zwo-camera service starts
    And the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "03c3", "product": "178c", "port": "simulated-usbv3-0:1" },
                 { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:2" } ] }
      """
    And I submit the server setup page setting "usb_devices.mode" to "auto"
    Then the page asks to confirm that the camera on "simulated-usbv3-0:2" moves from 0 to 1
    And the page says "The bus has changed since the service last started"
    And the configuration file holds "simulated-usbv3-0:2" at "/usb_devices/0/usb_port"

  Scenario: Without a list, a save the changed bus would renumber asks first, and one that changes nothing does not
    Given the staged USB inventory:
      """
      { "usb": [ { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:2" } ] }
      """
    When the zwo-camera service starts
    And the staged USB inventory becomes:
      """
      { "usb": [ { "vendor": "03c3", "product": "178c", "port": "simulated-usbv3-0:1" },
                 { "vendor": "03c3", "product": "2600", "port": "simulated-usbv3-0:2" } ] }
      """
    And I submit the server setup page setting "server.bind_address" to "0.0.0.0"
    Then the page reports the configuration was saved and nothing changed
    When I submit the setup page of device "camera" 0 setting "override.name" to "Main"
    Then the page asks to confirm that the camera on "simulated-usbv3-0:2" moves from 0 to 1
    And the configuration file has nothing at "/devices/0a1b2c3d4e5f6071"

  Scenario: Changing to no list carries each listed camera's names into its override
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
    When the zwo-camera service starts
    And I submit the server setup page setting "usb_devices.mode" to "auto"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file has nothing at "/usb_devices"
    And the configuration file holds "Main" at "/devices/0a1b2c3d4e5f6071/name"
    And after the reload the Cameras section's mode is "auto"

  Scenario: Changing to no list names a placeholder's names it would drop, and saves once confirmed
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
      | 1             | simulated-usbv3-0:9 | Old  |
    When the zwo-camera service starts
    And I submit the server setup page setting "usb_devices.mode" to "auto"
    Then the page asks to confirm dropping the names of "simulated-usbv3-0:9"
    And the configuration file holds "simulated-usbv3-0:9" at "/usb_devices/1/usb_port"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file has nothing at "/usb_devices"
    And the configuration file holds "Main" at "/devices/0a1b2c3d4e5f6071/name"

  Scenario: Changing to no list names the names of a listed camera whose open failed, since its serial was never read
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
    When the zwo-camera service starts with a simulated camera that cannot be opened
    And I submit the server setup page setting "usb_devices.mode" to "auto"
    Then the page asks to confirm dropping the names of "simulated-usbv3-0:1"
    And the configuration file holds "Main" at "/usb_devices/0/name"

  Scenario: The config actions are gone
    Given a running zwo-camera service with the simulation backend
    When the supported actions are queried on camera device 0
    Then the supported actions are empty
    When the action "config.get" is called on camera device 0
    Then the call should fail with an action-not-implemented error

  Scenario: The server page saves the MaxADU contract chosen from its dropdown
    Given a running zwo-camera service with the simulation backend
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the server setup page setting "max_adu_reporting" to "container_full_scale"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "container_full_scale" at "/max_adu_reporting"

  Scenario: A MaxADU contract outside the dropdown's values is refused beside it, the file untouched
    Given a running zwo-camera service with the simulation backend
    When I submit the server setup page setting "max_adu_reporting" to "full_scale"
    Then the page answers 422 with the error "must be one of the listed values" on "max_adu_reporting"
    And the configuration file has nothing at "/max_adu_reporting"

  Scenario: A save from another site is refused and the file is untouched
    Given a running zwo-camera service with the simulation backend
    When I submit the server setup page from the origin "http://evil.example" setting "server.bind_address" to "127.0.0.1"
    Then the response status is 403
    And the configuration file has nothing at "/server/bind_address"

  Scenario: The server's port moves from the page, which names its new address
    Given a running zwo-camera service with the simulation backend
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: With auth configured the pages answer 401 without a credential and render with one
    Given the zwo-camera service is running with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the response status is 200
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash
