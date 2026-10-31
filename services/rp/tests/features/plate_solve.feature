@serial
Feature: Plate solve MCP tool
  The plate_solve MCP tool proxies to the plate-solver rp-managed service
  over HTTP and returns a parsed WCS solution. It accepts either
  document_id (resolved via the image cache) or image_path (forwarded
  to the wrapper); if both are supplied, document_id takes precedence.
  Hints come from the call, else from the frame's own record: never
  from the live mount, never from the FITS header. The position is an
  explicit pointing_hint, else the pointing the frame's sidecar
  recorded, its ra_hours times 15 on the wrapper's degrees wire. A
  frame with no position, or a call with blind true, is solved blind:
  the wrapper gets blind true, no position and no search radius, so
  blind excludes pointing_hint and search_radius_deg. The scale is
  fov_hint_deg, else the sidecar's optics.fov_height_deg. The search
  radius is search_radius_deg, else plate_solver.default_search_radius_deg
  from rp config. A call's own timeout is forwarded, and rp's outer
  plate_solver.timeout never cuts a solve short of it. When called with
  document_id, results are written into the exposure document as a
  "wcs" section. With image_path, results are written into the sidecar
  beside the file when there is one; otherwise the result is returned
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

  Scenario: An external FITS is solved blind and returned without persistence
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "plate_solve" with image path "/tmp/external-frame-no-uuid.fits"
    Then the plate_solve result should contain "ra_center" with value 10.6848
    And the stub plate solver should have received a blind request with no position and no search radius

  Scenario: document_id takes precedence when both arguments are supplied
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with both the captured document_id and image path "/tmp/ignored.fits"
    Then the plate_solve result should contain "ra_center" with value 10.6848
    And the stub plate solver should have received a request whose fits_path matches the captured FITS

  Scenario: A frame's recorded pointing is the position hint
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    And I fetch the exposure document for the captured document_id
    Then the stub plate solver should have received the document's pointing as ra_hint and dec_hint, its ra_hours times 15
    And the stub plate solver should have received a request that is not blind

  Scenario: image_path mode takes the position hint from the sidecar beside the file
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured image path
    And I fetch the exposure document for the captured document_id
    Then the stub plate solver should have received the document's pointing as ra_hint and dec_hint, its ra_hours times 15

  Scenario: An explicit pointing_hint overrides the frame's recorded pointing
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and pointing_hint ra_deg 160.272 dec_deg 41.269
    Then the stub plate solver should have received a request with ra_hint 160.272 and dec_hint 41.269

  Scenario: A frame's recorded field of view is the scale hint
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a fully equipped capture rig on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    And I fetch the exposure document for the captured document_id
    Then the stub plate solver should have received the document's optics fov_height_deg as fov_hint_deg

  Scenario: Per-call fov_hint_deg, search_radius_deg and timeout override the defaults
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a fully equipped capture rig on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id, fov_hint_deg 1.5, search_radius_deg 2.0, and timeout "45s"
    Then the stub plate solver should have received a request with fov_hint_deg 1.5 and search_radius_deg 2.0 and timeout "45s"

  Scenario: A frame with no recorded pointing is solved blind
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator and plate_solver default_search_radius_deg 4.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    Then the stub plate solver should have received a blind request with no position and no search radius

  Scenario: A per-call search_radius_deg is dropped when the frame has no position
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and search_radius_deg 2.5
    Then the stub plate solver should have received a blind request with no position and no search radius

  Scenario: blind true ignores the frame's recorded pointing
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and blind true
    Then the stub plate solver should have received a blind request with no position and no search radius

  Scenario: blind true keeps the frame's recorded field of view
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a fully equipped capture rig on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and blind true
    And I fetch the exposure document for the captured document_id
    Then the stub plate solver should have received a blind request with no position and no search radius
    And the stub plate solver should have received the document's optics fov_height_deg as fov_hint_deg

  Scenario: blind true with a pointing_hint returns an error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id, pointing_hint ra_deg 1.0 dec_deg 2.0, and blind true
    Then the tool call should return an error
    And the error message should contain "blind excludes pointing_hint"
    And the stub plate solver should have received 0 solve calls

  Scenario: blind true with a search_radius_deg returns an error
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id, search_radius_deg 2.5, and blind true
    Then the tool call should return an error
    And the error message should contain "blind excludes search_radius_deg"
    And the stub plate solver should have received 0 solve calls

  Scenario: Config default_search_radius_deg is applied when MCP parameter is omitted
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator and plate_solver default_search_radius_deg 4.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and pointing_hint ra_deg 160.272 dec_deg 41.269
    Then the stub plate solver should have received a request with search_radius_deg 4.0

  Scenario: Per-call search_radius_deg overrides config default
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator and plate_solver default_search_radius_deg 4.0
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id, pointing_hint ra_deg 160.272 dec_deg 41.269, and search_radius_deg 2.5
    Then the stub plate solver should have received a request with search_radius_deg 2.5

  Scenario: A solve's own timeout reaches past plate_solver.timeout
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS after 2000 ms
    And rp is running with a camera on the simulator and plate_solver timeout 1000 ms
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id and timeout "5s"
    Then the plate_solve result should contain "ra_center" with value 10.6848

  Scenario: A solve without its own timeout is cut by plate_solver.timeout
    Given a running Alpaca simulator
    And a stub plate solver returning a canned WCS after 2000 ms
    And rp is running with a camera on the simulator and plate_solver timeout 1000 ms
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the MCP client calls "plate_solve" with the captured document_id
    Then the tool call should return an error
    And the error message should contain "service unreachable"

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
