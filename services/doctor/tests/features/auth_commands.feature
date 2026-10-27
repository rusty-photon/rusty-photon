Feature: Credential commands
  doctor auth hash-password hashes one password for hand-written configs
  (the third-party-driver escape hatch). doctor auth rotate mints a fresh
  observatory credential, overwrites the canonical pki/credential copy,
  and re-runs the distribution: the Argon2id hash into every installed
  service's server.auth, the plaintext into every client auth block —
  including blocks an earlier credential already occupied, which is what
  distinguishes rotate from the never-overwriting --fix pass. The client
  blocks are the shared service_auth pairs (sentinel, the MCP clients,
  planetarium-bridge's rp block) and every per-target credential that
  names a local service: rp's equipment and plugins, ui-htmx's rp target,
  sentinel's monitors. A credential for a host that is no local service
  is the operator's, and stays as it is.

  Scenario: hash-password reads stdin and prints an Argon2id hash
    When I run doctor auth hash-password with "correct horse battery staple" on stdin
    Then doctor exits with code 0
    And stdout starts with "$argon2id$"

  Scenario: rotate mints a new credential and re-aligns every copy
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 } }
      """
    And doctor has already run with --fix
    When I run doctor auth rotate
    Then the pki file "credential" has changed
    And the auth hash at "/server/auth/password_hash" in "ppba-driver.json" verifies against the credential file
    And the sentinel client auth password verifies against the auth hash in "ppba-driver.json"

  Scenario: hash-password rejects an empty password
    When I run doctor auth hash-password with empty input on stdin
    Then the command exits with a non-zero status
    And stderr contains "password must not be empty"

  Scenario: rotate reports the re-mint as JSON
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And doctor has already run with --fix
    When I run doctor auth rotate with --json
    Then the report records an applied "mint-credential" provisioning action
    And the pki file "credential" has changed

  Scenario: rotate re-aligns every client credential that names a local service
    Given a config file "zwo-camera.json" containing:
      """
      { "server": { "port": 11122 } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 },
        "equipment": { "cameras": [ { "id": "main", "alpaca_url": "http://localhost:11122",
                                      "auth": { "username": "observatory", "password": "old-password" } } ] } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 },
        "rp": { "base_url": "http://127.0.0.1:11115",
                "auth": { "username": "observatory", "password": "old-password" } } }
      """
    And a config file "session-runner.json" containing:
      """
      { "server": { "port": 11171 },
        "service_auth": { "username": "observatory", "password": "old-password" } }
      """
    And a config file "planetarium-bridge.json" containing:
      """
      { "server": { "port": 11126 },
        "rp": { "service_auth": { "username": "observatory", "password": "old-password" } } }
      """
    And doctor has already run with --fix
    When I run doctor auth rotate
    Then the client password at "/equipment/cameras/0/auth/password" in "rp.json" verifies against the auth hash in "zwo-camera.json"
    And the client password at "/rp/auth/password" in "ui-htmx.json" verifies against the auth hash in "rp.json"
    And the client password at "/service_auth/password" in "session-runner.json" verifies against the auth hash in "rp.json"
    And the client password at "/rp/service_auth/password" in "planetarium-bridge.json" verifies against the auth hash in "rp.json"

  Scenario: rotate leaves a credential for a host that is no local service as it is
    Given a config file "zwo-camera.json" containing:
      """
      { "server": { "port": 11122 } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 },
        "equipment": { "cameras": [ { "id": "main", "alpaca_url": "http://camera.example:11122",
                                      "auth": { "username": "camera-owner", "password": "their-password" } } ] } }
      """
    When I run doctor auth rotate
    Then the config file "rp.json" has the string "camera-owner" at "/equipment/cameras/0/auth/username"
    And the config file "rp.json" has the string "their-password" at "/equipment/cameras/0/auth/password"

  Scenario: rotate repairs a mismatched client credential
    Given a config file "ppba-driver.json" whose auth hash is of the password "right-password"
    And a config file "sentinel.json" whose client auth block carries the password "wrong-password"
    When I run doctor auth rotate
    And I run doctor with --json
    Then the report has no checks named "auth.mismatch"
    And the auth hash at "/server/auth/password_hash" in "ppba-driver.json" verifies against the credential file
