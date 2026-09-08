Feature: Server Registration
  As a system administrator
  I want the server to register devices based on configuration
  So that only enabled devices are exposed via ASCOM Alpaca

  Scenario: Both devices enabled responds on both endpoints
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    Then the switch endpoint should respond with 200
    And the OC endpoint should respond with 200

  Scenario: Switch only mode
    Given a UPBv2 server config with switch enabled and OC disabled
    When I start the UPBv2 server
    Then the switch endpoint should respond with 200
    And the OC endpoint should not respond with 200

  Scenario: OC only mode
    Given a UPBv2 server config with switch disabled and OC enabled
    When I start the UPBv2 server
    Then the switch endpoint should not respond with 200
    And the OC endpoint should respond with 200

  Scenario: No devices enabled
    Given a UPBv2 server config with switch disabled and OC disabled
    When I start the UPBv2 server
    Then the switch endpoint should not respond with 200
    And the OC endpoint should not respond with 200

  Scenario: Configured switch name is returned
    Given a UPBv2 server config with switch name "My Custom Switch"
    When I start the UPBv2 server
    Then the switch name endpoint should return "My Custom Switch"

  Scenario: Configured OC name is returned
    Given a UPBv2 server config with OC name "My Weather Station"
    When I start the UPBv2 server
    Then the OC name endpoint should return "My Weather Station"

  Scenario: Server binds to OS-assigned port
    Given a UPBv2 server config with switch enabled and OC disabled
    When I start the UPBv2 server
    Then the server should be reachable on the bound port
