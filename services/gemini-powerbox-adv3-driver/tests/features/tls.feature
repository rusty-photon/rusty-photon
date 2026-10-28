Feature: gemini-powerbox-adv3-driver TLS support
  gemini-powerbox-adv3-driver can serve over HTTPS when TLS is configured.

  Scenario: gemini-powerbox-adv3-driver starts with TLS and accepts HTTPS requests
    Given generated TLS certificates for gemini-powerbox-adv3-driver
    And gemini-powerbox-adv3-driver is configured with TLS enabled and mock serial
    When gemini-powerbox-adv3-driver is started with TLS
    Then the Alpaca management endpoint should respond over HTTPS
