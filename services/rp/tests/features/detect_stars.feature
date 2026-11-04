@serial
Feature: Star detection tool
  The detect_stars MCP tool returns the per-star list that measure_basic
  produces internally -- coordinates, flux, peak, and saturation flags --
  along with aggregate counts and the sigma-clipped background used to set
  the detection threshold. It accepts either document_id (resolved via the
  image cache, falling back to FITS on miss) or image_path (read from disk).
  When called with document_id, results are written into the exposure
  document as a "detected_stars" section -- separate from measure_basic's
  "image_analysis" and estimate_background's "background" so all three tools
  can coexist on one document.

  A pixel is saturated at or above the frame's saturation level: the frame's
  own clip, its maximum when at least five pixels hold it, or the camera's
  max_adu, the lower of the two.

  Scenario: Tool catalog includes detect_stars
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client lists available tools
    Then the tool list should include "detect_stars"

  Scenario: Returns contract fields after capture using image_path
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "detect_stars" with the captured image path
    Then the detect_stars result should contain "stars" as an array
    And the detect_stars result should contain "star_count" as a non-negative integer
    And the detect_stars result should contain "saturated_star_count" as a non-negative integer
    And the detect_stars result should contain "background_mean" as a non-negative number
    And the detect_stars result should contain "background_stddev" as a non-negative number

  Scenario: Returns contract fields when called with document_id
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "detect_stars" with the captured document_id
    Then the detect_stars result should contain "stars" as an array
    And the detect_stars result should contain "star_count" as a non-negative integer
    And the detect_stars result should contain "background_mean" as a non-negative number

  Scenario: Persists detected_stars section into the exposure document
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "detect_stars" with the captured document_id
    And I fetch the exposure document for the captured document_id
    Then the exposure document should contain a section named "detected_stars"
    And the "detected_stars" section should contain "stars"
    And the "detected_stars" section should contain "star_count"
    And the "detected_stars" section should contain "background_mean"

  Scenario: Very high threshold yields empty stars list with populated background
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "detect_stars" with the captured image path and threshold_sigma 1000.0
    Then the detect_stars result should contain "star_count" with value 0
    And the detect_stars result should contain "stars" as an empty array
    And the detect_stars result should contain "background_mean" as a non-negative number

  Scenario: detect_stars with nonexistent image path returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "detect_stars" with image path "/nonexistent/image.fits"
    Then the tool call should return an error

  Scenario: detect_stars with neither document_id nor image_path returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "detect_stars" with no arguments
    Then the tool call should return an error
    And the error message should contain "image_path"

  Scenario: detect_stars with unknown document_id returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "detect_stars" with document_id "unknown-doc-id"
    Then the tool call should return an error

  Scenario: detect_stars with image_path but missing min_area returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "detect_stars" with the captured image path but no min_area
    Then the tool call should return an error
    And the error message should contain "min_area"

  Scenario: detect_stars with image_path but missing max_area returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms
    And the MCP client calls "detect_stars" with the captured image path but no max_area
    Then the tool call should return an error
    And the error message should contain "max_area"

  Scenario: A star clipped at the frame's own ceiling is saturated without a max_adu
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "detect_stars" on the fixture "star_detection/qhy600m_saturated_crop.fits" with threshold_sigma 8.0
    Then the detect_stars result should contain "saturated_star_count" with value 1
    And the detect_stars stars should hold 65 saturated pixels in all

  Scenario: A frame that does not clip has no saturated star
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "detect_stars" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with threshold_sigma 5.0
    Then the detect_stars result should contain "saturated_star_count" with value 0
