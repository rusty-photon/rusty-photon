@serial
Feature: Image HTTP API
  The /api/images/{document_id} routes expose an exposure's image metadata
  and raw pixel data to plugins, frontends, and external consumers. Metadata
  is returned as JSON. Pixel data is served in ASCOM Alpaca ImageBytes wire
  format (application/imagebytes -- a 44-byte header of eleven little-endian
  i32 fields followed by raw little-endian pixel bytes). Both routes return
  404 when the document_id is unknown. The pixel route prefers the
  in-memory image cache and falls back to reading the FITS file from disk
  on cache miss; consumers are not expected to know which path served the
  bytes. Pixels travel width-major, x varying slowest, exactly as an
  Alpaca camera sends them; the FITS file on disk holds the same picture
  row-major (NAXIS1 = width is the fast axis), so pixel (x, y) on the
  wire is FITS row y, column x.

  Scenario: Image metadata after capture
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I fetch the image metadata for the captured document_id
    Then the image metadata response status should be 200
    And the image metadata should contain "document_id"
    And the image metadata should contain "width" as a positive integer
    And the image metadata should contain "height" as a positive integer
    And the image metadata should contain "bitpix" with value 16
    And the image metadata should contain "fits_path"
    And the image metadata should contain "in_cache" with value true
    And the image metadata should contain "document_url"

  Scenario: Image pixels after capture include valid ImageBytes header
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I fetch the image pixels for the captured document_id
    Then the image pixels response status should be 200
    And the image pixels content-type should be "application/imagebytes"
    And the image pixels header should match these constants (i32 little-endian):
      | field                     | offset | value |
      | metadata_version          | 0      | 1     |
      | data_start                | 16     | 44    |
      | image_element_type        | 20     | 2     |
      | transmission_element_type | 24     | 8     |
      | rank                      | 28     | 2     |

  Scenario: Image pixels carry the FITS picture in Alpaca width-major order
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And I fetch the image metadata for the captured document_id
    And I fetch the image pixels for the captured document_id
    Then the image pixels dimension_1 and dimension_2 should equal NAXIS1 and NAXIS2 of the FITS file at "fits_path"
    And the image pixel at (x, y) should equal the FITS pixel at row y column x for every x and y

  Scenario: Image metadata returns 404 for unknown document_id
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    When I fetch the image metadata for document_id "unknown-doc-id"
    Then the image metadata response status should be 404

  Scenario: Image pixels returns 404 for unknown document_id
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    When I fetch the image pixels for document_id "unknown-doc-id"
    Then the image pixels response status should be 404
