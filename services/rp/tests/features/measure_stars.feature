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

  Called with a list of stars instead -- the x, y and radius of each, the shape of
  its own stars output -- it detects nothing and measures those stars where
  they were last seen: each aperture starts at the star's last centre and
  radius, starts again at twice the radius while it finds no star, up to the
  size cap, and follows the star. A listed star counts as measured when its
  aperture settles as a star, it is not inside a brighter listed star's
  aperture, and its SNR exceeds 3. The stars come back in the order given; one
  not measured keeps its x, y and radius with a null hfr, so the array can be
  passed to the next frame unchanged. The aggregate hfr is the median over
  every listed star measured, and min_area is not needed.

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

  Scenario: A defocused star the threshold keeps whole is one star
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_defocused_crop.fits" with threshold_sigma 5.0, min_area 6 and max_area 5000
    Then the measure_stars result should contain "star_count" with value 1
    And the measure_stars result should contain "hfr" between 37.5 and 39.0

  Scenario: Listed stars are measured where they were last seen, in the order given
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with threshold_sigma 5.0, min_area 4 and max_area 400
    And the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with max_area 400 and the stars that call returned
    Then the measure_stars result should contain "star_count" with value 12
    And the measure_stars result should contain "hfr" between 1.1 and 1.2
    And every measure_stars star should lie within 0.1 px of the star listed in its place
    And every measured measure_stars star should have a radius of at least 3.0 and an snr above 3.0

  Scenario: A listed place with no star there is not measured and keeps its place
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with max_area 400 and the stars:
      | x     | y     | radius |
      | 250.0 | 180.0 | 3.0    |
      | 600.0 | 100.0 | 4.5    |
    Then the measure_stars result should contain "star_count" with value 0
    And the measure_stars result should contain "hfr" with value null
    And the measure_stars stars should be, in order, unmeasured at:
      | x     | y     | radius |
      | 250.0 | 180.0 | 3.0    |
      | 600.0 | 100.0 | 4.5    |

  Scenario: A defocused star is found from a small aperture at its centre
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_defocused_crop.fits" with max_area 5000 and the stars:
      | x     | y     | radius |
      | 256.0 | 256.0 | 3.0    |
    Then the measure_stars result should contain "star_count" with value 1
    And the measure_stars result should contain "hfr" between 37.5 and 39.0

  Scenario: Two listed stars on one defocused disc count once
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_defocused_crop.fits" with max_area 5000 and the stars:
      | x     | y     | radius |
      | 256.0 | 256.0 | 3.0    |
      | 230.0 | 256.0 | 3.0    |
    Then the measure_stars result should contain "star_count" with value 1
    And exactly 1 of the 2 measure_stars stars should be measured

  Scenario: An empty list measures nothing
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with max_area 400 and no stars
    Then the measure_stars result should contain "star_count" with value 0
    And the measure_stars result should contain "stars" as an empty array
    And the measure_stars result should contain "hfr" with value null

  Scenario: measure_stars with stars but missing max_area returns error
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with no max_area and the stars:
      | x     | y     | radius |
      | 134.6 | 138.7 | 4.0    |
    Then the tool call should return an error
    And the error message should contain "max_area"

  Scenario: A listed star with a radius that is not positive is refused, naming its place
    Given a running Alpaca simulator
    And rp is running with a camera on the simulator
    And an MCP client connected to rp
    When the MCP client calls "measure_stars" on the fixture "star_detection/qhy600m_best_focus_crop.fits" with max_area 400 and the stars:
      | x     | y     | radius |
      | 134.6 | 138.7 | 4.0    |
      | 340.5 | 334.9 | 0.0    |
    Then the tool call should return an error
    And the error message should contain "stars[1]"
    And the error message should contain "radius"
