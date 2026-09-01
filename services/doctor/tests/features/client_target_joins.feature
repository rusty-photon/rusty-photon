Feature: Client-target joins (#607)
  A service's config can point a URL at another catalog service —
  ui-htmx's rp/sentinel targets, rp's plate-solver/guider clients, every
  rp equipment.<kind>[].alpaca_url entry including the singular
  equipment.mount.alpaca_url (#663), sentinel's Alpaca monitors, and
  sentinel's operation-watchdog rp_url. These checks join that URL
  against the *named* service's own
  server.tls/server.auth: a scheme mismatch, or a self-signed target the
  client has no CA-trust field for — ui-htmx's per-target ca_cert_path,
  rp's and sentinel's single top-level ca_cert — breaks every request
  (joins.client-transport, fail); a target that requires auth while the
  client carries no working credential 401s every request
  (joins.client-auth, warn). The join only resolves for a loopback host —
  doctor diagnoses one config directory, so a different host names a
  service in a config file doctor cannot see. On an ACME install the
  target's public name <svc>.<domain> joins too (#805), a target serving
  the ACME wildcard fails hostname verification for any loopback client
  host (the wildcard's only SAN is *.<domain>), and --fix moves such a
  host onto the public name — scheme and host rewrites composing into a
  single written URL — unless acme.json declares the staging endpoint,
  which is reported but never converged onto.

  Scenario: A plain-HTTP ui-htmx target against a TLS-on rp is flagged
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "http://127.0.0.1:11115" } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "joins.client-transport" for service "ui-htmx"
    And that check's detail mentions "uses http"

  Scenario: --fix rewrites ui-htmx's scheme, CA trust, and credential once rp is provisioned
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115 } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "http://127.0.0.1:11115" } }
      """
    When I run doctor with --fix and --json
    Then the config file "ui-htmx.json" has the string "https://127.0.0.1:11115" at "/rp/base_url"
    And the config file "ui-htmx.json" has "/rp/ca_cert_path" pointing at the pki file "ca.pem"
    And the config file "ui-htmx.json" has the string "observatory" at "/rp/auth/username"

  Scenario: A missing credential against an auth-on target is flagged and fixed by --fix
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                    "auth": { "username": "observatory", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2g$aGFuZHNldA" } } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "https://127.0.0.1:11115" } }
      """
    When I run doctor with --json
    Then the report contains a "warn" check named "joins.client-auth" for service "ui-htmx"
    And that check's detail mentions "carries no credential"

  Scenario: A present but wrong ui-htmx credential is reported, not rewritten
    Given a config file "rp.json" whose auth hash is of the password "right-password"
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 },
        "rp": { "base_url": "http://127.0.0.1:11115",
                "auth": { "username": "observatory", "password": "wrong-password" } } }
      """
    When I run doctor with --json
    Then the report contains a "warn" check named "joins.client-auth" for service "ui-htmx"
    And that check's detail mentions "does not verify"

  Scenario: --fix rewrites rp's plate-solver scheme, CA trust, and credential once the target is provisioned
    Given a config file "plate-solver.json" containing:
      """
      { "server": { "port": 11131,
                    "auth": { "username": "observatory", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2g$aGFuZHNldA" } } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 }, "plate_solver": { "url": "http://localhost:11131" } }
      """
    When I run doctor with --fix and --json
    Then the config file "rp.json" has the string "https://localhost:11131" at "/plate_solver/url"
    And the config file "rp.json" has "/ca_cert" pointing at the pki file "ca.pem"
    And the config file "rp.json" has the string "observatory" at "/plate_solver/auth/username"

  Scenario: --fix rewrites rp's guider scheme, CA trust, and credential once the target is provisioned
    Given a config file "phd2-guider.json" containing:
      """
      { "server": { "port": 11130, "auth": { "username": "observatory", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2g$aGFuZHNldA" } } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 },
        "equipment": { "mount": { "alpaca_url": "http://localhost:11117",
                                   "guiding": { "url": "http://localhost:11130" } } } }
      """
    When I run doctor with --fix and --json
    Then the config file "rp.json" has the string "https://localhost:11130" at "/equipment/mount/guiding/url"
    And the config file "rp.json" has "/ca_cert" pointing at the pki file "ca.pem"
    And the config file "rp.json" has the string "observatory" at "/equipment/mount/guiding/auth/username"

  Scenario: --fix rewrites the mount's own scheme, CA trust, and credential once its target is provisioned
    Given a config file "star-adventurer-gti.json" containing:
      """
      { "server": { "port": 11117,
                    "auth": { "username": "observatory", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2g$aGFuZHNldA" } } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 },
        "equipment": { "mount": { "alpaca_url": "http://localhost:11117" } } }
      """
    When I run doctor with --fix and --json
    Then the config file "rp.json" has the string "https://localhost:11117" at "/equipment/mount/alpaca_url"
    And the config file "rp.json" has "/ca_cert" pointing at the pki file "ca.pem"
    And the config file "rp.json" has the string "observatory" at "/equipment/mount/auth/username"

  Scenario: --fix rewrites a camera's own scheme, CA trust, and credential once its target is provisioned
    Given a config file "zwo-camera.json" containing:
      """
      { "server": { "port": 11122,
                    "auth": { "username": "observatory", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2g$aGFuZHNldA" } } }
      """
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115 },
        "equipment": { "cameras": [ { "id": "main", "alpaca_url": "http://localhost:11122" } ] } }
      """
    When I run doctor with --fix and --json
    Then the config file "rp.json" has the string "https://localhost:11122" at "/equipment/cameras/0/alpaca_url"
    And the config file "rp.json" has "/ca_cert" pointing at the pki file "ca.pem"
    And the config file "rp.json" has the string "observatory" at "/equipment/cameras/0/auth/username"

  Scenario: A non-loopback client target is never joined
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "http://10.0.0.5:11115" } }
      """
    When I run doctor with --json
    Then the report has no checks named "joins.client-transport"

  Scenario: sentinel's Alpaca monitor scheme and credential are flagged and fixed by --fix
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112 } }
      """
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                         "host": "localhost", "port": 11112, "scheme": "http" } ] }
      """
    When I run doctor with --fix and --json
    Then the config file "sentinel.json" has the string "https" at "/monitors/0/scheme"
    And the config file "sentinel.json" has the string "observatory" at "/monitors/0/auth/username"

  Scenario: A self-signed monitor target sentinel has no ca_cert for is flagged read-only
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112, "tls": { "cert": "/pki/ppba-driver.pem", "key": "/pki/ppba-driver-key.pem" } } }
      """
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                         "host": "localhost", "port": 11112, "scheme": "https" } ] }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "joins.client-transport" for service "sentinel"
    And that check's detail mentions "self-signed"

  Scenario: A self-signed rp the watchdog has no ca_cert for is flagged read-only
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/rp.pem", "key": "/pki/rp-key.pem" } } }
      """
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "operation_watchdog": { "rp_url": "https://localhost:11115" } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "joins.client-transport" for service "sentinel"
    And that check's detail mentions "no ca_cert to trust it"

  Scenario: sentinel's own ca_cert satisfies a self-signed monitor target
    Given a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112, "tls": { "cert": "/pki/ppba-driver.pem", "key": "/pki/ppba-driver-key.pem" } } }
      """
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 }, "ca_cert": "/pki/ca.pem",
        "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                         "host": "localhost", "port": 11112, "scheme": "https" } ] }
      """
    When I run doctor with --json
    Then the report has no checks named "joins.client-transport"

  Scenario: sentinel's watchdog rp_url scheme is fixed, without duplicating auth.mismatch
    Given a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                    "auth": { "username": "observatory", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2g$aGFuZHNldA" } } }
      """
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "operation_watchdog": { "rp_url": "http://localhost:11115" } }
      """
    When I run doctor with --fix and --json
    Then the config file "sentinel.json" has the string "https://localhost:11115" at "/operation_watchdog/rp_url"
    And the report has no checks named "joins.client-auth"

  Scenario: An ACME target behind a loopback client URL fails hostname verification and is flagged
    Given an acme.json for the domain "pier1.example.com"
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "https://127.0.0.1:11115" } }
      """
    When I run doctor with --json
    Then the report contains a "fail" check named "joins.client-transport" for service "ui-htmx"
    And that check's detail mentions "hostname verification"

  Scenario: --fix composes the scheme and host rewrites into one URL on the public ACME name
    Given an acme.json for the domain "pier1.example.com"
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "http://127.0.0.1:11115" } }
      """
    When I run doctor with --fix and --json
    Then the config file "ui-htmx.json" has the string "https://rp.pier1.example.com:11115" at "/rp/base_url"

  Scenario: A client URL already on the public ACME name still joins its target
    Given an acme.json for the domain "pier1.example.com"
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" },
                    "auth": { "username": "observatory", "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$YWJjZGVmZ2g$aGFuZHNldA" } } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "https://rp.pier1.example.com:11115" } }
      """
    When I run doctor with --json
    Then the report contains a "warn" check named "joins.client-auth" for service "ui-htmx"
    And the report has no checks named "joins.client-transport"

  Scenario: A public name that is not the port-matched service's own never joins
    Given an acme.json for the domain "pier1.example.com"
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "https://sentinel.pier1.example.com:11115" } }
      """
    When I run doctor with --json
    Then the report has no checks named "joins.client-transport"

  Scenario: A staging acme.json reports the loopback break but never converges clients onto it
    Given an acme.json for the domain "pier1.example.com"
    And the acme.json is amended to use the staging endpoint
    And a config file "rp.json" containing:
      """
      { "server": { "port": 11115, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }
      """
    And a config file "ui-htmx.json" containing:
      """
      { "server": { "port": 11120 }, "rp": { "base_url": "https://127.0.0.1:11115" } }
      """
    When I run doctor with --fix and --json
    Then the config file "ui-htmx.json" has the string "https://127.0.0.1:11115" at "/rp/base_url"
    And the report contains a "fail" check named "joins.client-transport" for service "ui-htmx"
    And that check's detail mentions "staging"

  Scenario: --fix moves a loopback monitor host onto the target's public ACME name
    Given an acme.json for the domain "pier1.example.com"
    And a config file "ppba-driver.json" containing:
      """
      { "server": { "port": 11112, "tls": { "cert": "/pki/acme-cert.pem", "key": "/pki/acme-key.pem" } } }
      """
    And a config file "sentinel.json" containing:
      """
      { "server": { "port": 11114 },
        "monitors": [ { "type": "alpaca_safety_monitor", "name": "PPBA",
                         "host": "127.0.0.1", "port": 11112, "scheme": "https" } ] }
      """
    When I run doctor with --fix and --json
    Then the config file "sentinel.json" has the string "ppba-driver.pier1.example.com" at "/monitors/0/host"
