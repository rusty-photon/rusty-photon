Feature: The guider's setup page
  The guider configures itself on one page, /setup on its own listener,
  behind the same TLS and Basic Auth as its API: the shared setup crate's
  server page over the guider's configuration (docs/services/guider.md,
  "Setup page"). The address of the camera, the address of the mount and
  the focal length are all a rig has to supply. The guider has no
  in-process reload, so a save takes effect at the next start, and the
  page says how to restart. A save never reaches a device: these
  scenarios name devices at an address nothing answers on.

  Scenario: The page shows the devices and the focal length still to be named
    Given the guider is started for its setup page
    When I open the server setup page
    Then the response status is 200
    And the page shows an input named "devices.camera.alpaca_url" with value ""
    And the page shows an input named "devices.camera.device_number" with value "0"
    And the page shows an input named "devices.mount.alpaca_url" with value ""
    And the page shows an input named "optics.focal_length_mm" with value ""
    And the page shows a dropdown named "control.dec.mode" offering "auto,north,south,off" with "auto" chosen
    And the page says "A restart of the guider ends a guiding run"

  Scenario: A camera address is saved, waits for the restart, and reaches no device before it
    Given the guider is started for its setup page
    When I submit the server setup page setting "devices.camera.alpaca_url" to "http://127.0.0.1:11121"
    Then the page reports the configuration was saved and "devices.camera.alpaca_url" waits for the restart
    And the configuration file holds "http://127.0.0.1:11121" at "/devices/camera/alpaca_url"
    And the devices answer reports the camera as null

  Scenario: A focal length is saved as a number
    Given the guider is started for its setup page
    When I submit the server setup page setting "optics.focal_length_mm" to "200"
    Then the page reports the configuration was saved and "optics.focal_length_mm" waits for the restart
    And the configuration file holds 200.0 at "/optics/focal_length_mm"

  # The form holds a number to the bounds its field's schema states, and
  # names the field it refuses.
  Scenario: A number outside its field's bounds is refused beside the field, the file untouched
    Given the guider is started for its setup page
    When I submit the server setup page setting "camera.binning" to "0"
    Then the page answers 422 with the error "must be a whole number between 1 and 255" on "camera.binning"
    And the configuration file has nothing at "/camera/binning"

  Scenario: A state directory the guider could not write to is refused beside the field
    Given the guider is started for its setup page
    When I submit the server setup page setting "state_dir" to a directory under a file
    Then the page answers 422 with the error "cannot be created" on "state_dir"

  # A value the configuration refuses as it is read is refused before the
  # form knows which field it was, so it is named above the form, in a
  # message that names the field.
  Scenario Outline: A value the configuration refuses as it is read is named above the form
    Given the guider is started for its setup page
    When I submit the server setup page setting "<field>" to "<value>"
    Then the page reports the submitted values were rejected naming "<message>"
    And the configuration file has nothing at "/devices/camera/alpaca_url"

    Examples:
      | field                     | value           | message                                                   |
      | camera.exposure           | 2 parsecs       | camera.exposure is not a duration                         |
      | devices.camera.alpaca_url | ftp://camera:1  | devices.camera.alpaca_url must be an http:// or https:// URL |
      | mount.min_pulse           | 3s              | mount.min_pulse must be shorter than mount.max_pulse      |

  # The device credentials and the listener's TLS are edited in the file,
  # where doctor writes them, and a stored password never reaches the page.
  Scenario: The device credentials and the listener's TLS stay in the file, their passwords off the page
    Given the guider is started for its setup page naming a camera and a mount whose servers take the username "observatory" and the password "hunter2"
    When I open the server setup page
    Then the page names "devices.camera.auth" as edited in the configuration file
    And the page names "devices.mount.auth" as edited in the configuration file
    And the page names "server.tls" as edited in the configuration file
    And the setup page does not contain "hunter2"

  # A device's credential is bound to its server's address and to the CA
  # that verifies the server there: a page anyone reaching the guider can
  # save must not send a server's password to an address typed on it.
  Scenario: Moving the camera elsewhere asks before removing its credential, and keeps the mount's
    Given the guider is started for its setup page naming a camera and a mount whose servers take the username "observatory" and the password "hunter2"
    When I submit the server setup page setting "devices.camera.alpaca_url" to "http://elsewhere.example:11121"
    Then the page asks to confirm removing "devices.camera.auth"
    And the configuration file holds "observatory" at "/devices/camera/auth/username"
    When I save the confirmation
    Then the page reports the configuration was saved and "devices.camera.alpaca_url" waits for the restart
    And the page says "Removed from the file: devices.camera.auth, since devices.camera.alpaca_url changed"
    And the configuration file has nothing at "/devices/camera/auth"
    And the configuration file holds "hunter2" at "/devices/mount/auth/password"

  Scenario: Trusting another CA asks before removing both device credentials
    Given the guider is started for its setup page naming a camera and a mount whose servers take the username "observatory" and the password "hunter2"
    When I submit the server setup page setting "ca_cert" to the suite's CA certificate
    Then the page asks to confirm removing "devices.camera.auth"
    And the page asks to confirm removing "devices.mount.auth"
    When I save the confirmation
    Then the page reports the configuration was saved and "ca_cert" waits for the restart
    And the configuration file has nothing at "/devices/camera/auth"
    And the configuration file has nothing at "/devices/mount/auth"

  Scenario: A save that leaves the device addresses and the CA keeps both credentials
    Given the guider is started for its setup page naming a camera and a mount whose servers take the username "observatory" and the password "hunter2"
    When I submit the server setup page setting "settling.pixels" to "0.8"
    Then the page reports the configuration was saved and "settling.pixels" waits for the restart
    And the configuration file holds "hunter2" at "/devices/camera/auth/password"
    And the configuration file holds "hunter2" at "/devices/mount/auth/password"

  Scenario: Behind the guider's own login the page asks for the credential
    Given the guider is started behind its own login for its setup page
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the response status is 200
    And the page does not contain the stored password hash

  Scenario: A save posted from another site is refused
    Given the guider is started for its setup page
    When I submit the server setup page from the origin "https://elsewhere.example" setting "devices.camera.alpaca_url" to "http://127.0.0.1:11121"
    Then the response status is 403
    And the configuration file has nothing at "/devices/camera/alpaca_url"
