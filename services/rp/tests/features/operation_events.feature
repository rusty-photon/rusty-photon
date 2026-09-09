@serial
Feature: Operation event envelopes
  Every blocking MCP operation emits a uniform event envelope: a
  `*_started` event at entry and a matching `*_complete` or `*_failed`
  event at exit, all sharing one `operation_id` so a consumer can
  correlate the triple. Each emission carries its own `event_id` and a
  monotonic `event_seq`; the start carries `started_at` and the end adds
  `ended_at` plus `elapsed_ms`. The `slew_started`, `park_started`,
  `move_focuser_started`, and `exposure_started` events carry the
  `predicted_duration_ms` / `max_duration_ms` deadline fields (Phase 2) when
  the deadline can be predicted, and omit them on the fallback path (e.g. a
  pre-read failure, or no mount configured for park); operations not yet
  converted to predictive deadlines always omit them. The exposure deadline
  is sized from the exposure duration plus the camera's
  `readout_time_estimate` (a built-in default when unset) and a readout
  headroom; rp does not enforce it (the camera driver does), so it always
  rides `exposure_started`.

  The envelope is additive over the historical webhook body: the
  `event_id`, `event`, `timestamp`, and `payload` keys keep their exact
  meaning, so the started payload carries the operation inputs and the
  complete payload carries the same outcome keys webhook plugins already
  received.

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

  Scenario: Capture migrates the legacy exposure events onto one operation_id
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
