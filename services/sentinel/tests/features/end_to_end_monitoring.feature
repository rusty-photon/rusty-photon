@serial
Feature: End-to-end safety monitoring
  Sentinel polls a real ASCOM Alpaca SafetyMonitor (filemonitor),
  detects state changes, and records transitions in the dashboard API.

  Scenario: Sentinel detects safe state from filemonitor
    Given a monitoring file containing "OPEN"
    And filemonitor is running with a contains rule "OPEN" as safe
    And sentinel is configured to monitor the filemonitor
    And sentinel is running
    When I wait for sentinel to poll
    Then the dashboard status should show "Safe" for "Roof Monitor"

  Scenario: Sentinel detects unsafe state from filemonitor
    Given a monitoring file containing "CLOSED"
    And filemonitor is running with a contains rule "OPEN" as safe
    And sentinel is configured to monitor the filemonitor
    And sentinel is running
    When I wait for sentinel to poll
    Then the dashboard status should show "Unsafe" for "Roof Monitor"

  Scenario: Sentinel detects state transition after file change
    Given a monitoring file containing "OPEN"
    And filemonitor is running with a contains rule "OPEN" as safe and 1 second polling
    And sentinel is configured to monitor the filemonitor with 1 second polling
    And sentinel is running
    When I wait for sentinel to poll
    Then the dashboard status should show "Safe" for "Roof Monitor"
    When the monitoring file changes to "CLOSED"
    Then the dashboard status should show "Unsafe" for "Roof Monitor"

  Scenario: State transition is recorded in notification history
    Given a monitoring file containing "OPEN"
    And filemonitor is running with a contains rule "OPEN" as safe and 1 second polling
    And sentinel is configured to monitor the filemonitor with 1 second polling
    And a safe-to-unsafe transition rule for "Roof Monitor"
    And sentinel is running
    When I wait for sentinel to poll
    And the monitoring file changes to "CLOSED"
    Then the dashboard history should contain a record for "Roof Monitor"
    And the history record message should contain "Unsafe"

  Scenario: A Pushover API that never answers costs a failed notification, not the monitoring
    Given a monitoring file containing "OPEN"
    And filemonitor is running with a contains rule "OPEN" as safe and 1 second polling
    And sentinel is configured to monitor the filemonitor with 1 second polling
    And a safe-to-unsafe transition rule for "Roof Monitor"
    And the Pushover API never answers
    And sentinel is running
    Then the dashboard status should show "Safe" for "Roof Monitor"
    When the monitoring file changes to "CLOSED"
    Then the notification history should hold a failed notification for "Roof Monitor"
    When the monitoring file changes to "OPEN"
    Then the dashboard status should show "Safe" for "Roof Monitor"

  Scenario: Sentinel shows unknown when device is unreachable
    Given sentinel is configured to monitor a device at an unreachable address
    And sentinel is running
    When I wait for sentinel to poll
    Then the dashboard status should show "Unknown" for "Unreachable Monitor"

  Scenario: Sentinel reads a monitor again after its service restarts
    Given a monitoring file containing "OPEN"
    And filemonitor is running with a contains rule "OPEN" as safe and 1 second polling
    And sentinel is configured to monitor the filemonitor with 1 second polling
    And sentinel is running
    Then the dashboard status should show "Safe" for "Roof Monitor"
    When filemonitor stops
    Then the dashboard status should show "Unknown" for "Roof Monitor"
    When filemonitor starts again
    Then the dashboard status should show "Safe" for "Roof Monitor"

  Scenario: Sentinel connects a monitor again after another client disconnects it
    Given a monitoring file containing "OPEN"
    And filemonitor is running with a contains rule "OPEN" as safe and 1 second polling
    And sentinel is configured to monitor the filemonitor with 1 second polling
    And sentinel is running
    Then the dashboard status should show "Safe" for "Roof Monitor"
    When the filemonitor device is disconnected
    Then the filemonitor device should report connected
    And the dashboard status should show "Safe" for "Roof Monitor"

  Scenario: Sentinel shows unknown when the device never answers
    Given sentinel is configured to monitor a device that never answers
    And sentinel is running
    When I wait for sentinel to poll
    Then the dashboard status should show "Unknown" for "Silent Monitor"
