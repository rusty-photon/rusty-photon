Feature: Target store import form (P3)
  P3 of the planetarium-target-import plan activates add_target's third
  form — bare ra_hours + dec_degrees + a source {kind, client,
  received_at} provenance object — the shape the planetarium bridge
  sends (rp.md § Target Store → Import form). Naming is rp's job here:
  one reverse cone-search over the embedded catalog names the import (a
  deep-sky hit outranks any star hit; no hit falls back to the
  IAU-style coordinate form), so catalog_ref and display_name are
  rejected alongside source. Identity is proximity-only dedup
  (target_store.import.dedup_arcsec, default 30 arcsec): a repeated
  import within tolerance upserts only a row that is still pending and
  last written by the same source.kind; active or operator-touched rows
  are never modified (Decision 3,
  docs/plans/archive/planetarium-target-import.md). Imports always land paused
  with the config-default goals, stamped with typed writer identity
  (created_by / updated_by = source.kind) and a human-readable
  provenance line in notes.

  The slug is derived from the naming hit by the one name-to-slug rule
  the whole system shares (rp-targets.md § Identity): whitespace runs
  collapse to a hyphen, so the catalog name "M 31" imports as m-31 and
  a second framing of it allocates m-31-2.

  Scenario: The source form rejects naming, activation, notes, and framing parameters
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client imports at ra_hours 5.5 dec_degrees 40.0 with extra parameter "catalog_ref"
    Then the tool call should fail
    When the MCP client imports at ra_hours 5.5 dec_degrees 40.0 with extra parameter "display_name"
    Then the tool call should fail
    When the MCP client imports at ra_hours 5.5 dec_degrees 40.0 with extra parameter "active"
    Then the tool call should fail
    When the MCP client imports at ra_hours 5.5 dec_degrees 40.0 with extra parameter "notes"
    Then the tool call should fail
    When the MCP client imports at ra_hours 5.5 dec_degrees 40.0 with extra parameter "position_angle_degrees"
    Then the tool call should fail

  Scenario: The source form requires coordinates
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client imports a target without coordinates
    Then the tool call should fail

  Scenario: The operator writer identity cannot be forged through source
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client imports at ra_hours 5.5 dec_degrees 40.0 with source kind "operator"
    Then the tool call should fail

  Scenario: An import lands paused, named by the catalog, with typed provenance
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has resolved catalog target "M 31"
    When the MCP client imports a target at the resolved coordinates
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "m-31"
    And the imported target display_name should be "M 31"
    And the imported target catalog_ref should be "M 31"
    And the imported target should be paused
    And the imported target created_by should be "planetarium-bridge"
    And the imported target updated_by should be "planetarium-bridge"
    And the imported target notes should contain "Imported via planetarium-bridge from 192.0.2.10:52441"

  Scenario: An import receives the config-default goals
    Given rp is configured with default target goals:
      | filter | binning | exposure_duration | desired_count |
      | L      | 1x1     | 5m                | 20            |
    And rp is running with a target store
    And an MCP client connected to rp
    When the MCP client imports a target at ra_hours 1.9 dec_degrees -44.9
    Then the tool call should succeed
    And the imported target should have 1 goal with filter "L"

  Scenario: An import framed away from the catalog centroid reads as an offset
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has resolved catalog target "M 31"
    When the MCP client imports a target at the resolved coordinates offset east 8.0 north -4.0 arcmin
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "m-31"
    And the imported target display_name should be "M 31 +8′E −4′N"

  Scenario: A second framing of the same object is a new target beside the first
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has resolved catalog target "M 31"
    And the MCP client has imported a target at the resolved coordinates
    When the MCP client imports a target at the resolved coordinates offset east 8.0 north -4.0 arcmin
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "m-31-2"
    And the imported target display_name should be "M 31 +8′E −4′N"
    When the MCP client calls "get_target" for slug "m-31"
    Then the tool call should succeed
    And the fetched target should have display_name "M 31"

  Scenario: A repeated import within tolerance refreshes the pending row in place
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has resolved catalog target "M 31"
    And the MCP client has imported a target at the resolved coordinates
    When the MCP client imports a target at the resolved coordinates offset east 0.2 north 0.1 arcmin
    Then the tool call should succeed
    And the target result should be an in-place update
    And the target slug should be "m-31"
    And the imported target display_name should be "M 31"
    And the imported target coordinates should be the resolved coordinates offset east 0.2 north 0.1 arcmin
    When the MCP client calls "list_targets"
    Then the tool call should succeed
    And list_targets should report exactly 1 target

  Scenario: An activated target is never modified by a later import
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has resolved catalog target "M 31"
    And the MCP client has imported a target at the resolved coordinates
    And the MCP client has activated the target it just added
    When the MCP client imports a target at the resolved coordinates offset east 0.2 north 0.1 arcmin
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "m-31-2"
    When the MCP client calls "get_target" for slug "m-31"
    Then the tool call should succeed
    And the fetched target should be active

  Scenario: An operator edit protects a pending import from later upserts
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has resolved catalog target "M 31"
    And the MCP client has imported a target at the resolved coordinates
    And the MCP client has renamed the target it just added to "Andromeda mosaic A"
    When the MCP client imports a target at the resolved coordinates offset east 0.2 north 0.1 arcmin
    Then the tool call should succeed
    And the target result should be created
    And the target slug should be "m-31-2"
    And the imported target created_by should be "planetarium-bridge"
    When the MCP client calls "get_target" for slug "m-31"
    Then the tool call should succeed
    And the fetched target should have display_name "Andromeda mosaic A"
    And the fetched target created_by should be "planetarium-bridge"
    And the fetched target updated_by should be "operator"

  Scenario: A faint star anchor names an import when no deep-sky object is near
    Given rp is running with a target store
    And an MCP client connected to rp
    And the MCP client has resolved catalog target "HD 170881"
    When the MCP client imports a target at the resolved coordinates
    Then the tool call should succeed
    And the imported target display_name should be "HD 170881"
    And the target slug should be "hd-170881"

  Scenario: An import with no catalog neighbor gets the coordinate name
    Given rp is running with a target store
    And an MCP client connected to rp
    When the MCP client imports a target at ra_hours 1.9 dec_degrees -44.9
    Then the tool call should succeed
    And the imported target display_name should be "J0154-4454"
    And the target slug should be "j0154m4454"
