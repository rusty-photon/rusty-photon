Feature: ui-htmx's setup page
  The BFF's own configuration is edited on its own /setup, the shared setup
  crate's server page over the BFF's config: the listening server and where
  rp is (docs/services/ui-htmx.md, "Setup page"). The BFF has no in-process
  reload, so a save takes effect at the next start, and the page says how to
  restart. The page reads no rp: these scenarios point the BFF's rp block at
  an address nothing answers on.

  Scenario: The page shows where rp is
    Given ui-htmx is started on a temp config file for its setup page
    When I open the server setup page
    Then the response status is 200
    And the page shows an input named "rp.base_url" with value "http://127.0.0.1:1"

  Scenario: A changed rp address is saved and waits for the restart
    Given ui-htmx is started on a temp config file for its setup page
    When I submit the server setup page setting "rp.base_url" to "http://rig.local:11115"
    Then the page reports the configuration was saved and "rp.base_url" waits for the restart
    And the configuration file holds "http://rig.local:11115" at "/rp/base_url"

  Scenario: An rp address carrying credentials is refused, the file untouched
    Given ui-htmx is started on a temp config file for its setup page
    When I submit the server setup page setting "rp.base_url" to "http://obs:secret@127.0.0.1:11115"
    Then the page answers 422 with the error "must not carry credentials" on "rp.base_url"
    And the configuration file holds "http://127.0.0.1:1" at "/rp/base_url"

  # The credential the BFF presents to rp is an optional block the page does
  # not render: it is edited in the file, as the drivers' own rp credential
  # blocks are, and its password never reaches the page.
  Scenario: The credential the BFF presents to rp stays in the file, its password off the page
    Given ui-htmx is started for its setup page presenting rp the username "observatory" with the password "hunter2"
    When I open the server setup page
    Then the page names "rp.auth" as edited in the configuration file
    And the setup page does not contain "hunter2"

  # The credential is bound to rp's address and to the CA that verifies rp
  # there: a page anyone reaching the BFF can save must not send rp's
  # password to a server typed on it. A save that would asks first, and
  # keeps the credential only when its password is typed again.
  Scenario: Moving rp elsewhere asks before removing the credential the BFF presented to it
    Given ui-htmx is started for its setup page presenting rp the username "observatory" with the password "hunter2"
    When I submit the server setup page setting "rp.base_url" to "http://elsewhere.example:11115"
    Then the page asks to confirm removing "rp.auth"
    And the configuration file holds "http://127.0.0.1:1" at "/rp/base_url"
    When I save the confirmation
    Then the page reports the configuration was saved and "rp.base_url" waits for the restart
    And the page says "Removed from the file: rp.auth, since rp.base_url changed"
    And the configuration file has nothing at "/rp/auth"

  Scenario: Trusting another CA for rp asks before removing the credential the BFF presented to it
    Given ui-htmx is started for its setup page presenting rp the username "observatory" with the password "hunter2"
    When I submit the server setup page setting "rp.ca_cert_path" to "/etc/rusty-photon/ca.pem"
    Then the page asks to confirm removing "rp.auth"
    When I save the confirmation
    Then the page reports the configuration was saved and "rp.ca_cert_path" waits for the restart
    And the page says "Removed from the file: rp.auth, since rp.ca_cert_path changed"
    And the configuration file has nothing at "/rp/auth"

  Scenario: rp's password typed on the confirmation keeps the credential for the new address
    Given ui-htmx is started for its setup page presenting rp the username "observatory" with the password "hunter2"
    When I submit the server setup page setting "rp.base_url" to "http://rig.example:11115"
    And I save the confirmation typing "hunter2" as the password that keeps "rp.auth"
    Then the page reports the configuration was saved and "rp.base_url" waits for the restart
    And the configuration file holds "http://rig.example:11115" at "/rp/base_url"
    And the configuration file holds "hunter2" at "/rp/auth/password"

  Scenario: A wrong password on the confirmation is refused, the file untouched
    Given ui-htmx is started for its setup page presenting rp the username "observatory" with the password "hunter2"
    When I submit the server setup page setting "rp.base_url" to "http://elsewhere.example:11115"
    And I save the confirmation typing "not-it" as the password that keeps "rp.auth"
    Then the page answers 422 with the error "does not match the stored password" on "keep-credential:rp.auth"
    And the configuration file holds "http://127.0.0.1:1" at "/rp/base_url"
    And the configuration file holds "hunter2" at "/rp/auth/password"

  Scenario: A save that leaves rp's address and CA keeps the credential
    Given ui-htmx is started for its setup page presenting rp the username "observatory" with the password "hunter2"
    When I submit the server setup page setting "server.bind_address" to "0.0.0.0"
    Then the page reports the configuration was saved and "server.bind_address" waits for the restart
    And the configuration file holds "observatory" at "/rp/auth/username"
    And the configuration file holds "hunter2" at "/rp/auth/password"

  Scenario: A save posted from another site is refused
    Given ui-htmx is started on a temp config file for its setup page
    When I submit the server setup page from the origin "https://elsewhere.example" setting "rp.base_url" to "http://rig.local:11115"
    Then the response status is 403
    And the configuration file holds "http://127.0.0.1:1" at "/rp/base_url"
