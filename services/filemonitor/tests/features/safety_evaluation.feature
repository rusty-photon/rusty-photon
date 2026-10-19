Feature: Safety evaluation rules
  The safety monitor evaluates file content against configured parsing rules.
  Parsing rules are evaluated in order; the first match determines safety.
  No match defaults to unsafe.

  Scenario Outline: Contains rule evaluation
    Given case-insensitive matching
    And a contains rule with pattern "OPEN" that evaluates to safe
    And a contains rule with pattern "CLOSED" that evaluates to unsafe
    And a monitoring file containing "<content>"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return <expected>

    Examples:
      | content              | expected |
      | Roof Status: OPEN    | true     |
      | Roof Status: CLOSED  | false    |
      | roof status: open    | true     |
      | roof status: closed  | false    |
      | Unknown status       | false    |

  Scenario: Case-sensitive match succeeds with exact case
    Given case-sensitive matching
    And a contains rule with pattern "OPEN" that evaluates to safe
    And a monitoring file containing "Status: OPEN"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return true

  Scenario: Case-sensitive match fails with wrong case
    Given case-sensitive matching
    And a contains rule with pattern "OPEN" that evaluates to safe
    And a monitoring file containing "Status: open"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return false

  Scenario: Regex rule matches safe pattern
    Given case-insensitive matching
    And a regex rule with pattern "safe_or_ok" that evaluates to safe
    And a regex rule with pattern "danger_or_error" that evaluates to unsafe
    And a monitoring file containing "Status: SAFE"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return true

  Scenario: Regex rule matches safe pattern with whitespace
    Given case-insensitive matching
    And a regex rule with pattern "safe_or_ok" that evaluates to safe
    And a regex rule with pattern "danger_or_error" that evaluates to unsafe
    And a monitoring file containing "Status:   OK"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return true

  Scenario: Regex rule matches unsafe pattern
    Given case-insensitive matching
    And a regex rule with pattern "safe_or_ok" that evaluates to safe
    And a regex rule with pattern "danger_or_error" that evaluates to unsafe
    And a monitoring file containing "Status: DANGER"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return false

  Scenario: Regex rule matches unsafe pattern without whitespace
    Given case-insensitive matching
    And a regex rule with pattern "safe_or_ok" that evaluates to safe
    And a regex rule with pattern "danger_or_error" that evaluates to unsafe
    And a monitoring file containing "Status:ERROR"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return false

  Scenario: Regex rule no match defaults to unsafe
    Given case-insensitive matching
    And a regex rule with pattern "safe_or_ok" that evaluates to safe
    And a regex rule with pattern "danger_or_error" that evaluates to unsafe
    And a monitoring file containing "Status: UNKNOWN"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return false

  Scenario: First matching rule wins
    Given case-insensitive matching
    And a contains rule with pattern "SAFE" that evaluates to safe
    And a contains rule with pattern "SAFE" that evaluates to unsafe
    And a monitoring file containing "Status: SAFE"
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return true

  Scenario: Connected device with matching content reports safe via is_safe
    Given a monitoring file containing "Status: OPEN"
    And a contains rule with pattern "OPEN" that evaluates to safe
    And filemonitor is running with these rules
    When I connect the device
    Then is_safe should return true

  Scenario: Disconnected device returns not connected error from is_safe
    Given a monitoring file containing "OPEN"
    And a contains rule with pattern "OPEN" that evaluates to safe
    And filemonitor is running with these rules
    Then is_safe should fail with a not connected error

  Scenario: Connected device with no matching rules reports unsafe
    Given a monitoring file containing "UNKNOWN STATUS"
    And filemonitor is running
    When I connect the device
    Then is_safe should return false
