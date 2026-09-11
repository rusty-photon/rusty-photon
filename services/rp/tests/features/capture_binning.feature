@serial
Feature: Capture sets the camera's frame geometry before every exposure
  `capture`, `auto_focus` and `center_on_target` take an optional
  `binning` parameter spelled `"AxB"` — the same string an acquisition
  goal is keyed by — so a goal that asks for `"2x2"` and the capture
  that fulfils it are written the same way. Omitted, it means `"1x1"`.

  On a train-addressed `auto_focus` the train's `auto_focus.binning`
  is the default and a per-call `binning` overrides it, the same
  field-by-field merge every other sweep parameter gets; `refocus_train`
  takes the block only, since it addresses no call.

  `rp` writes the geometry to the camera before every exposure it
  starts, the omitted-parameter `1x1` case included, and never inherits
  the state it finds the camera in. A camera is shared equipment:
  another client can leave it binned or cropped, and inheriting that
  would record light frames against a goal bucket nobody asked for.
  That covers what the camera was already in when the capture started;
  a second capture arriving through the same camera *while* this one
  runs is not covered, and `rp` does not serialize those. Four
  properties are written,
  in this order: the binning factors, then the subframe origin at
  0 by 0, then the subframe size at the sensor size divided by the
  binning. The subframe write is not bookkeeping — ASCOM does not
  require a driver to rescale its subframe when the binning changes,
  and the reference simulator does not, so an exposure at a fresh
  binning fails outright without it.

  A binning the camera cannot do is a parameter error before anything
  is written, checked against the `MaxBinX`/`MaxBinY`/
  `CanAsymmetricBin` capabilities cached at connect time. Geometry is
  applied before `exposure_started` is emitted, so a rejected binning
  produces no exposure events at all.

  After the write `capture` reads the binning back, before it sizes the
  subframe. A camera that ends up at a different binning than it was
  set to fails the capture rather than exposing: sizing the subframe
  from factors the sensor is not at would write a crop, and a goal is
  keyed by binning, so a frame at a binning nobody asked for is worse
  than no frame. The value read back is what the exposure document and
  the `{binning}` filename token record.

  See docs/services/rp.md § Capture Tool Details, "Binning".

  Background:
    Given a running Alpaca simulator

  Scenario: A capture at 2x2 halves the frame and records the binning it ran at
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 100 ms at binning "2x2"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document field "binning" should be "2x2"
    And the captured frame should be 400 by 300 pixels

  Scenario: A capture with no binning records 1x1 and the full frame
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document field "binning" should be "1x1"
    And the captured frame should be 800 by 600 pixels

  Scenario: A capture with no binning resets a camera another client left binned
    Given rp is running with a capture rig and naming templates configured
    And another client has binned the simulator camera to "2x2"
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the document field "binning" should be "1x1"
    And the captured frame should be 800 by 600 pixels

  Scenario: A capture restores the full frame a foreign subframe had cropped
    Given rp is running with a capture rig and naming templates configured
    And another client has cropped the simulator camera to a 200 by 150 subframe
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the captured frame should be 800 by 600 pixels

  Scenario: The binning a frame ran at is the one its filename carries
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms at binning "2x2" and frame_type "Dark"
    Then the tool call should succeed
    And the captured image_path should contain "_2x2_"

  Scenario: A capture at a binning above the camera's maximum is rejected
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 100 ms at binning "5x5"
    Then the tool call should return an error
    And the error message should contain "bins at most 4 on x"

  Scenario: A capture at a zero binning factor is rejected
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 100 ms at binning "0x0"
    Then the tool call should return an error
    And the error message should contain "at least 1x1"

  Scenario: A binning that is not two factors is rejected
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 100 ms at binning "half"
    Then the tool call should return an error

  Scenario: A 2x2 capture makes progress against a 2x2 goal
    Given rp is running with a filter-wheel-less capture rig and naming templates configured
    And the MCP client has added a target named "M33" at ra_hours 1.4642 dec_degrees 30.6602
    And the MCP client has set its goals to:
      | filter | binning | exposure_duration | desired_count |
      |        | 2x2     | 1s                | 10            |
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms at binning "2x2", the added target, and frame_type "Light"
    Then the tool call should succeed
    When the MCP client calls "get_target" for slug "m33"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter | binning | exposure_duration | good | total | desired_count |
      |        | 2x2     | 1s                | 1    | 1     | 10            |

  Scenario: A per-call binning overrides the train's
    Given rp's data_directory is pinned to a fresh tempdir
    And rp is running with a camera and a focuser on the simulator in train "main" with the standard auto_focus block at binning "1x1"
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main" and binning "2x2"
    Then 5 FITS files should exist in the pinned data directory
    And every sidecar JSON in the pinned data directory should report binning "2x2"

  Scenario: A refocus sweep captures at the binning its train configures
    Given rp's data_directory is pinned to a fresh tempdir
    And rp is running with a camera and a focuser on the simulator in train "main" with the standard auto_focus block at binning "2x2"
    And an MCP client connected to rp
    When the MCP client calls "refocus_train" with train "main"
    Then 5 FITS files should exist in the pinned data directory
    And every sidecar JSON in the pinned data directory should report binning "2x2"

  Scenario: An auto_focus sweep captures at the binning its train configures
    Given rp's data_directory is pinned to a fresh tempdir
    And rp is running with a camera and a focuser on the simulator in train "main" with the standard auto_focus block at binning "2x2"
    And an MCP client connected to rp
    When the MCP client calls auto_focus with train "main"
    Then 5 FITS files should exist in the pinned data directory
    And every sidecar JSON in the pinned data directory should report binning "2x2"
