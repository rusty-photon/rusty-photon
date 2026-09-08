Feature: Configuration actions
  The driver registers two ASCOM devices (Switch + ObservingConditions) backed
  by one config file, and BOTH expose the vendor actions `config.get`,
  `config.apply`, and `config.schema` — the cross-driver protocol in
  `docs/services/config-actions.md`. An apply on either device operates on the
  same full driver config and fires the same in-process reload. `config.get`
  returns the effective configuration (secrets redacted) plus CLI-override-pinned
  paths; `config.apply` validates and persists a full config blob (invalid ->
  `status:"invalid"` with field errors, file unchanged; a valid change ->
  persisted + `status:"applying"` + reload); `config.schema` returns a JSON
  Schema plus editability tiers covering both devices' identity fields.

  Scenario: Both devices advertise the config actions
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And the supported actions are queried on the switch device
    Then the queried supported actions should include config.get, config.apply, and config.schema
    When the supported actions are queried on the observingconditions device
    Then the queried supported actions should include config.get, config.apply, and config.schema

  Scenario: The configuration schema is served with both devices' editability tiers
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And config.schema is called on the switch device
    Then the schema should describe the serial, server, switch, and observingconditions sections
    And the schema should mark switch.unique_id and observingconditions.unique_id as locked fields

  Scenario: Read the current configuration
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And config.get is called on the switch device
    Then the config should report serial.port as /dev/mock
    And the config should report no overrides

  Scenario: A valid change is persisted and reloaded
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And config.apply pins the bound port and sets the switch name to "Renamed Switch"
    Then the apply status should be applying
    And the reloaded service serves switch name "Renamed Switch"

  Scenario: An invalid configuration is rejected and not persisted
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And config.apply is called with an empty serial port
    Then the apply status should be invalid
    And the response should contain validation errors

  Scenario: An unknown action is not implemented
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And the action "config.frobnicate" is called on the switch device
    Then the call should fail with an action-not-implemented error
