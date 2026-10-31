@serial
Feature: Finding a frame's files on disk
  A frame is a pair of files: the FITS file holding the pixels and the
  sidecar beside it, same name, ".json" for ".fits", holding the
  record. When rp looks a frame up on disk it serves the pair only when
  both files confirm the document: the FITS header's DOC_ID confirms
  the pixels and the sidecar's id confirms the record. A FITS file
  whose DOC_ID is missing or cannot be read is refused, since nothing
  confirms its pixels. A lookup by document_id that finds no confirmed
  pair answers 404. A lookup by path (plate_solve's image_path) that
  finds no confirmed pair has no record: the file is still solved, and
  no sidecar is written.

  Everything after a lookup works on the files it found, never on the
  file_path the sidecar recorded at capture: that path says where the
  frame was written, not where it is now. So a frame whose pair was
  copied or moved into a new data directory is solved, measured and
  reported at its new place, and a solve's wcs goes into the sidecar
  beside the file that was solved, inside the data directory or not.
  (rp.md § Document Resolution.)

  Background:
    Given a running Alpaca simulator
    And rp's data_directory is pinned to a fresh tempdir

  Scenario: A frame whose sidecar holds another frame's record is not found by document_id
    Given rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I remember the captured document_id as "first"
    And the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I remember the captured document_id as "second"
    And the sidecar of frame "first" is replaced by the sidecar of frame "second"
    And rp is restarted
    And I fetch the document for remembered document_id "first"
    Then the document response status should be 404

  Scenario Outline: A frame whose FITS DOC_ID cannot be confirmed is not found by document_id
    Given rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I remember the captured document_id as "first"
    And the FITS file of frame "first" is altered so that <alteration>
    And rp is restarted
    And I fetch the document for remembered document_id "first"
    Then the document response status should be 404

    Examples:
      | alteration                   |
      | it has no DOC_ID card        |
      | its DOC_ID card is a number  |
      | it is no longer a FITS file  |

  Scenario: A frame copied into a new data directory is reported and solved at its new place
    Given a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the captured frame's files are copied into a new data directory
    And rp is restarted
    And I fetch the image metadata for the captured document_id
    And the MCP client calls "plate_solve" with the captured document_id
    Then the image metadata fits_path should be the relocated FITS file
    And the stub plate solver should have received a request whose fits_path is the relocated FITS file
    And the relocated sidecar should contain a section named "wcs"
    And the original sidecar should not contain a section named "wcs"

  Scenario: A frame moved into a new data directory that the cache cannot hold is measured at its new place
    Given rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the sidecar of the captured frame records max_adu as null
    And the captured frame's files are moved into a new data directory
    And rp is restarted
    And the MCP client calls "compute_image_stats" with the captured document_id
    Then the tool call should succeed

  Scenario: A solve by path writes its wcs beside the file it solved, outside the data directory
    Given a stub plate solver returning a canned WCS
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And the captured frame's files are copied into an archive outside the data directory
    And the MCP client calls "plate_solve" with the relocated image path
    Then the relocated sidecar should contain a section named "wcs"
    And the original sidecar should not contain a section named "wcs"

  # The mount gives both frames a recorded pointing, so a solve that
  # took the "first" sidecar as its record would not be blind.
  Scenario: A solve by path does not write into a sidecar that belongs to another frame
    Given a stub plate solver returning a canned WCS
    And rp is running with a camera and a mount on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I remember the captured document_id as "first"
    And the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I remember the captured document_id as "second"
    And the FITS file of frame "first" is replaced by the FITS file of frame "second"
    And the MCP client calls "plate_solve" with the image path of frame "first"
    Then the plate_solve result should contain "ra_center" with value 10.6848
    And the stub plate solver should have received a blind request with no position and no search radius
    And the sidecar of frame "first" should not contain a section named "wcs"
