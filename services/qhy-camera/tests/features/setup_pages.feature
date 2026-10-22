@serial
Feature: Setup pages
  qhy-camera configures itself through the two HTML pages the Alpaca
  Management API defines, served by the service itself: /setup for the
  whole server, and /setup/v1/camera/{n}/setup and
  /setup/v1/filterwheel/{n}/setup for each camera and filter wheel it
  serves. The server page holds the server block, the Security section and
  the Cameras section, which edits usb_devices.

  The Cameras section has two modes: every camera the USB scan places,
  numbered in port order, with the wheels the CFW probe finds on them, or
  only the checked cameras. It shows one row per port that holds a QHY
  record now or is listed, saying what the running service serves there,
  camera and wheel. With a list, the checked rows keep the order of the
  numbers they have now and a newly checked row takes the next number; each
  row's wheel box declares the camera's filter wheel, and the checked wheels
  are numbered the same way among the wheels. A save that would move a
  served camera or wheel to another number, give a dropped one's number to
  another, or drop names answers 409 with the change, writing nothing until
  the operator saves that page again. Leaving a list that declares a wheel
  is always confirmed: without a list the reload numbers the wheels by what
  its CFW probe finds, which the page, opening nothing, cannot tell. A
  change of mode carries the names across, filter names included: to a
  list, each camera's devices override fills its row and its entry; to no
  list, each listed camera's names become an override under its SDK id.

  A listed camera's page and a placeholder's page show what the driver
  knows and link to the Cameras section. A camera served without a list
  edits its devices override on its own page. Every wheel's page, a
  placeholder wheel's too, edits its filter names, one per line: in its
  camera's entry with a list, in its camera's devices override without one.
  The config.get, config.apply and config.schema actions are gone.

  The simulation build's scan holds one record, 1618:f178 on
  simulated-usbv3-0:1, where the simulated camera (SDK id SIM-QHY178M) is
  placed; its 7-slot CFW is found by the probe. A listed
  simulated-usbv3-0:9 holds no record, so it is a placeholder, and so is
  the wheel its entry declares.

  Scenario: The server page holds the server block and the Cameras section, and links to the camera's and the wheel's pages
    Given a running qhy-camera service with the simulation backend
    When I open the server setup page
    Then the page shows an input named "server.bind_address" with value "0.0.0.0"
    And the Cameras section's mode is "auto"
    And the Cameras section lists the port "simulated-usbv3-0:1" as "camera 0"
    And the Cameras section lists the wheel on port "simulated-usbv3-0:1" as "filter wheel 0"
    And the page links to the setup page of device "camera" 0
    And the page links to the setup page of device "filterwheel" 0

  Scenario: A camera served without a list is named on its own page, which creates its override
    Given a running qhy-camera service with the simulation backend
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I open the setup page of device "camera" 0
    Then the page shows an input named "override.name" with value ""
    And the page says "simulated-usbv3-0:1"
    When I submit the setup page of device "camera" 0 setting "override.name" to "Main Imaging"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "Main Imaging" at "/devices/SIM-QHY178M/name"
    And after the reload the setup page of device "camera" 0 shows "override.name" as "Main Imaging"
    And camera device 0 reports the Name "Main Imaging"

  Scenario: A wheel served without a list names its filters on its own page, in its camera's override
    Given the configuration JSON {"devices": {"SIM-QHY178M": {"name": "Main"}}}
    When the qhy-camera service starts
    Given the bound port is pinned in the configuration file so a reload keeps the address
    When I open the setup page of device "filterwheel" 0
    Then the page says "CFW-SIM-QHY178M"
    And the page shows the filters ""
    When I submit the setup page of filter wheel 0 naming the filters "L, R, G, B, Ha, OIII, SII"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds {"name": "Main", "filter_names": ["L", "R", "G", "B", "Ha", "OIII", "SII"]} at "/devices/SIM-QHY178M"
    And after the reload the setup page of filter wheel 0 shows the filters "L, R, G, B, Ha, OIII, SII"
    Given filterwheel device 0 is connected
    Then filterwheel device 0 reports the filter names L, R, G, B, Ha, OIII, SII

  Scenario: A blank filter name among the others is refused beside the box, and nothing is written
    Given a running qhy-camera service with the simulation backend
    When I submit the setup page of filter wheel 0 naming the filters "L, , R"
    Then the page answers 422 with the error "filter name must not be empty" on "wheel.filter_names"
    And the page shows the filters "L, , R"
    And the configuration file has nothing at "/devices/SIM-QHY178M"

  Scenario: Changing to a list keeps the camera's and the wheel's numbers and carries their names into the entry
    Given the configuration JSON {"devices": {"SIM-QHY178M": {"name": "Main", "filter_names": ["L", "R"]}}}
    When the qhy-camera service starts
    Given the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the server setup page setting "usb_devices.mode" to "list"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [{"device_number": 0, "usb_port": "simulated-usbv3-0:1", "name": "Main", "filter_wheel_number": 0, "filter_names": ["L", "R"]}] at "/usb_devices"
    And the configuration file holds {} at "/devices"
    And after the reload the Cameras section lists the wheel on port "simulated-usbv3-0:1" as "filter wheel 0"
    And the running service registers 1 Camera device and 1 FilterWheel device

  Scenario: Checking a row's wheel declares it at the next wheel number
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:1 |
    When the qhy-camera service starts
    And I open the server setup page
    Then the Cameras section lists the wheel on port "simulated-usbv3-0:1" as "no wheel"
    When I submit the server setup page checking the wheel for port "simulated-usbv3-0:1"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [{"device_number": 0, "usb_port": "simulated-usbv3-0:1", "filter_wheel_number": 0}] at "/usb_devices"
    And after the reload the Cameras section lists the wheel on port "simulated-usbv3-0:1" as "filter wheel 0"
    And the running service registers 1 Camera device and 1 FilterWheel device

  Scenario: Unchecking a lower wheel moves the wheels after it, once the move is confirmed
    Given the configuration lists these USB devices:
      | device_number | usb_port            | filter_wheel_number |
      | 0             | simulated-usbv3-0:9 | 0                   |
      | 1             | simulated-usbv3-0:1 | 1                   |
    When the qhy-camera service starts
    And I open the server setup page
    Then the Cameras section lists the wheel on port "simulated-usbv3-0:9" as "placeholder 0"
    When I submit the server setup page unchecking the wheel for port "simulated-usbv3-0:9"
    Then the page asks to confirm that the filter wheel on "simulated-usbv3-0:1" moves from 1 to 0
    And the page warns that the rp filter wheel entries bound to those numbers must be renumbered
    And the configuration file holds 0 at "/usb_devices/0/filter_wheel_number"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [{"device_number": 0, "usb_port": "simulated-usbv3-0:9"}, {"device_number": 1, "usb_port": "simulated-usbv3-0:1", "filter_wheel_number": 0}] at "/usb_devices"
    And after the reload the Cameras section lists the wheel on port "simulated-usbv3-0:1" as "filter wheel 0"

  Scenario: A listed wheel's page edits the filter names in its camera's entry
    Given the configuration lists these USB devices:
      | device_number | usb_port            | filter_wheel_number |
      | 0             | simulated-usbv3-0:1 | 0                   |
    When the qhy-camera service starts
    And I open the setup page of device "filterwheel" 0
    Then the page says "Declared in its camera's row"
    When I submit the setup page of filter wheel 0 naming the filters "Ha, OIII"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds ["Ha", "OIII"] at "/usb_devices/0/filter_names"
    And after the reload the setup page of filter wheel 0 shows the filters "Ha, OIII"

  Scenario: A placeholder wheel's page names the port and why, and still edits the declared wheel's filter names
    Given the configuration lists these USB devices:
      | device_number | usb_port            | filter_wheel_number |
      | 0             | simulated-usbv3-0:9 | 0                   |
    When the qhy-camera service starts
    And I open the setup page of device "filterwheel" 0
    Then the page says "simulated-usbv3-0:9"
    And the page says "no working camera is enumerated on simulated-usbv3-0:9"
    When I submit the setup page of filter wheel 0 naming the filters "L"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds ["L"] at "/usb_devices/0/filter_names"

  Scenario: Leaving a list that declares a wheel asks first, then carries every name into the camera's override
    Given the configuration JSON {"usb_devices": [{"device_number": 0, "usb_port": "simulated-usbv3-0:1", "name": "Main", "filter_wheel_number": 0, "filter_names": ["L", "R"]}]}
    When the qhy-camera service starts
    And I submit the server setup page setting "usb_devices.mode" to "auto"
    Then the page asks to confirm that the reload numbers the wheels by what its probe finds
    And the page warns that the rp filter wheel entries bound to those numbers must be renumbered
    And the configuration file holds "simulated-usbv3-0:1" at "/usb_devices/0/usb_port"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file has nothing at "/usb_devices"
    And the configuration file holds {"name": "Main", "filter_names": ["L", "R"]} at "/devices/SIM-QHY178M"
    And after the reload the Cameras section's mode is "auto"
    And the running service registers 1 Camera device and 1 FilterWheel device

  Scenario: Leaving a list that declares no wheel carries the camera's names without asking
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
    When the qhy-camera service starts
    And I submit the server setup page setting "usb_devices.mode" to "auto"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file has nothing at "/usb_devices"
    And the configuration file holds "Main" at "/devices/SIM-QHY178M/name"

  Scenario: Unchecking a camera moves the numbers after it, once the move is confirmed
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
      | 1             | simulated-usbv3-0:1 |
    When the qhy-camera service starts
    And I submit the server setup page unchecking the row for port "simulated-usbv3-0:9"
    Then the page asks to confirm that the camera on "simulated-usbv3-0:1" moves from 1 to 0
    And the page warns that the rp camera entries bound to those numbers must be renumbered
    And the configuration file holds "simulated-usbv3-0:9" at "/usb_devices/0/usb_port"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [{"device_number": 0, "usb_port": "simulated-usbv3-0:1"}] at "/usb_devices"
    And within 30 seconds camera device 0 reports the UniqueID "SIM-QHY178M"

  Scenario: Unchecking every row asks before dropping the names, saves an empty list, and the server page is still served
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
    When the qhy-camera service starts
    And I submit the server setup page unchecking the row for port "simulated-usbv3-0:1"
    Then the page asks to confirm dropping the names of "simulated-usbv3-0:1"
    And the configuration file holds "Main" at "/usb_devices/0/name"
    When I save the confirmation
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds [] at "/usb_devices"
    And after the reload the Cameras section lists the port "simulated-usbv3-0:1" as "not served"
    And the running service registers 0 Camera devices and 0 FilterWheel devices

  Scenario: A listed camera's page shows what the driver knows and links to its row
    Given the configuration lists these USB devices:
      | device_number | usb_port            | name |
      | 0             | simulated-usbv3-0:1 | Main |
    When the qhy-camera service starts
    And I open the setup page of device "camera" 0
    Then the page says "simulated-usbv3-0:1"
    And the page says "SIM-QHY178M"
    And the page shows no input named "override.name"
    And the page offers no Save button
    And the page links to the Cameras section of the server page

  Scenario: A placeholder's page names the port it stands in for and why, and links to the Cameras section
    Given the configuration lists these USB devices:
      | device_number | usb_port            |
      | 0             | simulated-usbv3-0:9 |
    When the qhy-camera service starts
    And I open the setup page of device "camera" 0
    Then the page says "simulated-usbv3-0:9"
    And the page says "no working camera is enumerated on simulated-usbv3-0:9"
    And the page offers no Save button
    And the page links to the Cameras section of the server page

  Scenario: The config actions are gone
    Given a running qhy-camera service with the simulation backend
    When the supported actions are queried on camera device 0
    Then the supported actions are empty
    When the action "config.get" is called on camera device 0
    Then the call should fail with an action-not-implemented error

  Scenario: A save from another site is refused and the file is untouched
    Given a running qhy-camera service with the simulation backend
    When I submit the server setup page from the origin "http://evil.example" setting "server.bind_address" to "127.0.0.1"
    Then the response status is 403
    And the configuration file has nothing at "/server/bind_address"

  Scenario: The server's port moves from the page, which names its new address
    Given a running qhy-camera service with the simulation backend
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: With auth configured the pages answer 401 without a credential and render with one
    Given the qhy-camera service is running with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the response status is 200
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash
