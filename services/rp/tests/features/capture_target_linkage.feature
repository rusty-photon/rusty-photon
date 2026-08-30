@serial
Feature: Capture threads target identity into the file-naming template (Decision 11)
  `capture` accepts two optional parameters: `target` (a slug) and
  `frame_type` (`Light`/`Dark`/`Flat`/`Bias`). `frame_type` stamps the
  exposure document's `target`/`frame_type` fields; omitted, both stay
  unset and `capture` writes a flat `<doc_uuid_8>.fits` regardless of
  `target`, exactly as before Decision 11 landed.

  The templated on-disk path is a **separate** switch:
  `session.file_naming_pattern`. Configured, `capture` renders
  `session.directory_pattern` (defaulting to
  `"{target}/{night_date}/{frame_type}"`) then `file_naming_pattern`
  into the final path, then appends the document's UUID-8 after the
  rendered filename (`<pattern>_<uuid8>.fits`): the suffix is rp's
  reverse-lookup key, not a template token. Unconfigured, a
  `frame_type` capture still
  records what the frame is and what it is of — it just keeps the flat
  name, and with nothing on disk to attribute, derives no progress
  (rp.md § Progress derivation).

  `frame_type: Light` requires `target`: the slug is resolved against
  the target store, an unknown slug errors, and the resolved
  `slug`/`display_name`/`ra_hours`/`dec_degrees` are denormalized onto
  the exposure document's `target` field. `Dark`/`Flat`/`Bias` frames
  don't image a sky object, so an omitted `target` falls back to a
  reserved slug equal to the lowercased frame type (`"dark"`/`"flat"`/
  `"bias"`) — a shared bucket per calibration type, `display_name`/
  `ra_hours`/`dec_degrees` left unset since it names no real
  target-store row. `{frame_number}` is derived, not stored: `capture`
  scans the target's directory for existing frames sharing the same
  `(filter, binning, exposure_duration)` sub-spec and uses `count + 1`.

  `{filter}`/`{filter_position}` read the resolved camera's train
  filter wheel live for `Light`/`Flat`; `Dark`/`Bias` always render the
  fixed `"NA"`/`0` regardless of whether a wheel is present, and any
  frame type falls back to the same `"NA"`/`0` when the train has no
  filter wheel at all.

  See docs/services/rp.md § Capture Tool Details and
  docs/crates/rp-targets.md § File-naming template for the full
  contract.

  Background:
    Given a running Alpaca simulator

  Scenario: frame_type is omitted and capture keeps writing a flat filename
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 100 ms
    Then the tool call should succeed
    And the captured image_path should be a flat uuid8-named .fits file

  Scenario: frame_type without a configured naming pattern still stamps the document
    Given rp is running with a capture rig and no naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms and frame_type "Dark"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the captured image_path should be a flat uuid8-named .fits file
    And the document field "frame_type" should be "Dark"
    And the document's target slug should be "dark"

  Scenario: A Light frame requires a target
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms and frame_type "Light"
    Then the tool call should return an error
    And the error message should contain "target"

  Scenario: A Light frame with an unknown target slug is rejected
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms, target "does-not-exist", and frame_type "Light"
    Then the tool call should return an error
    And the error message should contain "does-not-exist"

  Scenario: A Light frame with a known target renders a templated path and denormalizes the document target
    Given rp is running with a capture rig and naming templates configured
    And the MCP client has added a target named "M33" at ra_hours 1.4642 dec_degrees 30.6602
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms, the added target, and frame_type "Light"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the captured image_path should exist on disk
    And the captured image_path should contain "/Light/"
    And the captured image_path should end in the document's uuid8 suffix
    And the document field "frame_type" should be "Light"
    And the document's target slug should be "m33"
    And the document's target display_name should be "M33"

  Scenario: A Dark frame with no target uses the reserved "dark" slug
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms and frame_type "Dark"
    And I fetch the document for the captured document_id
    Then the tool call should succeed
    And the captured image_path should contain "/dark/"
    And the captured image_path should contain "/Dark/"
    And the document field "frame_type" should be "Dark"
    And the document's target slug should be "dark"
    And the document's target display_name should be absent

  Scenario: A Dark frame always renders "NA" for filter, even with a filter wheel present
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms and frame_type "Dark"
    Then the tool call should succeed
    And the captured image_path should contain "_NA_"

  Scenario: A camera with no filter wheel renders "NA" for a Light frame's filter
    Given rp is running with a filter-wheel-less capture rig and naming templates configured
    And the MCP client has added a target named "M33" at ra_hours 1.4642 dec_degrees 30.6602
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms, the added target, and frame_type "Light"
    Then the tool call should succeed
    And the captured image_path should contain "_NA_"

  Scenario: Two captures in the same sub-spec get incrementing frame numbers
    Given rp is running with a capture rig and naming templates configured
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms and frame_type "Dark"
    Then the tool call should succeed
    And the captured image_path should contain "_0001_"
    When the MCP client calls "capture" with camera "main-cam" for 1000 ms and frame_type "Dark"
    Then the tool call should succeed
    And the captured image_path should contain "_0002_"
