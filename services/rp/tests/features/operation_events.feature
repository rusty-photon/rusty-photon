@serial
Feature: Operation event envelopes
  Every blocking MCP operation emits a uniform event envelope: a
  `*_started` event at entry and a matching `*_complete` or `*_failed`
  event at exit, all sharing one `operation_id` so a consumer can
  correlate the triple. Each emission carries its own `event_id` and a
  monotonic `event_seq`; the start carries `started_at` and the end adds
  `ended_at` plus `elapsed_ms`. The `slew_started`, `park_started`,
  `move_focuser_started`, `exposure_started` and `centering_started`
  events carry the `predicted_duration_ms` / `max_duration_ms` deadline
  fields when the deadline can be predicted (as do `guide_started` and
  `dither_started` when a settle timeout is resolved), and omit them on
  the fallback path (e.g. a
  pre-read failure, or no mount configured for park); an operation rp
  sizes no deadline for always omits them. The exposure deadline
  is sized from the exposure duration plus the camera's
  `readout_time_estimate` (a built-in default when unset) and a readout
  headroom; rp does not enforce it (the camera driver does), so it always
  rides `exposure_started`.

  The envelope adds to the body every event carries: the `event_id`,
  `event`, `timestamp`, and `payload` keys mean the same on an
  operation's events as on a point event, so the started payload
  carries the operation inputs and the complete payload its outcome.

  A cover move (`open_cover`, `close_cover`) names the resolved
  `calibrator_id` on every event of its triple, whether the call
  addressed the calibrator or its train, and its complete payload
  carries the cover `status` the tool returns. A cover move is bounded
  by a fixed ceiling, not a sized deadline, so its started event
  carries no deadline fields.

  Scenario: Slew emits a started and complete event under one operation_id
    Given a running Alpaca simulator
    And a webhook subscriber for the "slew" operation
    And rp is running with a mount and the operation-event plugin
    And an MCP client connected to rp
    And the mount is unparked
    And the mount tracking is set to true
    When the operator slews to ra 10.6847 dec 41.2689
    Then the webhook delivers the "slew_started" event
    And the webhook delivers the "slew_complete" event
    And the "slew_started" and "slew_complete" events share one operation_id
    And the "slew_started" and "slew_complete" events have distinct event_ids
    And the "slew_complete" event has a higher event_seq than the "slew_started" event
    And the "slew_started" event carries a started_at timestamp
    And the "slew_complete" event carries ended_at and elapsed_ms
    And the "slew_started" event carries the deadline fields
    And the "slew_started" event payload includes "ra"
    And the "slew_complete" event payload includes "actual_ra"

  Scenario: Slew failure emits a started and failed event under one operation_id
    Given a running Alpaca simulator
    And a webhook subscriber for the "slew" operation
    And rp is running with a mount and the operation-event plugin
    And an MCP client connected to rp
    And the mount is unparked
    And the mount tracking is set to false
    When the operator slews to ra 10.6847 dec 41.2689
    Then the webhook delivers the "slew_started" event
    And the webhook delivers the "slew_failed" event
    And the "slew_started" and "slew_failed" events share one operation_id
    And the "slew_failed" event carries ended_at and elapsed_ms
    And the "slew_failed" event payload includes "error"

  Scenario: Park emits a started and complete event under one operation_id
    Given a running Alpaca simulator
    And a webhook subscriber for the "park" operation
    And rp is running with a mount and the operation-event plugin
    And an MCP client connected to rp
    And the mount is unparked
    And the mount tracking is set to true
    When the operator parks the mount
    Then the webhook delivers the "park_started" event
    And the webhook delivers the "park_complete" event
    And the "park_started" and "park_complete" events share one operation_id
    And the "park_complete" event has a higher event_seq than the "park_started" event
    And the "park_started" event carries the deadline fields

  Scenario: Sync emits a complete event with no started event
    Given a running Alpaca simulator
    And a webhook subscriber for the "sync_mount" operation
    And rp is running with a mount and the operation-event plugin
    And an MCP client connected to rp
    And the mount tracking is set to true
    When the operator syncs the mount to ra 5.0 dec 10.0
    Then the webhook delivers the "sync_mount_complete" event
    And the webhook delivers no "sync_mount_started" event
    And the "sync_mount_complete" event carries ended_at and elapsed_ms
    And the "sync_mount_complete" event payload includes "ra"

  Scenario: Unpark emits a started and complete event under one operation_id
    Given a running Alpaca simulator
    And a webhook subscriber for the "unpark" operation
    And rp is running with a mount and the operation-event plugin
    And an MCP client connected to rp
    When the operator unparks the mount
    Then the webhook delivers the "unpark_started" event
    And the webhook delivers the "unpark_complete" event
    And the "unpark_started" and "unpark_complete" events share one operation_id

  Scenario: Capture emits a started and complete exposure event under one operation_id
    Given a running Alpaca simulator
    And a webhook subscriber for the "exposure" operation
    And rp is running with a camera and the operation-event plugin
    And an MCP client connected to rp
    When the operator captures a 200 ms frame on camera "main-cam"
    Then the webhook delivers the "exposure_started" event
    And the webhook delivers the "exposure_complete" event
    And the "exposure_started" and "exposure_complete" events share one operation_id
    And the "exposure_started" event carries the deadline fields
    And the "exposure_started" event payload includes "camera_id"
    And the "exposure_complete" event payload includes "document_id"
    And the "exposure_complete" event payload includes "file_path"

  Scenario: Move-focuser emits a started and complete event under one operation_id
    Given a running Alpaca simulator
    And a webhook subscriber for the "move_focuser" operation
    And rp is running with a focuser and the operation-event plugin
    And an MCP client connected to rp
    When the operator moves the focuser to position 25500
    Then the webhook delivers the "move_focuser_started" event
    And the webhook delivers the "move_focuser_complete" event
    And the "move_focuser_started" and "move_focuser_complete" events share one operation_id
    And the "move_focuser_started" event carries the deadline fields
    And the "move_focuser_started" event payload includes "focuser_id"
    And the "move_focuser_started" event payload includes "backlash_compensated"
    And the "move_focuser_complete" event payload includes "position"

  Scenario: Close-cover emits a started and complete event naming the resolved calibrator
    Given a running Alpaca simulator
    And a webhook subscriber for the "close_cover" operation
    And rp is running with a camera and a cover calibrator on the simulator in an imaging train
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with train "main"
    Then the webhook delivers the "close_cover_started" event
    And the webhook delivers the "close_cover_complete" event
    And the "close_cover_started" and "close_cover_complete" events share one operation_id
    And the "close_cover_complete" event has a higher event_seq than the "close_cover_started" event
    And the "close_cover_started" event carries a started_at timestamp
    And the "close_cover_complete" event carries ended_at and elapsed_ms
    And the "close_cover_started" event reserves the deadline fields as absent
    And the "close_cover_started" event payload field "calibrator_id" should be "flat-panel"
    And the "close_cover_complete" event payload field "calibrator_id" should be "flat-panel"
    And the "close_cover_complete" event payload field "status" should be "closed"

  Scenario: Open-cover emits a started and complete event under one operation_id
    Given a running Alpaca simulator
    And a webhook subscriber for the "open_cover" operation
    And rp is running with a cover calibrator on the simulator
    And an MCP client connected to rp
    When the MCP client calls "close_cover" with calibrator "flat-panel"
    And the MCP client calls "open_cover" with calibrator "flat-panel"
    Then the tool call should succeed
    And the webhook delivers the "open_cover_started" event
    And the webhook delivers the "open_cover_complete" event
    And the "open_cover_started" and "open_cover_complete" events share one operation_id
    And the "open_cover_complete" event has a higher event_seq than the "open_cover_started" event
    And the "open_cover_started" event reserves the deadline fields as absent
    And the "open_cover_started" event payload field "calibrator_id" should be "flat-panel"
    And the "open_cover_complete" event payload field "status" should be "open"
