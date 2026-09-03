@serial
Feature: What survives an rp restart
  rp keeps no run state. A restart (crash, power failure, systemd
  restart) restores configuration and reconnects equipment, and nothing
  else: runs belong to the orchestrator that started them, which
  resumes its own (session-runner's recovery.feature covers that end to
  end), and rp re-invokes nobody. What survives on rp's side is derived,
  not persisted — progress is read from the frames on disk on every
  get_session_progress call, so a restarted rp reports the true count
  rather than zero (rp.md § What Survives an rp Restart).

  Background:
    Given a running Alpaca simulator

  # The data_directory is pinned so both the redb target store (which
  # owns the target and its goals) and the captured frames survive the
  # restart. Progress needs no recovery machinery at all: it is derived
  # from those frames on every read (rp.md § Progress derivation), so a
  # restarted rp resumes at the true count rather than at zero. The
  # target is addressed by slug ("Test Field" -> test-field).
  Scenario: Derived progress survives an rp restart
    Given rp is configured with site latitude 51.0786 longitude -0.2944
    And rp is configured with frame naming
    And rp's data_directory is pinned to a fresh tempdir
    And rp is running with a mount on the simulator
    And an MCP client connected to rp
    And the MCP client has added the always-visible target "Test Field" with goals:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 2m                | 4             |
    And the data directory contains these frames:
      | path                                                                             | sidecar |
      | test-field/2026-07-30/Light/test-field_Red_1x1_0001_2m_fpos_1_-10C_aaaaaaa1.fits | absent  |
      | test-field/2026-07-30/Light/test-field_Red_1x1_0002_2m_fpos_1_-10C_aaaaaaa2.fits | absent  |
    When rp is killed
    And rp is restarted after the crash
    And an MCP client connected to rp
    And the MCP client calls "get_session_progress"
    Then the tool call should succeed
    And the progress for target "test-field" should be exactly:
      | filter | binning | exposure_duration | good | total | desired_count |
      | Red    | 1x1     | 2m                | 2    | 2     | 4             |
