@serial
Feature: Basic image measurement tool
  The measure_basic MCP tool detects stars in an image and reports aggregate
  HFR, star count, and sigma-clipped background statistics. It accepts either
  document_id (resolved via the image cache, falling back to the FITS file on
  miss) or image_path (read from disk). When called with document_id, results
  are written into the exposure document as an "image_analysis" section. With
  no stars detected the tool succeeds with hfr null, star_count zero, and
  populated background fields -- the caller decides whether that is a failure.

  Hot pixels are not stars. Before smoothing, a pixel above the detection
  threshold is replaced by the median of its 3x3 neighbourhood, unless a 3x3
  window that contains it has a median more than 2 background deviations
  above the background mean (or above the threshold, if that is lower), as
  windows in and around a star do.
  After measuring, a component whose half-flux radius is under 0.25 px is
  rejected. The real-frame scenario runs on a 512x512 crop of a QHY600M
  best-focus frame, whose twelve stars sit among the sensor's hot pixels.

  Scenario: Tool catalog includes measure_basic
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "measure_basic"

  Scenario: Returns contract fields after capture using image_path
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_basic" with the captured image path
    Then the measure_basic result should contain "hfr"
    And the measure_basic result should contain "star_count" as a non-negative integer
    And the measure_basic result should contain "background_mean" as a non-negative number
    And the measure_basic result should contain "background_stddev" as a non-negative number
    And the measure_basic result should contain "pixel_count" as a positive integer

  Scenario: Returns contract fields when called with document_id
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_basic" with the captured document_id
    Then the measure_basic result should contain "hfr"
    And the measure_basic result should contain "star_count" as a non-negative integer
    And the measure_basic result should contain "background_mean" as a non-negative number
    And the measure_basic result should contain "background_stddev" as a non-negative number

  Scenario: Persists image_analysis section into the exposure document
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_basic" with the captured document_id
    And I fetch the exposure document for the captured document_id
    Then the exposure document should contain a section named "image_analysis"
    And the "image_analysis" section should contain "hfr"
    And the "image_analysis" section should contain "star_count"
    And the "image_analysis" section should contain "background_mean"
    And the "image_analysis" section should contain "background_stddev"

  Scenario: Very high threshold yields zero stars with populated background
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_basic" with the captured image path and threshold_sigma 1000.0
    Then the measure_basic result should contain "hfr" with value null
    And the measure_basic result should contain "star_count" with value 0
    And the measure_basic result should contain "background_mean" as a non-negative number
    And the measure_basic result should contain "background_stddev" as a non-negative number

  Scenario: measure_basic with nonexistent image path returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_basic" with image path "/nonexistent/image.fits"
    Then the tool call should return an error

  Scenario: measure_basic with neither document_id nor image_path returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_basic" with no arguments
    Then the tool call should return an error
    And the error message should contain "image_path"

  Scenario: measure_basic with unknown document_id returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_basic" with document_id "unknown-doc-id"
    Then the tool call should return an error

  Scenario: Hot pixels in a real frame are not counted as stars
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_basic" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with threshold_sigma 5.0, min_area 4 and max_area 400
    Then the measure_basic result should contain "star_count" with value 12
    And the measure_basic result should contain "hfr" between 1.0 and 1.1
