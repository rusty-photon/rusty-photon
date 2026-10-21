// The coverage job's `bazel coverage` run (bazel-coverage.yml): the whole
// repository under the coverage config, interrupted before the job's own
// timeout can cancel it, a failure named by Bazel's exit code, and the
// combined report it leaves for the steps after it. Conventions:
// docs/skills/ci-logic.md.

import { spawn as nodeSpawn } from 'node:child_process';
import { stat } from 'node:fs/promises';
import path from 'node:path';

import { command, describe, withoutRunnerVariables } from '../lib/helpers.mjs';

/**
 * The command, after `bazel`. --config=coverage selects the nightly channel,
 * --cfg=coverage_nightly, --combined_report=lcov, the first-party
 * instrumentation filter and --test_timeout=900 (.bazelrc). It comes after
 * --config=ci, so that its test scheduling wins over test:ci's.
 * --test_tag_filters=-requires-cargo includes the BDD suites, and replaces
 * --config=coverage's `-requires-cargo,-conformu`, so the conformu-tagged
 * targets are selected too; they skip themselves, since this job sets no
 * CONFORMU_PATH.
 * --remote_download_outputs=all overrides build:ci's `toplevel`, so the
 * combined report and the per-test .dat files it is made from are written
 * locally. --test_output=all prints rp:bdd's BDD-TRACE breadcrumbs (ENTER,
 * reset-ok, EXIT, each tagged +Ns) on a passing run too, as bazel.yml's BDD
 * step does. --spawn_strategy=local and --remote_timeout=15 come from
 * .bazelrc's test:ci and build:ci.
 */
export const ARGS = [
  'coverage',
  '--config=ci',
  '--config=coverage',
  '--config=ci-remote',
  '--test_tag_filters=-requires-cargo',
  '--remote_download_outputs=all',
  '--test_output=all',
  '//...',
];

/**
 * How long the run may take: 50 minutes, ten under the job's 60. Past it
 * Bazel is sent SIGINT, which interrupts the run and writes the log of each
 * test still running, so the upload after this step has them. The job's own
 * timeout would cancel the step and write nothing.
 */
export const CAP_MS = 50 * 60_000;

/** How long an interrupted Bazel has to exit before it is sent SIGKILL. */
export const GRACE_MS = 2 * 60_000;

/**
 * Runs `bazel` with `args`, its output going to the step's log, and resolves
 * to how it ended: `{ code, signal, error, capped, stopped }`. Past `capMs`
 * Bazel is sent SIGINT (`capped`), and so it is at once when `signal` aborts
 * (`stopped`: the runner stopping the step). One still running `graceMs`
 * after that is sent SIGKILL. `error` is why Bazel could not start.
 */
export function runBazel(args, { env = process.env, spawn = nodeSpawn, signal, capMs = CAP_MS, graceMs = GRACE_MS } = {}) {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn('bazel', args, { env: withoutRunnerVariables(env), stdio: ['ignore', 'inherit', 'inherit'] });
    } catch (err) {
      resolve({ code: null, signal: null, error: err, capped: false, stopped: false });
      return;
    }
    let error;
    let capped = false;
    let stopped = false;
    let kill;
    const interrupt = () => {
      if (kill) return;
      child.kill('SIGINT');
      kill = setTimeout(() => child.kill('SIGKILL'), graceMs);
    };
    const cap = setTimeout(() => {
      capped = true;
      interrupt();
    }, capMs);
    const stop = () => {
      stopped = true;
      interrupt();
    };
    if (signal?.aborted) stop();
    signal?.addEventListener('abort', stop, { once: true });
    // Node still emits 'close' after a child that could not start.
    child.on('error', (err) => {
      error ??= err;
    });
    child.on('close', (code, killed) => {
      clearTimeout(cap);
      clearTimeout(kill);
      signal?.removeEventListener('abort', stop);
      resolve({ code, signal: killed, error, capped, stopped });
    });
  });
}

/**
 * Why the run fails the step, or undefined when it passed. Coverage from a
 * run in which a test failed, or that did not finish, is not a passing
 * suite's, so every ending but exit 0 fails the step, and the coverage is not
 * published: the steps after this one that make and upload it need it to
 * succeed.
 */
export function failure({ code, signal, error, capped, stopped }, { capMs = CAP_MS } = {}) {
  const ended = signal ? `stopped by ${signal}` : `exit ${code}`;
  if (error) return `bazel coverage did not start: ${describe(error)}`;
  if (capped) {
    return (
      `bazel coverage hit its ${capMs / 60_000}-minute cap and was interrupted (${ended}): a test, almost ` +
      "certainly //services/rp:bdd, did not finish in time. The test logs are in the 'bazel-testlogs' artifact."
    );
  }
  if (stopped) return `bazel coverage was stopped with the step (${ended})`;
  if (code === 0) return undefined;
  if (code === 3) {
    return (
      'A coverage test failed or hit its --test_timeout (bazel exit 3). Coverage from a failed run does not ' +
      "reflect a passing suite, so it is not published. The test logs are above and in the 'bazel-testlogs' artifact."
    );
  }
  if (signal) return `bazel coverage was ${ended}`;
  return `bazel coverage failed (exit ${code}: 1 = build error, 2 = bad flags, 4 = no tests, 8 = interrupted)`;
}

/**
 * The combined report a passing run leaves, `_coverage/_coverage_report.dat`
 * under Bazel's output path. A missing or empty one throws: the upload would
 * publish no coverage at all.
 */
export async function combinedReport({ run = command } = {}) {
  const outputPath = (await run('bazel', ['info', 'output_path'])).trim();
  const file = path.join(outputPath, '_coverage', '_coverage_report.dat');
  const size = await stat(file).then(
    (s) => s.size,
    (err) => {
      if (err.code === 'ENOENT') return 0;
      throw err;
    },
  );
  if (size === 0) throw new Error(`the combined coverage report is missing or empty at ${file}`);
  return file;
}
