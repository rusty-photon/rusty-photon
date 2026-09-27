@serial
Feature: Plate solve MCP tool
  The plate_solve MCP tool proxies to the plate-solver rp-managed service
  over HTTP and returns a parsed WCS solution. It accepts either
  document_id (resolved via the image cache) or image_path (forwarded
  to the wrapper); if both are supplied, document_id takes precedence.
  Pointing hints are explicit: callers either pass an explicit
  pointing_hint object or set use_mount_hints=true to read the current
  mount position; the two are mutually exclusive. Right-ascension is
  decimal hours from the Alpaca mount and decimal degrees on the
  wrapper wire — rp performs the ×15 conversion when use_mount_hints
  is set. fov_hint_deg, search_radius_deg, and timeout are forwarded
  verbatim; search_radius_deg defaults to plate_solver.default_search_radius_deg
  from rp config when omitted, and a per-call value overrides. When
  called with document_id, results are written into the exposure
  document as a "wcs" section. With image_path, results are written
  into the matching sidecar when the filename's UUID-8 suffix
  resolves to a known document; otherwise the result is returned
  without persistence.

  Scenario: Tool catalog includes plate_solve
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "plate_solve"

  Scenario: Returns contract fields after capture using image_path
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured image path
    Then the plate_solve result should contain "ra_center" with value 10.6848
    And the plate_solve result should contain "dec_center" with value 41.269
    And the plate_solve result should contain "pixel_scale_arcsec" with value 1.05
    And the plate_solve result should contain "rotation_deg" with value 12.3
    And the plate_solve result should contain "solver" with value "stub-astap-1.0"
    And the plate_solve result wcs_matrix should match these values:
      | field  | value       |
      | crpix1 | 512.0       |
      | crpix2 | 384.0       |
      | cd1_1  | -0.00029167 |
      | cd1_2  | 0.0         |
      | cd2_1  | 0.0         |
      | cd2_2  | 0.00029167  |

  Scenario: Returns contract fields when called with document_id
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    Then the plate_solve result should contain "ra_center" with value 10.6848
    And the plate_solve result should contain "dec_center" with value 41.269

  Scenario: Persists wcs section into the exposure document in document_id mode
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    And I fetch the exposure document for the captured document_id
    Then the exposure document should contain a section named "wcs"
    And the "wcs" section should contain "ra_center"
    And the "wcs" section should contain "dec_center"
    And the "wcs" section should contain "pixel_scale_arcsec"
    And the "wcs" section should contain "rotation_deg"
    And the "wcs" section should contain "solver"
    And the "wcs" section should contain "wcs_matrix"

  Scenario: image_path mode against rp-produced FITS persists wcs to the matching sidecar
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured image path
    And I fetch the exposure document for the captured document_id
    Then the exposure document should contain a section named "wcs"

  Scenario: image_path mode against an external FITS returns result without persistence
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "plate_solve" with image path "/tmp/external-frame-no-uuid.fits"
    Then the plate_solve result should contain "ra_center" with value 10.6848

  Scenario: document_id takes precedence when both arguments are supplied
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with both the captured document_id and image path "/tmp/ignored.fits"
    Then the plate_solve result should contain "ra_center" with value 10.6848
    And the stub plate solver should have received a request whose fits_path matches the captured FITS

  Scenario: Explicit pointing_hint forwarded to the wrapper as ra_hint and dec_hint
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and pointing_hint ra_deg 160.272 dec_deg 41.269
    Then the stub plate solver should have received a request with ra_hint 160.272 and dec_hint 41.269

  # The hint is pinned against what the mount actually reported, not
  # against the 10.6848 h we synced to: OmniSim's SyncToCoordinates
  # does not land exactly on the requested RA, and the size of the
  # miss is a wall-clock artefact of the runner (issue #1252). The
  # readback keeps the ×15 hours-to-degrees guard tight; the 0.5°
  # sanity bound still catches a mount left pointing elsewhere.
  Scenario: use_mount_hints true reads the mount and forwards converted RA and Dec
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And the mount tracking is set to true
    And an MCP client connected to rp
    When the MCP client calls "sync_mount" with ra "10.6848" dec "41.2690"
    And the MCP client calls "capture" with camera "main-cam" for 100 ms
    # A read either side of the call brackets rp's own read of the mount, which
    # OmniSim does not hold still: a sync lands offset and the reported RA then
    # moves for several ticks, and under load its tick starves and RA slips at
    # about the sidereal rate. Bracketing holds however fast it moves.
    And I record the mount position reported by get_mount_position
    And the MCP client calls "plate_solve" with the captured document_id and use_mount_hints true
    And I record the mount position reported by get_mount_position
    Then the stub plate solver should have received ra_hint and dec_hint bracketed by the recorded mount positions
    And the recorded mount position should be within 0.5 degrees of ra 160.272 and dec 41.269

  Scenario: use_mount_hints true with no mount configured returns error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and use_mount_hints true
    Then the tool call should return an error
    And the error message should contain "use_mount_hints"

  Scenario: pointing_hint and use_mount_hints together return error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id, pointing_hint ra_deg 1.0 dec_deg 2.0, and use_mount_hints true
    Then the tool call should return an error
    And the error message should contain "pointing_hint"

  Scenario: No hints produces a blind solve request
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    Then the stub plate solver should have received a request with no hint fields

  Scenario: fov_hint_deg, search_radius_deg, and timeout are forwarded verbatim
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id, fov_hint_deg 1.5, search_radius_deg 2.0, and timeout "45s"
    Then the stub plate solver should have received a request with fov_hint_deg 1.5 and search_radius_deg 2.0 and timeout "45s"

  Scenario: Config default_search_radius_deg is applied when MCP parameter is omitted
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator and plate_solver default_search_radius_deg 4.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    Then the stub plate solver should have received a request with search_radius_deg 4.0

  Scenario: Per-call search_radius_deg overrides config default
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator and plate_solver default_search_radius_deg 4.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and search_radius_deg 2.5
    Then the stub plate solver should have received a request with search_radius_deg 2.5

  Scenario: plate_solve without configured plate solver returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    Then the tool call should return an error
    And the error message should contain "plate solver not configured"

  Scenario: Service unreachable error when plate_solver URL points at unbound port
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator and plate_solver pointing at an unbound port
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    Then the tool call should return an error
    And the error message should contain "service unreachable"

  Scenario Outline: Wrapper structured errors propagate verbatim
    Given a running Alpaca simulator
    And a stub plate solver returning error code "<code>" with message "<message>"
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    Then the tool call should return an error
    And the error message should contain "<code>"
    And the error message should contain "<message>"

    Examples:
      | code            | message                          |
      | invalid_request | malformed body                   |
      | fits_not_found  | not a regular file               |
      | solve_failed    | ASTAP exited with code 1         |
      | solve_timeout   | wall-clock deadline expired      |
      | internal        | broken pipe                      |

  Scenario: plate_solve with neither document_id nor image_path returns error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "plate_solve" with no arguments
    Then the tool call should return an error
    And the error message should contain "image_path"
