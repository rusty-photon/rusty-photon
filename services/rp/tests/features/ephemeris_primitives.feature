@serial
Feature: Ephemeris primitive MCP tools
  rp exposes nine ephemeris primitives as MCP tools, each backed by
  the `Ephemeris` trait in `rp-ephemeris` (which wraps ERFA / IAU
  SOFA via the `erfars` crate). Each is a single-operation tool
  shaped so that a planner plugin can compose them; the convenience
  tools (planner.feature) call into the same trait. The `get_site` tool
  reports the configured site itself, so a plugin (polar-align) can
  source the observer coordinates instead of duplicating them in its
  own config.

  Tools that need a site error cleanly when the deployment has no
  `site` block. Time inputs are RFC3339 strings (`"2026-05-03T22:00:00Z"`)
  and default to the server's wall clock if omitted. Date inputs are
  `YYYY-MM-DD`.

  Scenario: Tool catalog includes the ephemeris primitives
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "compute_alt_az"
    And the tool list should include "compute_transit"
    And the tool list should include "compute_rise_set"
    And the tool list should include "compute_meridian_flip"
    And the tool list should include "get_sun_position"
    And the tool list should include "get_twilight"
    And the tool list should include "get_moon_position"
    And the tool list should include "compute_moon_separation"
    And the tool list should include "get_local_sidereal_time"
    And the tool list should include "get_site"

  Scenario: get_site reports the configured coordinates
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_site"
    Then the tool call should succeed
    And the result latitude_degrees should be 51.0786
    And the result longitude_degrees should be -0.2944

  Scenario: get_site fails cleanly when site is not configured
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_site"
    Then the tool call should fail
    And the tool error message should mention "site not configured"

  Scenario: get_local_sidereal_time returns a value in [0, 24)
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_local_sidereal_time" with time "2026-05-03T22:00:00Z"
    Then the tool call should succeed
    And the result lst_hours should be in the range [0, 24)

  Scenario: compute_alt_az for Polaris returns altitude near observer latitude
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "compute_alt_az" for Polaris
    Then the tool call should succeed
    And the result altitude_degrees should be approximately 51.1 within 1.5

  Scenario: compute_alt_az fails cleanly when site is not configured
    Given a running Alpaca simulator
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "compute_alt_az" for Polaris
    Then the tool call should fail
    And the tool error message should mention "site not configured"

  Scenario: compute_alt_az rejects out-of-range RA
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "compute_alt_az" with ra "25.0" dec "0.0"
    Then the tool call should fail
    And the tool error message should mention "ra_hours 25 is outside [0, 24)"

  Scenario: get_twilight rejects an unknown kind
    Given a running Alpaca simulator
    And rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "get_twilight" for date "2026-12-21" kind "daytime"
    Then the tool call should fail
    And the tool error message should mention "unknown twilight kind"
