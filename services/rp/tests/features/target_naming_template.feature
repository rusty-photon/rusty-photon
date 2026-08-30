Feature: Round-trippable file-naming template config-load validation (P1)
  `session.file_naming_pattern` and `session.directory_pattern` are a
  round-trippable contract rp both renders and parses back (rp.md §
  Persistence, rp-targets.md § File-naming template). Both are parsed
  and checked at startup: a bad pattern fails the load, not a session.
  Rejection rules: the pattern
  must carry every token needed to derive the quota key (`{target}`,
  `{filter}`, `{binning}`, `{exposure_duration}`); it must not name
  `{uuid8}` — rp appends every frame's UUID-8 after the rendered pattern
  itself, so per-frame uniqueness never depends on the pattern and
  `{frame_number}` is optional; it must compile to an
  unambiguous anchored regex — two variable-width tokens can't sit
  adjacent with no literal separator excluded from both charsets; and
  unknown tokens are rejected. `session.directory_pattern` shares the
  same unambiguous-regex check but not the quota-token
  requirement (its default, `"{target}/{night_date}/{frame_type}"`, has
  none). *(This feature covers config-load validation only —
  `capture`'s actual use of these patterns is covered by
  `capture_target_linkage.feature`.)*

  Scenario: A directory_pattern with an unknown token is rejected at config load
    Given an rp config with directory_pattern "{target}/{night_date}/{bogus_token}"
    When rp attempts to start
    Then rp should fail to start

  Scenario: The documented default directory_pattern starts successfully
    Given an rp config with directory_pattern "{target}/{night_date}/{frame_type}"
    When rp attempts to start
    Then rp should start successfully

  Scenario: A pattern missing a required quota token is rejected at config load
    Given an rp config with file_naming_pattern "{target}_{frame_number}"
    When rp attempts to start
    Then rp should fail to start

  # {frame_number} and {exposure_duration} are both purely-numeric charsets with
  # no literal separator between them here — the doc's own canonical
  # example of an unresolvable split.
  Scenario: A pattern placing two ambiguous variable-width tokens adjacent is rejected at config load
    Given an rp config with file_naming_pattern "{target}_{filter}_{binning}_{frame_number}{exposure_duration}_fpos_{filter_position}_{sensor_temp}"
    When rp attempts to start
    Then rp should fail to start

  Scenario: An unknown token is rejected at config load
    Given an rp config with file_naming_pattern "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_{bogus_token}"
    When rp attempts to start
    Then rp should fail to start

  Scenario: The default file_naming_pattern starts successfully
    Given an rp config with file_naming_pattern "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}"
    When rp attempts to start
    Then rp should start successfully

  Scenario: The old {duration}/{sequence} token names are rejected as unknown tokens
    Given an rp config with file_naming_pattern "{target}_{filter}_{binning}_{sequence}_{duration}_fpos_{filter_position}_{sensor_temp}"
    When rp attempts to start
    Then rp should fail to start

  Scenario: A pattern naming the uuid8 token is rejected at config load, since rp appends the suffix itself
    Given an rp config with file_naming_pattern "{target}_{filter}_{binning}_{frame_number}_{exposure_duration}_fpos_{filter_position}_{sensor_temp}_{uuid8}"
    When rp attempts to start
    Then rp should fail to start

  Scenario: A pattern carrying only the quota tokens starts successfully, since uniqueness comes from the uuid8 suffix
    Given an rp config with file_naming_pattern "{target}_{filter}_{binning}_{exposure_duration}"
    When rp attempts to start
    Then rp should start successfully
