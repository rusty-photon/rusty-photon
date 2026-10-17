Feature: Configuration actions
  The driver exposes its own configuration over HTTP as the vendor ASCOM
  actions `config.get`, `config.apply`, and `config.schema`. `config.get`
  returns the effective configuration (secrets redacted) plus the
  CLI-override-pinned field paths. `config.apply` parses and validates a full
  configuration blob: invalid input returns `status:"invalid"` with field-level
  errors and leaves the file unchanged, while a valid change is persisted
  atomically, classified, and applied through an in-process reload
  (`status:"applying"`). `config.schema` returns a JSON Schema describing the
  configuration's shape plus the editability tiers (identity/locked and hard
  read-only fields) the web UI renders the form from. The actions work whether
  or not the device is connected. An action name is matched without regard
  to case, as the Alpaca specification requires of every Action name;
  SupportedActions lists the canonical spelling.

  Scenario: The config actions are advertised
    Given a running FP2 service
    When the supported actions are queried
    Then the supported actions should include config.get and config.apply

  Scenario: The configuration schema is served with its editability tiers
    Given a running FP2 service
    When config.schema is called
    Then the schema should describe the serial, server, and cover_calibrator sections
    And the schema should mark cover_calibrator.unique_id as a locked field
    And the schema should mark server.port as a read-only field

  Scenario: Read the current configuration while disconnected
    Given a running FP2 service
    When config.get is called
    Then the config should report serial.port as /dev/mock
    And the config should report no overrides

  Scenario: A valid change is persisted and reloaded
    Given a running FP2 service
    When config.apply sets max_brightness to 2048
    Then the apply status should be applying
    And the reload list should include cover_calibrator.max_brightness

  Scenario: A reloaded server rebinds its port and serves the new configuration
    Given a running FP2 service
    When config.apply pins the bound port and sets max_brightness to 2048
    Then the reloaded service serves max_brightness 2048

  Scenario: An invalid configuration is rejected and not persisted
    Given a running FP2 service
    When config.apply is called with an invalid baud_rate
    Then the apply status should be invalid
    And the response should contain validation errors

  Scenario: A min_brightness above max_brightness is rejected and not persisted
    Given a running FP2 service
    When config.apply is called with min_brightness above max_brightness
    Then the apply status should be invalid
    And the response should contain validation errors

  Scenario: An unknown action is not implemented
    Given a running FP2 service
    When the action "config.frobnicate" is called
    Then the call should fail with an action-not-implemented error

  Scenario: An action name is matched without regard to case
    Given a running FP2 service
    When the action "CONFIG.GET" is called
    Then the config should report serial.port as /dev/mock
