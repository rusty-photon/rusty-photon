@serial
Feature: Operation watchdog
  Sentinel subscribes to rp's operation event stream and tracks each
  operation's deadline independently of rp. An operation that completes
  within its deadline passes silently; one that overruns its deadline — or a
  stream that goes away because rp itself is unreachable — raises an
  escalation through the same notifier chain and notification history the
  safety monitor uses. Each escalation is recorded under the "Operation
  Watchdog" monitor name.

  Scenario: An operation that completes within its deadline raises no alert
    Given rp is streaming a slew operation that completes within its deadline
    And sentinel is running
    Then the watchdog records no escalation

  Scenario: A guide start that settles within its deadline raises no alert
    Given rp is streaming a guide start that settles within its deadline
    And sentinel is running
    Then the watchdog records no escalation

  Scenario: An operation that overruns its deadline is escalated
    Given rp is streaming a slew operation that never completes
    And sentinel is running
    Then the watchdog records an escalation mentioning "exceeded its deadline"

  Scenario: An unreachable rp is escalated as unresponsive
    Given rp's event stream is unreachable
    And sentinel is running
    Then the watchdog records an escalation mentioning "unresponsive"

  Scenario: An overrunning abort_then_restart operation aborts its service
    Given the mount service is reachable and records aborts
    And rp is streaming a slew operation that never completes
    And sentinel is running
    Then the watchdog aborts the mount service
    And the watchdog records an escalation mentioning "abort=ok"
