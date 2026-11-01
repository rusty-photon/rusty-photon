@serial
Feature: Per-star photometry and PSF metrics
  The measure_stars MCP tool measures the stars measure_basic measures -- one
  per aperture, each with its aperture half-flux radius and flux, listed
  brightest first -- and fits a 2D Gaussian to each to report FWHM and
  eccentricity, plus the aggregate HFR measure_basic reports, median
  aggregates, and the sigma-clipped background. It accepts either
  document_id (resolved via the image cache, falling back to FITS on miss) or
  image_path (read from disk). When called with document_id, results are
  written into the exposure document as a "measured_stars" section -- distinct
  from "detected_stars", "image_analysis", and "background" so all four tools
  coexist on one document.

  Scenario: Tool catalog includes measure_stars
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "measure_stars"

  Scenario: Returns contract fields after capture using image_path
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_stars" with the captured image path
    Then the measure_stars result should contain "stars" as an array
    And the measure_stars result should contain "star_count" as a non-negative integer
    And the measure_stars result should contain "background_mean" as a non-negative number
    And the measure_stars result should contain "background_stddev" as a non-negative number
    And the measure_stars result should contain "hfr"
    And the measure_stars result should contain "median_fwhm"
    And the measure_stars result should contain "median_hfr"

  Scenario: Returns contract fields when called with document_id
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_stars" with the captured document_id
    Then the measure_stars result should contain "stars" as an array
    And the measure_stars result should contain "star_count" as a non-negative integer
    And the measure_stars result should contain "background_mean" as a non-negative number

  Scenario: Persists measured_stars section into the exposure document
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_stars" with the captured document_id
    And I fetch the exposure document for the captured document_id
    Then the exposure document should contain a section named "measured_stars"
    And the "measured_stars" section should contain "stars"
    And the "measured_stars" section should contain "star_count"
    And the "measured_stars" section should contain "background_mean"

  Scenario: Very high threshold yields empty stars list with null medians
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_stars" with the captured image path and threshold_sigma 1000.0
    Then the measure_stars result should contain "star_count" with value 0
    And the measure_stars result should contain "stars" as an empty array
    And the measure_stars result should contain "hfr" with value null
    And the measure_stars result should contain "median_fwhm" with value null
    And the measure_stars result should contain "median_hfr" with value null

  Scenario: measure_stars with nonexistent image path returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" with image path "/nonexistent/image.fits"
    Then the tool call should return an error

  Scenario: measure_stars with neither document_id nor image_path returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" with no arguments
    Then the tool call should return an error
    And the error message should contain "image_path"

  Scenario: measure_stars with unknown document_id returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" with document_id "unknown-doc-id"
    Then the tool call should return an error

  Scenario: measure_stars with image_path but missing min_area returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_stars" with the captured image path but no min_area
    Then the tool call should return an error
    And the error message should contain "min_area"

  Scenario: measure_stars with zero stamp_half_size returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "measure_stars" with the captured image path and stamp_half_size 0
    Then the tool call should return an error
    And the error message should contain "stamp_half_size"

  Scenario: Stars in a real frame are measured in apertures and listed brightest first
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with threshold_sigma 5.0, min_area 4 and max_area 400
    Then the measure_stars result should contain "star_count" with value 12
    And the measure_stars result should contain "hfr" between 1.1 and 1.2
    And the measure_stars stars should be listed brightest first

  Scenario: A defocused star that fragments at the threshold is one star
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_defocused_crop.fits" with threshold_sigma 8.0, min_area 6 and max_area 5000
    Then the measure_stars result should contain "star_count" with value 1
    And the measure_stars result should contain "hfr" between 37.5 and 39.0
