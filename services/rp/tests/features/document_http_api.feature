@serial
Feature: Document HTTP API
  The /api/documents/{document_id} route exposes the exposure document --
  capture metadata plus any sections that image-analysis tools or plugins
  have written. The route goes through the image cache, which on a miss
  scans the configured data directory for the matching FITS+sidecar pair
  and rehydrates them. The contract operators see is "live as long as the
  files are on disk", not "live as long as rp is up": a document remains
  reachable after eviction or even rp restart, as long as its FITS and
  sidecar pair sits under the data directory — flat `<uuid8>.fits` from a
  capture with no frame_type, or `<rendered pattern>_<uuid8>.fits` inside
  the directory_pattern tree under a naming template. The scan walks the
  tree to the directory pattern's depth, and every frame ends in its
  document's UUID-8 regardless of the pattern, which is what it keys on.

  Scenario: Document body shape after capture
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I fetch the document for the captured document_id
    Then the document response status should be 200
    And the document body should contain "id"
    And the document body should contain "captured_at"
    And the document body should contain "file_path"
    And the document body should contain "width"
    And the document body should contain "height"
    And the document body should contain "camera_id"
    And the document body should contain "duration"
    And the document body should contain "max_adu"

  Scenario: Document returns 404 for unknown id
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    When I fetch the document for document_id "00000000-0000-0000-0000-000000000000"
    Then the document response status should be 404

  Scenario: Section round-trip via measure_basic
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_basic" with the captured document_id
    And I fetch the document for the captured document_id
    Then the document response status should be 200
    And the document sections should contain "image_analysis"

  Scenario: Document survives cache eviction via on-disk fallback
    Given a running Alpaca simulator
    And rp's image cache holds at most 1 image
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I remember the captured document_id as "first"
    And the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I fetch the document for remembered document_id "first"
    Then the document response status should be 200
    And the document body should contain "id"

  Scenario: A frame captured under a naming template survives cache eviction via on-disk fallback
    Given a running Alpaca simulator
    And rp's image cache holds at most 1 image
    And rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms and frame_type "Dark"
    And I remember the captured document_id as "first"
    And the MCP client calls "capture" with camera "main-cam" for 1000 ms and frame_type "Dark"
    And I fetch the document for remembered document_id "first"
    Then the document response status should be 200
    And the document body should contain "file_path"

  Scenario: Document survives rp restart via on-disk fallback
    Given a running Alpaca simulator
    And rp's data_directory is pinned to a fresh tempdir
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I remember the captured document_id as "first"
    And rp is restarted
    And I fetch the document for remembered document_id "first"
    Then the document response status should be 200
    And the document body should contain "id"
    And the document body should contain "file_path"
