@serial
Feature: Effective position angle (P2)
  Decision 5 of the planetarium-target-import plan
  (docs/plans/archive/planetarium-target-import.md): a target's framing angle
  is a three-layer fallback resolved at read time — the target's own
  position_angle_degrees, then the imaging train's configured
  default_position_angle_degrees, then 0.0 (north-up) — homed per
  optical train, because a camera's fixed mounting angle is a physical
  fact of one light path (rp.md § Target Store → Position angle).

  get_next_target resolves the fallback: it accepts an optional
  train_id (an equipment.optical_trains[] id; unknown ids are an
  error) and returns the effective angle as a top-level
  position_angle_degrees field beside exposure, null only when target
  is null. The deep_sky workflow threads the returned value through
  its blackboard into a move_rotator call after slew/centering
  (session-runner.md § deep_sky.json). Storage and operator editing of
  the per-target field are pinned in target_store_crud.feature;
  imports never carry an angle (target_store_import.feature).

  Scenario: A target's own angle outranks the train default
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a camera on the simulator in an imaging train with default position angle 254.0
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Framed Field" with position angle 121.25
    When the MCP client calls "get_next_target" with train "main"
    Then the tool call should succeed
    And the result reason should be "best_transiting_candidate"
    And the result position_angle_degrees should be 121.25

  Scenario: A target without an angle inherits the train default
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a camera on the simulator in an imaging train with default position angle 254.0
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Plain Field"
    When the MCP client calls "get_next_target" with train "main"
    Then the tool call should succeed
    And the result position_angle_degrees should be 254.0

  Scenario: North-up when neither the target nor the train carries an angle
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a camera on the simulator in an imaging train
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Plain Field"
    When the MCP client calls "get_next_target" with train "main"
    Then the tool call should succeed
    And the result position_angle_degrees should be 0.0

  Scenario: Without a train the target's own angle still applies
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Framed Field" with position angle 121.25
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the result position_angle_degrees should be 121.25

  Scenario: An unknown train is an error
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a camera on the simulator in an imaging train
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Plain Field"
    When the MCP client calls "get_next_target" with train "no-such-train"
    Then the tool call should fail
    And the tool error message should mention "no-such-train"

  Scenario: A recommendation without a target carries no angle
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_next_target"
    Then the tool call should succeed
    And the result reason should be "no_targets_configured"
    And the result position_angle_degrees should be null
