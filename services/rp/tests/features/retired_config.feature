Feature: The retired orchestrator surface is refused at config load
  rp registers no orchestrators and keeps no session (mcp-sessionless
  D6): runs start at the orchestrator (session-runner's POST /runs).
  A config that still carries the old surface — a plugins[] entry with
  type "orchestrator", or a session.session_state_file key — is
  rejected at load, by PUT /api/config and by rp doctor alike, with a
  message naming the migration ("orchestrator registrations were
  removed; start runs at session-runner — see
  docs/plans/mcp-sessionless.md"), because accepting it silently would
  leave an operator believing rp will start their session at dusk
  (D11). rusty-photon-doctor reports the same on an installed config as
  rp.orchestrator-registration-removed.

  Scenario: A config registering an orchestrator plugin fails to load
    Given an rp config registering an orchestrator plugin at "http://127.0.0.1:11171/invoke"
    When rp attempts to start
    Then rp should fail to start

  Scenario: A config naming a session state file fails to load
    Given an rp config with session_state_file "/tmp/rp-session/session_state.json"
    When rp attempts to start
    Then rp should fail to start

  Scenario: An event plugin registration beside no orchestrator still loads
    Given an rp config registering an event plugin at "http://127.0.0.1:11140/webhook"
    When rp attempts to start
    Then rp should start successfully
