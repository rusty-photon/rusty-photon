@serial
Feature: Captures through one camera run one at a time
  Every camera has a capture slot. A capture holds it from its first
  frame-geometry write until its image is downloaded, so the binning
  it wrote, the exposure it started and the frame it reads back are
  all its own. A `capture`, or an internal capture of `auto_focus`,
  `refocus_train` or `center_on_target`, through a camera whose slot
  is held waits its turn rather than failing, and waiters take the
  slot in arrival order. Captures through different cameras never wait
  for each other.

  The slot is released at download, not at persistence: writing the
  FITS file and the sidecar touches no camera, so the next exposure
  starts while the previous frame is still being written. A capture
  takes its camera's slot before the mount motion gate, so one queued
  behind a busy camera holds no gate permit while it waits. The wait
  is raced against the call's cancellation, and a queued call emits
  nothing — no event, no progress — until it holds the camera, so
  `exposure_started` and its deadline describe its own exposure rather
  than its time in the queue.

  The slot serializes rp's own captures. A client outside rp exposing
  through the same camera concurrently is not queued by it.

  See docs/services/rp.md § Capture Tool Details, "Binning" →
  Concurrency.

  Background:
    Given a running Alpaca simulator

  Scenario: Two overlapping captures through one camera each get the frame they asked for
    Given a test webhook receiver subscribed to the events "exposure_started"
    And rp is running with a camera on the simulator
    When a second MCP client starts a "3s" capture of camera "main-cam" at binning "2x2" in the background
    And the test webhook receiver has received an "exposure_started" event
    And the MCP client calls "capture" with camera "main-cam" for 100 ms at binning "1x1"
    Then the tool call should succeed
    And the background "capture" call should succeed
    When I fetch the document for the captured document_id
    Then the document field "binning" should be "1x1"
    And the captured frame should be 800 by 600 pixels
    When I fetch the document for the background capture
    Then the document field "binning" should be "2x2"
    And the captured frame should be 400 by 300 pixels
