Feature: Setup pages
  The camera configures itself through the two HTML pages the Alpaca
  Management API defines, served by the service itself: /setup for the whole
  server and /setup/v1/camera/0/setup for the camera. Both answer GET with a
  form filled from the configuration file and POST with a save that overlays
  the submitted fields onto that page's section of the file, validates,
  persists, and reloads the service in process. A save of one page never
  alters the other page's section. The follow-mode blocks, pointing.telescope
  and pointing.rotator, are optional objects no form control renders: the
  server page names them as edited in the configuration file, and a save
  leaves them, and the client passwords they may carry, as the file has
  them. A value a control cannot hold, letters in a number, is refused beside
  its field, and so is a value the camera cannot work with: a focal length,
  pixel size, sensor size or survey timeout of zero, the rules the file load
  applies. The binary takes no command-line overrides, so no field is ever
  pinned. No password hash reaches a page. A POST that does not come from the
  page itself is refused with 403 and the file is untouched. With auth
  configured the pages answer 401 without a credential and render with one.
  The config.get, config.apply and config.schema actions are gone:
  SupportedActions is empty.

  Background:
    Given a sky-survey-camera with default optics
    And SkyView is reachable

  Scenario: The server page shows the optics, pointing and survey sections and links to the camera's page
    Given the service is running
    When I open the server setup page
    Then the page shows an input named "optics.focal_length_mm" with value "1000.0"
    And the page shows an input named "optics.sensor_width_px" with value "640"
    And the page shows an input named "survey.name" with value "DSS2 Red"
    And the page shows an input named "survey.request_timeout" with value "5s"
    And the page names "pointing.telescope" as edited in the configuration file
    And the page names "pointing.rotator" as edited in the configuration file
    And the page shows no input named "device.name"
    And the page links to the setup page of device "camera" 0

  Scenario: The camera's page shows only its own section
    Given the service is running
    When I open the setup page of device "camera" 0
    Then the page shows an input named "device.name" with value "Test Sky Survey Camera"
    And the page shows an input named "device.unique_id" with value "sky-survey-camera-test-001"
    And the page shows no input named "optics.focal_length_mm"

  Scenario: A valid save is persisted, reloaded, and served back after the wait
    Given the service is running
    And the bound port is pinned in the configuration file so a reload keeps the address
    When I submit the setup page of device "camera" 0 setting "device.description" to "Renamed Camera"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "Renamed Camera" at "/device/description"
    And the configuration file holds "DSS2 Red" at "/survey/name"
    And after the reload the setup page of device "camera" 0 shows "device.description" as "Renamed Camera"

  Scenario: An invalid save names the field and leaves the file untouched
    Given the service is running
    When I submit the setup page of device "camera" 0 setting "device.unique_id" to ""
    Then the page answers 422 with the error "must not be empty" on "device.unique_id"
    And the configuration file holds "sky-survey-camera-test-001" at "/device/unique_id"

  Scenario: A value a control cannot hold is refused beside its field
    Given the service is running
    When I submit the server setup page setting "optics.focal_length_mm" to "fast"
    Then the page answers 422 with the error "must be a number" on "optics.focal_length_mm"
    And the configuration file holds 1000.0 at "/optics/focal_length_mm"

  Scenario: A zero focal length or survey timeout is refused beside its field
    Given the service is running
    When I submit the server setup page setting "optics.focal_length_mm" to "0"
    Then the page answers 422 with the error "must be greater than 0" on "optics.focal_length_mm"
    And the configuration file holds 1000.0 at "/optics/focal_length_mm"
    When I submit the server setup page setting "survey.request_timeout" to "0s"
    Then the page answers 422 with the error "must be greater than 0" on "survey.request_timeout"
    And the configuration file holds "5s" at "/survey/request_timeout"

  Scenario: A save leaves the follow-mode block as the file has it
    The telescope block is edited in the file: the server page names it, and
    a save of the camera's page carries nothing of it.
    Given a mount reports RA 10.0 hours and Dec 30.0 degrees
    And the camera is configured to follow that mount
    And the service is running
    When I open the server setup page
    Then the page names "pointing.telescope" as edited in the configuration file
    When I submit the setup page of device "camera" 0 setting "device.description" to "Follows the mount"
    Then the page reports the configuration was saved and the service is reloading
    And the configuration file holds "Follows the mount" at "/device/description"
    And the configuration file holds 0 at "/pointing/telescope/device_number"
    And the configuration file holds "2s" at "/pointing/telescope/request_timeout"

  Scenario: A save that changes nothing needs no reload
    Given the service is running
    When I submit the server setup page setting "survey.name" to "DSS2 Red"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A cross-site POST is refused
    Given the service is running
    When I submit the server setup page from the origin "https://elsewhere.example" setting "survey.name" to "DSS2 Blue"
    Then the response status is 403
    And the configuration file holds "DSS2 Red" at "/survey/name"

  Scenario: A server port change answers with the new address
    Given the service is running
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and the service is reloading
    And the page names the server setup page at the new port

  Scenario: No password hash reaches a page, and the pages are behind the credential
    Given the service is running with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash

  Scenario: The configuration actions are gone
    Given the service is running
    When the supported actions are queried
    Then the supported actions are empty
    When the action "config.get" is called
    Then the call should fail with an action-not-implemented error
