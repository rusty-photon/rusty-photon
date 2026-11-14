Feature: Doctor subcommand smoke
  The service binary's own doctor subcommand diagnoses its config file
  read-only through the same typed load path a start would use
  (docs/services/doctor.md).

  Scenario: A valid config file yields a clean report
    Given this service's valid config file staged for doctor
    When the doctor subcommand runs
    Then the doctor report is clean

  Scenario: An unknown config key fails the report and is named
    Given this service's valid config file with an unknown key added
    When the doctor subcommand runs
    Then the doctor report fails naming the unknown key
