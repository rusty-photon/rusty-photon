Feature: Import spooling and health
  rp being down must never lose an Align. Delivery failures append the import
  request to a bounded on-disk FIFO spool — one JSON request per line,
  fsynced per append — replayed in order once rp is reachable again, and the
  spool survives bridge restarts and the in-process reload a setup-page save
  fires. At the bound the oldest entry is dropped with an error log and a
  counter increment. Requests rp actively rejected (tool errors) are never
  spooled: they would fail identically on replay. GET /health on the Alpaca
  listener reports rp reachability, the durable backlog length, and
  process-lifetime replay and drop totals, which a reload keeps.

  Scenario: An Align while rp is unreachable spools durably
    Given rp is unreachable
    And the bridge is running
    And a connected planetarium client
    When the client aligns at ra 20.9877 hours dec 44.5253 degrees
    Then the align call should succeed
    And the health endpoint should report:
      | field         | value |
      | rp_reachable  | false |
      | spooled       | 1     |
      | dropped_total | 0     |

  Scenario: Spooled imports replay in order when rp recovers
    Given rp is unreachable
    And the spool replay backoff is capped at "2s"
    And the bridge is running
    And a connected planetarium client
    When the client aligns at ra 1.0 hours dec 10.0 degrees
    And the client aligns at ra 2.0 hours dec 20.0 degrees
    And the stub rp comes up at the configured rp address
    Then the stub rp should receive 2 add_target calls
    And the imports should arrive in Align order: ra_hours 1.0 then ra_hours 2.0
    And the replayed imports should carry receipt times from before rp recovered
    And the health endpoint should report:
      | field          | value |
      | rp_reachable   | true  |
      | spooled        | 0     |
      | replayed_total | 2     |

  Scenario: The spool replays across bridge restarts
    Given rp is unreachable
    And the spool replay backoff is capped at "2s"
    And the bridge is running
    And a connected planetarium client
    When the client aligns at ra 3.0 hours dec 30.0 degrees
    And the bridge is stopped
    And the stub rp comes up at the configured rp address
    And the bridge is restarted with the same configuration
    Then the stub rp should receive 1 add_target call
    And the import should carry ra_hours 3.0 and dec_degrees 30.0

  Scenario: The spool replays across a reload
    Given rp is unreachable
    And the spool replay backoff is capped at "2s"
    And the bridge is running
    And the bound port is pinned in the configuration file so a reload keeps the address
    And a connected planetarium client
    When the client aligns at ra 3.0 hours dec 30.0 degrees
    And I submit the server setup page setting "spool.replay_backoff_max" to "3s"
    Then the page reports the configuration was saved and the service is reloading
    And after the reload the server setup page shows "spool.replay_backoff_max" as "3s"
    When the stub rp comes up at the configured rp address
    Then the stub rp should receive 1 add_target call
    And the import should carry ra_hours 3.0 and dec_degrees 30.0

  Scenario: The health totals span a reload
    Given rp is unreachable
    And the spool is bounded to 1 entries
    And the spool replay backoff is capped at "2s"
    And the bridge is running
    And the bound port is pinned in the configuration file so a reload keeps the address
    And a connected planetarium client
    When the client aligns at ra 1.0 hours dec 10.0 degrees
    And the client aligns at ra 2.0 hours dec 20.0 degrees
    Then the health endpoint should report:
      | field         | value |
      | spooled       | 1     |
      | dropped_total | 1     |
    When I submit the server setup page setting "spool.replay_backoff_max" to "3s"
    Then the page reports the configuration was saved and the service is reloading
    And after the reload the server setup page shows "spool.replay_backoff_max" as "3s"
    And the health endpoint should report:
      | field         | value |
      | spooled       | 1     |
      | dropped_total | 1     |

  Scenario: Spool overflow drops the oldest entry and counts the drop
    Given rp is unreachable
    And the spool is bounded to 2 entries
    And the spool replay backoff is capped at "2s"
    And the bridge is running
    And a connected planetarium client
    When the client aligns at ra 1.0 hours dec 10.0 degrees
    And the client aligns at ra 2.0 hours dec 20.0 degrees
    And the client aligns at ra 3.0 hours dec 30.0 degrees
    Then the health endpoint should report:
      | field         | value |
      | spooled       | 2     |
      | dropped_total | 1     |
    When the stub rp comes up at the configured rp address
    Then the stub rp should receive 2 add_target calls
    And the imports should arrive in Align order: ra_hours 2.0 then ra_hours 3.0

  Scenario: A corrupt spool line is skipped on replay
    Given a stub rp MCP server is running
    And the spool replay backoff is capped at "2s"
    And a spool file already containing:
      """
      {"ra_hours":4.0,"dec_degrees":40.0,"source":{"kind":"planetarium-bridge","client":"10.0.0.1:1","received_at":"2026-07-30T05:00:00Z"}}
      this line is not JSON
      {"ra_hours":5.0,"dec_degrees":50.0,"source":{"kind":"planetarium-bridge","client":"10.0.0.1:1","received_at":"2026-07-30T05:01:00Z"}}
      """
    When the bridge is running
    Then the stub rp should receive 2 add_target calls
    And the imports should arrive in Align order: ra_hours 4.0 then ra_hours 5.0

  Scenario: A tool error is not spooled
    Given a stub rp MCP server is running
    And the stub rp rejects add_target calls
    And the bridge is running
    And a connected planetarium client
    When the client aligns at ra 6.0 hours dec 60.0 degrees
    Then the align call should succeed
    And the stub rp should receive 1 add_target call
    And the health endpoint should report:
      | field   | value |
      | spooled | 0     |
