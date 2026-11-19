Feature: TLS material and platform default diagnosis
  A server.tls block whose cert or key path does not exist means the service
  will refuse to serve at next start — a fail. server.auth without
  server.tls is legal but puts HTTP Basic credentials on the wire in
  cleartext, so it warns (the services' auth scheme is Basic over TLS). And
  rp self-creates a session.data_directory default that is not valid on
  every platform: a directory that does not exist is a fail because session
  state cannot persist.

  Background:
    Given platform facts with enabled units:
      | unit                     |
      | rusty-photon-qhy-focuser |
      | rusty-photon-rp          |

  Scenario: A TLS cert path that does not exist is a fail
    Given a config directory with "qhy-focuser.json" containing:
      """
      { "server": { "port": 11113,
          "tls": { "cert": "{missing}/cert.pem", "key": "{missing}/key.pem" } } }
      """
    When I run doctor with --json
    Then doctor exits with code 1
    And the report contains a "fail" check named "tls.paths" for service "qhy-focuser"
    And that check's detail mentions "{missing}/cert.pem"

  Scenario: TLS material that exists passes
    Given a config directory containing PEM files "cert.pem" and "key.pem"
    And a config file "qhy-focuser.json" with a tls block pointing at those PEM files on port 11113
    When I run doctor with --json
    Then the report contains an "ok" check named "tls.paths" for service "qhy-focuser"

  Scenario: Relative TLS paths cannot be judged and warn
    Given a config directory with "qhy-focuser.json" containing:
      """
      { "server": { "port": 11113,
          "tls": { "cert": "pki/qhy-focuser.pem", "key": "pki/qhy-focuser-key.pem" } } }
      """
    When I run doctor with --json
    Then the report contains a "warn" check named "tls.paths" for service "qhy-focuser"
    And that check's detail mentions "working directory"

  Scenario: Auth without TLS warns about cleartext credentials
    Given a config directory with "qhy-focuser.json" containing:
      """
      { "server": { "port": 11113,
          "auth": { "username": "observatory", "password_hash": "$argon2id$stub" } } }
      """
    When I run doctor with --json
    Then the report contains a "warn" check named "tls.auth-without-tls" for service "qhy-focuser"

  Scenario: Auth over TLS does not warn
    Given a config directory containing PEM files "cert.pem" and "key.pem"
    And a config file "qhy-focuser.json" with tls and auth blocks pointing at those PEM files on port 11113
    When I run doctor with --json
    Then the report has no checks named "tls.auth-without-tls"

  Scenario: An rp data_directory that does not exist is a fail
    Given a config directory with "rp.json" containing:
      """
      { "server": { "port": 11115 },
        "session": { "data_directory": "{missing}/rusty-photon-data" } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "rp.data-directory" for service "rp"
    And that check's detail mentions "{missing}/rusty-photon-data"

  Scenario: An rp data_directory that exists passes
    Given a config directory with an existing data directory
    And a config file "rp.json" with session.data_directory pointing at that data directory on port 11115
    When I run doctor with --json
    Then the report contains an "ok" check named "rp.data-directory" for service "rp"
