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

  Scenario: A save posted from another site is refused
    Given ui-htmx is started on a temp config file for its setup page
    When I submit the server setup page from the origin "https://elsewhere.example" setting "rp.base_url" to "http://rig.local:11115"
    Then the response status is 403
    And the configuration file holds "http://127.0.0.1:1" at "/rp/base_url"
