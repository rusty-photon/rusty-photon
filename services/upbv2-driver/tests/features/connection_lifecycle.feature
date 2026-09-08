Feature: Connection Lifecycle
  As an ASCOM client
  I want to manage device connections
  So that I can control power outputs and read sensors

  Scenario: Switch device starts disconnected
    Given a running UPBv2 server
    Then the switch device should report disconnected

  Scenario: Switch device connects successfully
    Given a running UPBv2 server
    When I connect the switch device
    Then the switch device should report connected

  Scenario: Switch device disconnects successfully
    Given a running UPBv2 server
    When I connect the switch device
    And I disconnect the switch device
    Then the switch device should report disconnected

  Scenario: Switch device connect is idempotent
    Given a running UPBv2 server
    When I connect the switch device
    And I connect the switch device
    Then the switch device should report connected

  Scenario: Switch device disconnect is idempotent
    Given a running UPBv2 server
    When I connect the switch device
    And I disconnect the switch device
    And I disconnect the switch device
    Then the switch device should report disconnected

  Scenario: Switch device survives multiple connect-disconnect cycles
    Given a running UPBv2 server
    When I cycle the switch device connection 5 times
    Then the switch device should report disconnected

  Scenario: OC device starts disconnected
    Given a running UPBv2 server
    Then the OC device should report disconnected

  Scenario: OC device connects successfully
    Given a running UPBv2 server
    When I connect the OC device
    Then the OC device should report connected

  Scenario: OC device disconnects successfully
    Given a running UPBv2 server
    When I connect the OC device
    And I disconnect the OC device
    Then the OC device should report disconnected

  Scenario: OC device connect is idempotent
    Given a running UPBv2 server
    When I connect the OC device
    And I connect the OC device
    Then the OC device should report connected

  Scenario: OC device disconnect is idempotent
    Given a running UPBv2 server
    When I connect the OC device
    And I disconnect the OC device
    And I disconnect the OC device
    Then the OC device should report disconnected
