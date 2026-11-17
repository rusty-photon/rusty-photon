@serial @wip
Feature: Transition rules
  A transition rule names a monitor and a direction, and sends a
  notification when the monitor's state changes that way. A reading is
  compared with the monitor's last known reading, so polls that cannot
  read the monitor in between change nothing. A monitor that has failed
  five polls in a row is unreadable: an unreadable rule fires then, and
  again at the monitor's first reading afterwards.

  Scenario: A failed poll between safe and unsafe still sends the safe-to-unsafe notification
    Given a monitoring file containing "OPEN"
    And filemonitor is running with a contains rule "OPEN" as safe and 1 second polling
    And sentinel is configured to monitor the filemonitor with 1 second polling
    And a safe-to-unsafe transition rule for "Roof Monitor"
    And sentinel is running
    Then the dashboard status should show "Safe" for "Roof Monitor"
    When the filemonitor device is disconnected
    Then the dashboard status should show "Unknown" for "Roof Monitor"
    When the monitoring file changes to "CLOSED"
    And the filemonitor device is reconnected
    Then the notification history should hold the message "Roof Monitor changed to Unsafe"
    And the notification history should hold 1 message

  Scenario: A monitor that fails five polls in a row sends the unreadable notification
    Given sentinel is configured to monitor a device at an unreachable address
    And an unreadable transition rule for "Unreachable Monitor"
    And sentinel is running
    Then the notification history should hold the message "Unreachable Monitor is now Unknown"
    And the notification history should hold 1 message

  Scenario: An unreadable monitor's first reading afterwards sends the unreadable notification again
    Given a monitoring file containing "OPEN"
    And filemonitor is running with a contains rule "OPEN" as safe and 1 second polling
    And sentinel is configured to monitor the filemonitor with 1 second polling
    And an unreadable transition rule for "Roof Monitor"
    And sentinel is running
    Then the dashboard status should show "Safe" for "Roof Monitor"
    When the filemonitor device is disconnected
    Then the notification history should hold the message "Roof Monitor is now Unknown"
    When the filemonitor device is reconnected
    Then the notification history should hold the message "Roof Monitor is now Safe"
    And the notification history should hold 2 messages
