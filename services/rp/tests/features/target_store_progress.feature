Feature: Target progress derivation (P1)
  Progress is computed on demand from goals plus the frames on disk,
  never stored (rp.md § Target Store → Progress derivation).
  `get_target`, `list_targets`, `get_target_status`,
  `get_session_progress`, and `record_exposure` all report, per target,
  a list of `{filter, binning, exposure_duration, good, total,
  desired_count}` — one entry per `AcquisitionGoal`, in goal order —
  superseding the filter-only `{completed, goal}` counter shape, which
  could not distinguish two goals that share a filter (e.g. Ha at two
  different exposure lengths).

  `rp` walks the night directories under the target's slug using the
  configured `directory_pattern` and `file_naming_pattern`, so it finds
  exactly the layout `capture` writes. Frames are bucketed by the
  `(filter, binning, exposure_duration)` triple, which is precisely an
  `AcquisitionGoal`'s identity, and accumulate across every night — a
  target's plan spans however many nights it takes to reach its goals.

  `good` counts every frame that is not demonstrably rejected: a frame
  is rejected only when its sidecar's `grading` section carries a metric
  that violates the target's effective thresholds (its own `grading`,
  field-wise over `target_store.default_grading`). No sidecar, no
  grading section, or no value for the judged metric all count as good —
  otherwise `good` would sit at 0 forever on every rig with no grading
  plugin installed, and no plan could ever complete.

  These scenarios use the documented default patterns:
  `directory_pattern` = `{target}/{night_date}/{frame_type}` and
  `file_naming_pattern` =
  `{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}`.

  Scenario: A target with no captured frames reports zero progress against every goal
    Given rp is running with a target store and filter roster "Luminance, Red"
    And an MCP client connected to rp
    And the MCP client has added a target named "Fresh Frame" at ra_hours 5.0 dec_degrees 10.0
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
      | Red       | 1x1     | 5m       | 20            |
    When the MCP client calls "get_target" for slug "fresh-frame"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 0    | 0     | 40      |
      | Red       | 1x1     | 5m       | 0    | 0     | 20      |

  Scenario: Frames on disk count toward the goal whose sub-spec they match
    Given rp is running with a target store, filter roster "Luminance, Red", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
      | Red       | 1x1     | 5m       | 20            |
    And the data directory contains these frames:
      | path                                                                     | sidecar |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | absent  |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0002_5m_fpos_1_-10C_aaaaaaa2.fits | absent  |
      | m31/2026-07-30/Light/m31_Red_1x1_0001_5m_fpos_2_-10C_bbbbbbb1.fits       | absent  |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 2    | 2     | 40      |
      | Red       | 1x1     | 5m       | 1    | 1     | 20      |

  Scenario: Frames from earlier nights accumulate into the same goal
    Given rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar |
      | m31/2026-07-28/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | absent  |
      | m31/2026-07-29/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa2.fits | absent  |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa3.fits | absent  |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 3    | 3     | 40      |

  Scenario: A frame whose sub-spec matches no goal is counted against nothing
    Given rp is running with a target store, filter roster "Luminance, Red", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | absent  |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_2m_fpos_1_-10C_aaaaaaa2.fits | absent  |
      | m31/2026-07-30/Light/m31_Red_1x1_0001_5m_fpos_2_-10C_bbbbbbb1.fits       | absent  |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 1    | 1     | 40      |

  Scenario: Filenames that do not match the configured pattern are skipped
    Given rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | absent  |
      | m31/2026-07-30/Light/hand-copied-from-the-old-rig.fits                   | absent  |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 1    | 1     | 40      |

  Scenario: Calibration frames under the same slug do not count toward a Light goal
    Given rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | absent  |
      | m31/2026-07-30/Flat/m31_Luminance_1x1_0001_5m_fpos_1_-10C_ccccccc1.fits  | absent  |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 1    | 1     | 40      |

  Scenario: With no grading thresholds configured every captured frame is good
    Given rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar      |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | {"hfr": 9.9} |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0002_5m_fpos_1_-10C_aaaaaaa2.fits | {"hfr": 2.1} |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 2    | 2     | 40      |

  Scenario: A frame violating a configured threshold counts toward total but not good
    Given rp is configured with target-store default grading max_hfr_pixels 3.0
    And rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar      |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | {"hfr": 2.1} |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0002_5m_fpos_1_-10C_aaaaaaa2.fits | {"hfr": 9.9} |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 1    | 2     | 40      |

  Scenario: A frame is good when its sidecar carries no value for the judged metric
    Given rp is configured with target-store default grading max_hfr_pixels 3.0
    And rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar            |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | absent             |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0002_5m_fpos_1_-10C_aaaaaaa2.fits | no-grading         |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0003_5m_fpos_1_-10C_aaaaaaa3.fits | {"star_count": 90} |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 3    | 3     | 40      |

  Scenario: Any one violated metric rejects the frame
    Given rp is configured with target-store default grading max_hfr_pixels 3.0 and min_star_count 100
    And rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar                         |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | {"hfr": 2.1, "star_count": 900} |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0002_5m_fpos_1_-10C_aaaaaaa2.fits | {"hfr": 2.1, "star_count": 12}  |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 1    | 2     | 40      |

  Scenario: A per-target grading override replaces the configured default
    Given rp is configured with target-store default grading max_hfr_pixels 3.0
    And rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the MCP client has set its grading max_hfr_pixels to 10.0
    And the data directory contains these frames:
      | path                                                                     | sidecar      |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | {"hfr": 2.1} |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0002_5m_fpos_1_-10C_aaaaaaa2.fits | {"hfr": 9.9} |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 2    | 2     | 40      |

  # A goal's sub-spec is its identity, and the store rejects a goal set
  # that repeats one ("duplicate goal key"), so a bucket always belongs
  # to exactly one goal. Over-shooting is still possible, and reports
  # the true count rather than clamping at desired_count.
  Scenario: An over-shot goal reports every frame captured against it
    Given rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 2             |
    And the data directory contains these frames:
      | path                                                                     | sidecar |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | absent  |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0002_5m_fpos_1_-10C_aaaaaaa2.fits | absent  |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0003_5m_fpos_1_-10C_aaaaaaa3.fits | absent  |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 3    | 3     | 2       |

  # `capture` renders the literal "NA" for the filter token on a train
  # with no filter wheel, so the scan maps an "NA" frame back to the
  # unfiltered goal slot — otherwise an OSC rig's plan could never
  # progress (rp.md § Progress derivation). No roster is configured
  # here: an unfiltered goal is what a wheel-less rig plans.
  Scenario: An "NA" frame counts toward the unfiltered goal
    Given rp is running with a target store and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter | binning | exposure_duration | desired_count |
      |        | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                              | sidecar |
      | m31/2026-07-30/Light/m31_NA_1x1_0001_5m_fpos_0_-10C_aaaaaaa1.fits | absent  |
    When the MCP client calls "get_target" for slug "m31"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter | binning | exposure_duration | good | total | desired_count |
      |        | 1x1     | 5m       | 1    | 1     | 40      |

  Scenario: get_session_progress reports every target's per-goal progress
    Given rp is running with a target store and filter roster "Luminance, Red"
    And an MCP client connected to rp
    And the MCP client has added a target named "First Frame" at ra_hours 5.0 dec_degrees 10.0
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the MCP client has added a target named "Second Frame" at ra_hours 6.0 dec_degrees 12.0
    And the MCP client has set its goals to:
      | filter | binning | exposure_duration | desired_count |
      | Red    | 1x1     | 5m       | 20            |
    When the MCP client calls "get_session_progress"
    Then the tool call should succeed
    And the progress for target "first-frame" should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 0    | 0     | 40      |
    And the progress for target "second-frame" should be exactly:
      | filter | binning | exposure_duration | good | total | desired_count |
      | Red    | 1x1     | 5m       | 0    | 0     | 20      |

  Scenario: record_exposure reports derived progress and increments nothing
    Given rp is running with a target store, filter roster "Luminance", and frame naming configured
    And an MCP client connected to rp
    And the MCP client has added a target named "M31" at ra_hours 0.712 dec_degrees 41.269
    And the MCP client has set its goals to:
      | filter    | binning | exposure_duration | desired_count |
      | Luminance | 1x1     | 5m       | 40            |
    And the data directory contains these frames:
      | path                                                                     | sidecar |
      | m31/2026-07-30/Light/m31_Luminance_1x1_0001_5m_fpos_1_-10C_aaaaaaa1.fits | absent  |
    When the MCP client calls "record_exposure" for target "m31" filter "Luminance"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 1    | 1     | 40      |
    When the MCP client calls "record_exposure" for target "m31" filter "Luminance"
    Then the tool call should succeed
    And the reported progress should be exactly:
      | filter    | binning | exposure_duration | good | total | desired_count |
      | Luminance | 1x1     | 5m       | 1    | 1     | 40      |

  Scenario: record_exposure against an unknown target is an error
    Given rp is running with a target store and filter roster "Luminance"
    And an MCP client connected to rp
    When the MCP client calls "record_exposure" for target "not-a-target" filter "Luminance"
    Then the tool call should return an error
    And the error message should contain "not-a-target"
