Feature: Targets inbox
  /targets is the operator surface for rp's target store: pending (paused)
  targets — typically planetarium imports — await review in the inbox,
  active targets sit in the roster section, and the per-target review page
  edits display name, priority, notes, the framing position angle, and the
  acquisition goals, then activates the target into the planner's rotation
  or discards it. Target CRUD is MCP-only on rp, so every read and write
  here rides rp's target tools over a per-request rp-mcp-client session;
  the filter roster and train position-angle defaults join in from rp's
  config. The position-angle field keeps "inherit the train default"
  (blank) and "explicit 0 north-up" distinguishable: blank submits an
  explicit null (clear back to inherit), "0" submits the number 0.0.

  Scenario: A pending import awaits review with its provenance shown
    Given a running rp orchestrator with an empty roster
    And a planetarium import is pending at RA 2.0 hours Dec -10.5 degrees from client "couch-ipad"
    And a BFF pointed at rp
    When I open the targets page
    Then the inbox section lists 1 pending target
    And the pending row provenance mentions "Imported via planetarium-bridge from couch-ipad at "
    And the pending row provenance mentions "planetarium-bridge"
    And the pending row shows position angle "inherit"

  Scenario: Active targets sit in the roster section, not the inbox
    Given a running rp orchestrator with an empty roster
    And I have added the active target "M 81"
    And I have added the pending target "M 82"
    And a BFF pointed at rp
    When I open the targets page
    Then the inbox section lists 1 pending target
    And the inbox section contains "M 82"
    And the active section contains "M 81"
    And the active row for "m-81" offers no discard button

  Scenario: An empty store points the operator at the import path
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the targets page
    Then the targets empty state mentions "Align"
    And the rendered output matches the "targets_empty" snapshot

  Scenario: The review page for an unknown slug says there is no such target
    Given a running rp orchestrator with an empty roster
    And a BFF pointed at rp
    When I open the review page for "no-such-target"
    Then the page shows an error banner mentioning "no target"

  Scenario: The review form of an inheriting target shows a blank angle and the train default
    Given a running rp orchestrator with an imaging train whose default position angle is 254.0
    And I have added the pending target "M 82"
    And a BFF pointed at rp
    When I open the review page for "m-82"
    Then the position angle field is blank
    And the position angle hint mentions "main: 254.0"

  Scenario: Saving a zero angle stores an explicit north-up, not inherit
    Given a running rp orchestrator with an empty roster
    And I have added the pending target "M 82"
    And a BFF pointed at rp
    When I open the review page for "m-82"
    And I save the review form with position angle "0"
    Then the stored position angle of "m-82" is 0.0
    And the position angle field shows "0"

  Scenario: Clearing the angle field returns the target to inherit
    Given a running rp orchestrator with an empty roster
    And I have added the pending target "M 82" with position angle 121.25
    And a BFF pointed at rp
    When I open the review page for "m-82"
    And I save the review form with position angle ""
    Then the stored position angle of "m-82" is cleared
    And the position angle field is blank

  Scenario: A non-numeric angle re-renders the form with the input preserved
    Given a running rp orchestrator with an empty roster
    And I have added the pending target "M 82"
    And a BFF pointed at rp
    When I open the review page for "m-82"
    And I save the review form with position angle "north"
    Then the position angle field has an error mentioning "not a number"
    And the position angle field shows "north"
    And the stored position angle of "m-82" is cleared

  Scenario: An out-of-range angle surfaces rp's own domain error on the field
    Given a running rp orchestrator with an empty roster
    And I have added the pending target "M 82"
    And a BFF pointed at rp
    When I open the review page for "m-82"
    And I save the review form with position angle "360"
    Then the position angle field has an error mentioning "below 360"
    And the position angle field shows "360"
    And the stored position angle of "m-82" is cleared

  Scenario: Editing goals replaces the plan
    Given a running rp orchestrator with an empty roster
    And I have added the pending target "M 82"
    And a BFF pointed at rp
    When I open the review page for "m-82"
    And I save the review form with a goal of 12 frames in filter "Ha" binning "1x1" exposing "5m"
    Then the stored goals of "m-82" have 1 entry
    And the stored goal 0 of "m-82" wants 12 frames in filter "Ha"
    And the review page lists a goal mentioning "Ha"

  Scenario: A goal filter missing from the rig's roster is flagged
    Given a running rp orchestrator whose filter roster is "Ha" and "OIII"
    And I have added the pending target "M 82" with a goal in filter "Ha"
    And rp restarts with a filter roster of only "OIII"
    And a BFF pointed at rp
    When I open the targets page
    Then the goal flag for "m-82" mentions "not in the rig's filter roster"

  Scenario: A goal filter present in the rig's roster is not flagged
    Given a running rp orchestrator whose filter roster is "Ha" and "OIII"
    And I have added the pending target "M 82" with a goal in filter "Ha"
    And a BFF pointed at rp
    When I open the targets page
    Then no goal flag is rendered

  Scenario: Activating a pending target moves it into the active roster
    Given a running rp orchestrator with an empty roster
    And I have added the pending target "M 82"
    And a BFF pointed at rp
    When I open the targets page
    And I press the activate button of "m-82"
    Then the stored target "m-82" is active
    When I open the targets page
    Then the active section contains "M 82"

  Scenario: Pausing an active target returns it to the inbox
    Given a running rp orchestrator with an empty roster
    And I have added the active target "M 81"
    And a BFF pointed at rp
    When I open the targets page
    And I press the pause button of "m-81"
    Then the stored target "m-81" is paused
    When I open the targets page
    Then the inbox section contains "M 81"

  Scenario: Discarding a pending target removes it from the store
    Given a running rp orchestrator with an empty roster
    And I have added the pending target "M 82"
    And a BFF pointed at rp
    When I open the targets page
    And I press the discard button of "m-82"
    Then the store no longer holds "m-82"

  # The target tools are ungated (rp.md § Safety → In-Flight Tool Calls),
  # so an unreachable rp is an outage, never a safety refusal in disguise.
  Scenario: The unavailable card names the outage
    Given a BFF pointed at an unreachable rp
    When I open the targets page
    Then the targets unavailable card mentions "down or unreachable"
    And the targets unavailable card offers a retry link
