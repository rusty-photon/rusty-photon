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

  Scenario: A switch label applied at runtime is persisted and served after the reload
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And config.apply pins the bound port and sets the switch labels {"12V Output 1": "QHY600"}
    Then the apply status should be applying
    And the reloaded service reports switch.labels as {"12V Output 1": "QHY600"}

  Scenario Outline: A label map that would break the switch table is rejected
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And config.apply is called with the switch labels <labels>
    Then the call should fail with an INVALID_VALUE error naming "<offender>"

    Examples: a key that names no switch an operator may label
      | labels                     | offender     |
      | {"12V Output 9": "QHY600"} | 12V Output 9 |
      | {"Temperature": "Sky"}     | Temperature  |

    Examples: a label that collides with a name another switch already publishes
      | labels                                         | offender             |
      | {"12V Output 1": "12V Output 2"}               | 12V Output 2         |
      | {"12V Output 1": "12V Output 2 Current"}       | 12V Output 2 Current |
      | {"12V Output 1": "Cam", "12V Output 2": "Cam"} | Cam                  |

    Examples: a blank label, which is not how a switch goes back to its built-in name
      | labels                   | offender     |
      | {"12V Output 1": "   "}  | 12V Output 1 |

  Scenario: An unknown action is not implemented
    Given a UPBv2 server config with switch enabled and OC enabled
    When I start the UPBv2 server
    And the action "config.frobnicate" is called on the switch device
    Then the call should fail with an action-not-implemented error
