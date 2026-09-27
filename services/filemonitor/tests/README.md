# ConformU Integration Tests

This directory contains integration tests that use ConformU for ASCOM Alpaca compliance testing.

## Running ConformU Tests

The ConformU tests are integrated into the Rust test suite but require ConformU to be installed:

```bash
# Install ConformU first (if not already installed)
./test-conformance.sh --install-conformu

# Run the ConformU integration tests. The default test filter excludes the
# `conformu` tag, so select it with --config=conformu (needs ConformU
# installed; point CONFORMU_PATH at it).
CONFORMU_PATH=/path/to/conformu bazel test --config=conformu //...

# Or just the filemonitor ConformU test
CONFORMU_PATH=/path/to/conformu bazel test --config=conformu //services/filemonitor:conformu_integration
```

## Test Structure

- `conformu_integration.rs`: Main ConformU compliance test
- Drives the `conformu` binary through `bdd_infra::run_conformu` — ConformU's
  URL-argument verbs, so the run is always ConformU's full test set; the
  runner writes its own settings file (ConformU's defaults, since this test
  passes `None`) and no test selection is expressible from the test
- Creates temporary test environment with config and status files
- Starts filemonitor service and runs both the `alpacaprotocol` and
  `conformance` suites

## Requirements

- ConformU must be installed, and `CONFORMU_PATH` must name the binary; the
  test self-skips (and passes) when the variable is unset, which is what keeps
  it inert in the ordinary `cargo` / `bazel` suites
- Runs nightly in the `conformu.yml` rotation via `[package.metadata.conformu]`
- Built with the `conformu` cargo feature (gates the test file); no
  `ascom-alpaca` test feature is involved — the runner lives in `bdd-infra`
  (see [docs/skills/testing.md §1.4](../../../docs/skills/testing.md#14-conformu-integration-tests))
