Feature: Setup page
  The guider configures itself on one HTML page, /setup, served by its
  service listener — the one its REST API and /health use, behind the same
  TLS and Basic Auth. The page covers the whole config, the camera facade's
  block included. GET answers with a form filled from the configuration
  file, POST with a save that overlays the submitted fields onto the file,
  validates and persists. The service has no reload path, so a saved change
  waits for the next start: the saved page names the fields and says how to
  restart the service, and the page names them until the restart has run
  them. Enabling the facade is held to the rules a start holds it to, its
  image directory tested by the service itself, so a save never leaves a
  file the next start would refuse; a zero timeout or interval is refused
  beside its field. The facade's UniqueID sits under the Identity
  disclosure, and its listener's TLS and auth blocks are edited in the
  configuration file. The facade's own Alpaca server listens on loopback
  alone, so both of its setup paths send the browser to the guider's page,
  and its Actions carry no configuration. A POST that does not come from
  the page itself is refused with 403. With auth configured the page
  answers 401 without a credential and renders with one, never carrying the
  stored hash.

  Scenario: The page shows the whole config, the facade's block included
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I open the server setup page
    Then the page shows an input named "phd2.host" with value "127.0.0.1"
    And the page shows an input named "settling.pixels" with value "0.5"
    And the page shows an input named "stop_timeout" with value "10s"
    And the page shows an input named "server.port" with value "0"
    And the page shows an input named "camera.capture_grace" with value "10s"
    And the page shows an input named "camera.pixel_size_x_um" with value ""
    And the page shows an input named "camera.server.port" with value "11128"
    And the page shows an input named "camera.server.bind_address" with value "127.0.0.1"
    And the page names "camera.server.auth" as edited in the configuration file
    And the page names "phd2.spawn_env" as edited in the configuration file
    And the page names nothing as waiting for the restart

  Scenario: A saved change waits for the restart, and the page says how to restart
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I submit the server setup page setting "settling.pixels" to "0.8"
    Then the page reports the configuration was saved and "settling.pixels" waits for the restart
    And the page says "sudo systemctl restart rusty-photon-phd2-guider"
    And the configuration file holds 0.8 at "/settling/pixels"
    When I open the server setup page
    Then the page shows an input named "settling.pixels" with value "0.8"
    And the page names "settling.pixels" as waiting for the restart
    When the guider service is restarted on its configuration file
    And I open the server setup page
    Then the page shows an input named "settling.pixels" with value "0.8"
    And the page names nothing as waiting for the restart

  Scenario: A save that changes nothing says so
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I submit the server setup page setting "stop_timeout" to "10s"
    Then the page reports the configuration was saved and nothing changed

  Scenario: A zero reconnect interval is refused beside its field
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I submit the server setup page setting "phd2.reconnect.interval" to "0s"
    Then the page answers 422 with the error "must be greater than 0" on "phd2.reconnect.interval"
    And the configuration file holds "200ms" at "/phd2/reconnect/interval"

  Scenario: Enabling the facade without its fields is refused naming each
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I submit the server setup page setting "camera.enabled" to "on"
    Then the page answers 422 with the error "is required when camera.enabled is true" on "camera.pixel_size_x_um"
    And the page answers 422 with the error "is required when camera.enabled is true" on "camera.image_dir"
    And the configuration file has nothing at "/camera"

  Scenario: An image directory the service cannot use is refused beside its field
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I submit the server setup page enabling the camera facade on a missing image directory
    Then the page answers 422 with the error "cannot be used" on "camera.image_dir"
    And the configuration file has nothing at "/camera"

  Scenario: A facade listener beyond loopback is refused naming the field
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I submit the server setup page setting "camera.server.bind_address" to "0.0.0.0"
    Then the page reports the submitted values were rejected naming "camera.server.bind_address"
    And the configuration file has nothing at "/camera"

  Scenario: The facade enabled on the page serves after the restart under the UniqueID it mints
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running
    When I submit the server setup page enabling the camera facade on its image directory
    Then the page reports the configuration was saved and "camera" waits for the restart
    And the configuration file holds true at "/camera/enabled"
    When the guider service is restarted on its configuration file
    Then the config file should hold a camera unique_id
    And the facade camera's UniqueID should be the config file's camera unique_id
    When I open the server setup page
    Then the page shows "camera.unique_id" under the Identity disclosure

  Scenario: The facade's setup paths send the browser to the guider's page
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    When I open the facade's server setup page
    Then the facade sends the browser to the guider's server setup page
    When I open the facade's setup page of device "camera" 0
    Then the facade sends the browser to the guider's server setup page

  Scenario: The facade's Actions carry no configuration
    Given a mock PHD2 2.6.14 with an 8 x 6 camera
    And the guider service is running with the camera facade enabled
    Then the facade's supported actions should be the sweep lease's alone
    When the client calls the facade's Action "config.get"
    Then the facade call should fail with ASCOM error 0x40C

  Scenario: A server port change names the new address beside the restart
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I submit the server setup page moving "server.port" to a free port
    Then the page reports the configuration was saved and "server.port" waits for the restart
    And the page names the server setup page at the new port

  Scenario: A cross-site POST is refused
    Given a mock PHD2 that settles successfully
    And the guider service is running
    When I submit the server setup page from the origin "https://elsewhere.example" setting "settling.pixels" to "0.9"
    Then the response status is 403
    And the configuration file holds 0.5 at "/settling/pixels"

  Scenario: No password hash reaches the page, and the page is behind the credential
    Given a mock PHD2 that settles successfully
    And the guider service is running with TLS and auth
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the page shows an input named "auth.username" with value "observatory"
    And the page shows an empty password input named "auth.password"
    And the page does not contain the stored password hash
