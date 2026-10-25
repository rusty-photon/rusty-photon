Feature: rp's setup page
  rp edits its own configuration on its own page, /setup, behind the same
  TLS and Basic Auth as every other route: GET answers the form, POST saves
  it. Beside rp's own sections the page carries the equipment roster, one
  row per entry with its address, its live state, the credential rp
  presents to its driver, and links to the driver's server page
  (<alpaca_url>/setup) and the device's own page
  (<alpaca_url>/setup/v1/<type>/<number>/setup). An entry that names its
  driver by a loopback address links it at the host the browser reached rp
  by; any other host is linked as written. The camera that is phd2-guider's
  facade links the guider's setup page instead, at the mount's guiding
  address.

  A row's fields are named by the entry's place in the file
  (equipment.cameras.0.gain). A row's Remove box takes the entry out, and
  each kind's Add block adds an entry when its id is filled. A save drawn
  before the file's roster changed is refused rather than written onto the
  wrong entry. A credential's password never reaches a page: blank keeps
  the stored one while the entry's address stays as it is, a username
  needs a password, and clearing both removes the credential. The CA rp
  trusts for every credential it presents is edited in the file, not on
  the page.

  rp applies a saved change at its next start, so the saved page names the
  fields that wait for the restart. These scenarios start rp on a temp
  configuration whose entries point at addresses nothing answers on, so
  every entry reads as not connected; no simulator is needed.

  Scenario: The page shows rp's own settings and each entry's fields
    Given a temp rp config whose roster holds:
      | kind     | id          | alpaca_url                   | device_number |
      | cameras  | main-cam    | http://127.0.0.1:1           | 0             |
      | focusers | far-focuser | http://focuslynx.invalid:4247 | 1             |
    And rp is started with that config file
    When I open the server setup page
    Then the page shows an input named "imaging.cache_max_mib" with value "1024"
    And the page shows an input named "equipment.cameras.0.id" with value "main-cam"
    And the page shows an input named "equipment.focusers.0.device_number" with value "1"
    And the roster row "main-cam" reads "not connected"

  Scenario: A loopback entry links its driver at the host the browser used, another host as written
    Given a temp rp config whose roster holds:
      | kind     | id          | alpaca_url                   | device_number |
      | cameras  | main-cam    | http://127.0.0.1:1           | 0             |
      | focusers | far-focuser | http://focuslynx.invalid:4247 | 1             |
    And rp is started with that config file
    When I open rp's setup page reached as "rig.example"
    Then the roster row "main-cam" links its driver's setup page at "http://rig.example:1/setup"
    And the roster row "main-cam" links its device's page at "http://rig.example:1/setup/v1/camera/0/setup"
    And the roster row "far-focuser" links its driver's setup page at "http://focuslynx.invalid:4247/setup"
    And the roster row "far-focuser" links its device's page at "http://focuslynx.invalid:4247/setup/v1/focuser/1/setup"

  Scenario: The camera that is phd2-guider's facade links the guider's setup page
    Given a temp rp config whose camera "guide-cam" is the stub camera facade and whose mount guides through "http://127.0.0.1:11130"
    And rp is started with that config file
    When I open rp's setup page reached as "rig.example"
    Then the roster row "guide-cam" links the guider's setup page at "http://rig.example:11130/setup"

  Scenario: A changed entry field is saved and waits for rp's restart
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And rp is started with that config file
    When I submit the server setup page setting "equipment.cameras.0.gain" to "120"
    Then the page reports the configuration was saved and "equipment.cameras" waits for the restart
    And the configuration file holds 120 at "/equipment/cameras/0/gain"

  Scenario: An entry whose Remove box is checked leaves the roster
    Given a temp rp config whose roster holds:
      | kind    | id        | alpaca_url         | device_number |
      | cameras | main-cam  | http://127.0.0.1:1 | 0             |
      | cameras | guide-cam | http://127.0.0.1:2 | 0             |
    And rp is started with that config file
    When I submit the server setup page setting "remove:equipment.cameras.0" to "on"
    Then the page reports the configuration was saved and "equipment.cameras" waits for the restart
    And the configuration file holds "guide-cam" at "/equipment/cameras/0/id"
    And the configuration file has nothing at "/equipment/cameras/1"

  Scenario: An Add block whose id is filled adds an entry, which connects after the restart
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And rp is started with that config file
    When I submit the server setup page with:
      | add.focusers.id            | guide-focuser      |
      | add.focusers.alpaca_url    | http://127.0.0.1:4 |
      | add.focusers.device_number | 2                  |
    Then the page reports the configuration was saved and "equipment.focusers" waits for the restart
    And the configuration file holds "guide-focuser" at "/equipment/focusers/0/id"
    And the configuration file holds 2 at "/equipment/focusers/0/device_number"
    When I open the server setup page
    Then the roster row "guide-focuser" reads "connects after rp restarts"

  Scenario: An entry's stored password never reaches the page, and a blank one keeps it
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And the entry "main-cam" presents the username "observatory" with the password "hunter2"
    And rp is started with that config file
    When I open the server setup page
    Then the page shows an empty password input named "equipment.cameras.0.auth.password"
    And the page shows an input named "equipment.cameras.0.auth.username" with value "observatory"
    And the page does not contain "hunter2"
    When I submit the server setup page setting "equipment.cameras.0.auth.username" to "operator"
    Then the configuration file holds "operator" at "/equipment/cameras/0/auth/username"
    And the configuration file holds "hunter2" at "/equipment/cameras/0/auth/password"

  Scenario: A username with no password stored is refused, the file untouched
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And rp is started with that config file
    When I submit the server setup page setting "equipment.cameras.0.auth.username" to "observatory"
    Then the page answers 422 with the error "is needed with a username: none is stored" on "equipment.cameras.0.auth.password"
    And the configuration file has nothing at "/equipment/cameras/0/auth"

  # A stored password goes only to the address it was set for: a page
  # anyone reaching rp could save must not send it to an address typed on it.
  Scenario: Moving an entry elsewhere asks for its password again, the file untouched
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And the entry "main-cam" presents the username "observatory" with the password "hunter2"
    And rp is started with that config file
    When I submit the server setup page setting "equipment.cameras.0.alpaca_url" to "http://elsewhere.example:11121"
    Then the page answers 422 with the error "the address changed" on "equipment.cameras.0.auth.password"
    And the configuration file holds "http://127.0.0.1:1" at "/equipment/cameras/0/alpaca_url"
    And the configuration file holds "hunter2" at "/equipment/cameras/0/auth/password"

  Scenario: rp's CA is edited in the file, and a save that carries one leaves the file's
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And rp is started with that config file
    When I open the server setup page
    Then the page names "ca_cert" as edited in the configuration file
    And the page shows no input named "ca_cert"
    When I submit the server setup page setting "ca_cert" to "/etc/elsewhere/ca.pem"
    Then the page reports the configuration was saved and nothing changed
    And the configuration file has nothing at "/ca_cert"

  Scenario: Clearing the username and the password removes the credential
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And the entry "main-cam" presents the username "observatory" with the password "hunter2"
    And rp is started with that config file
    When I submit the server setup page setting "equipment.cameras.0.auth.username" to ""
    Then the page reports the configuration was saved and "equipment.cameras" waits for the restart
    And the configuration file has nothing at "/equipment/cameras/0/auth"

  Scenario: A refused save's page keeps the removal it carried, and saving it again after the fix writes both
    Given a temp rp config whose roster holds:
      | kind     | id          | alpaca_url         | device_number |
      | cameras  | main-cam    | http://127.0.0.1:1 | 0             |
      | focusers | far-focuser | http://127.0.0.1:2 | 0             |
    And rp is started with that config file
    When I submit the server setup page with:
      | remove:equipment.cameras.0          | on      |
      | equipment.focusers.0.auth.username  | someone |
    Then the response status is 422
    And the page says "Removed by this save"
    When I save the page again setting "equipment.focusers.0.auth.username" to ""
    Then the page reports the configuration was saved and "equipment.cameras" waits for the restart
    And the configuration file holds [] at "/equipment/cameras"

  Scenario: A save from a page drawn before the file's roster changed is refused
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And rp is started with that config file
    When I open the server setup page
    And the configuration file gains the camera "late-cam" behind the page's back
    And I save the page again setting "equipment.cameras.0.gain" to "120"
    Then the response status is 422
    And the page says "changed after this page was drawn"
    And the configuration file has nothing at "/equipment/cameras/0/gain"

  Scenario: A page refused as drawn from an older file shows the file's entries, and saving it writes each to its own
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
      | cameras | side-cam | http://127.0.0.1:2 | 0             |
    And the entry "side-cam" presents the username "observatory" with the password "hunter2"
    And rp is started with that config file
    When I open the server setup page
    And the configuration file gains the camera "late-cam" behind the page's back
    And I save the page again setting "remove:equipment.cameras.0" to "on"
    Then the response status is 422
    And the page says "changed after this page was drawn"
    And the page does not contain "Removed by this save"
    When I save the page again setting "equipment.cameras.0.gain" to "120"
    Then the page reports the configuration was saved and "equipment.cameras" waits for the restart
    And the configuration file holds "main-cam" at "/equipment/cameras/0/id"
    And the configuration file has nothing at "/equipment/cameras/0/auth"
    And the configuration file holds "hunter2" at "/equipment/cameras/1/auth/password"
    And the configuration file holds "late-cam" at "/equipment/cameras/2/id"

  Scenario: A save posted from another site is refused
    Given a temp rp config whose roster holds:
      | kind    | id       | alpaca_url         | device_number |
      | cameras | main-cam | http://127.0.0.1:1 | 0             |
    And rp is started with that config file
    When I submit the server setup page from the origin "https://elsewhere.example" setting "equipment.cameras.0.gain" to "120"
    Then the response status is 403
    And the configuration file has nothing at "/equipment/cameras/0/gain"

  @serial
  Scenario: Behind TLS and auth the page asks for the credential and never shows rp's stored hash
    Given generated TLS certificates
    And rp is started behind TLS and auth for its setup page
    When I open the server setup page without a credential
    Then the response status is 401
    When I open the server setup page with the credential
    Then the response status is 200
    And the page does not contain the stored password hash
