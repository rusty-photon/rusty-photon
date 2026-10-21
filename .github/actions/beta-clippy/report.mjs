// check.yml's nightly beta clippy, before its tracking issues (track.mjs):
// clippy on the beta toolchain in the stable gate's two passes, with every
// lint capped at a warning, so the job reports a lint new on beta rather than
// failing on it. It prints what clippy reported and counts it per lint with
// tools/ci/beta_clippy_census.py, which developers run too. Conventions:
// docs/skills/ci-logic.md.

import { spawn as nodeSpawn } from 'node:child_process';
import { appendFileSync, closeSync, mkdirSync, openSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';

import { execute, fail, io, withoutRunnerVariables } from '../lib/helpers.mjs';
import { ended, exitText } from '../lib/processes.mjs';

// The stable gate's two passes (check.yml's `stable / clippy`), so beta
// announces a lint on the default-features surface too. The cap goes on
// clippy's own arguments, not RUSTFLAGS, so it reaches the workspace's
// packages alone and the dependencies' artifacts stay cached.
export const PASSES = [
  ['clippy', '--all-targets', '--all-features', '--message-format=json', '--', '--cap-lints', 'warn'],
  ['clippy', '--workspace', '--lib', '--bins', '--message-format=json', '--', '--cap-lints', 'warn'],
];

/** The census script, in the checkout at `root`. */
export const CENSUS = 'tools/ci/beta_clippy_census.py';

// A variable the runner sets for every step.
function runnerVariable(env, name) {
  if (!env[name]) throw new Error(`${name} is not set; the runner sets it for every step`);
  return env[name];
}

// Runs `program` with `args` as a `run:` step would have, never through a
// shell, its stdin the file `stdin` (none without one) and its stdout appended
// to the file `stdout`. Its stderr goes to the log as it comes. Resolves to
// how it ended (ended).
async function runWith(program, args, { env, spawn, signal, stdin, stdout }) {
  const input = stdin === undefined ? 'ignore' : openSync(stdin, 'r');
  const output = openSync(stdout, 'a');
  try {
    const child = spawn(program, args, { env, signal, stdio: [input, output, 'pipe'] });
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (text) => io.stderr(text));
    return await ended(child);
  } finally {
    if (input !== 'ignore') closeSync(input);
    closeSync(output);
  }
}

/**
 * The diagnostics `text`, cargo's JSON lines, as clippy renders them, which
 * `--message-format=json` moves out of the log and into the JSON. A line
 * that is not JSON is left out.
 */
export function rendered(text) {
  const out = [];
  for (const line of text.split('\n')) {
    if (!line.startsWith('{')) continue;
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      continue;
    }
    const shown = message.reason === 'compiler-message' ? message.message?.rendered : undefined;
    if (typeof shown === 'string' && shown !== '') out.push(shown.endsWith('\n') ? shown : `${shown}\n`);
  }
  return out.join('');
}

/**
 * The action: beta clippy's two passes in the checkout, in `env` without the
 * runner's variables, then what they reported, printed, and the census of it,
 * appended to the run's summary as a table. A pass that fails, as on a
 * compile error, fails the step and makes no census; `signal` stops the run.
 * Resolves to the census's file, in RUNNER_TEMP, or undefined when there is
 * none.
 */
export async function report({ env = process.env, spawn = nodeSpawn, signal } = {}) {
  const root = runnerVariable(env, 'GITHUB_WORKSPACE');
  const summary = runnerVariable(env, 'GITHUB_STEP_SUMMARY');
  const dir = path.join(runnerVariable(env, 'RUNNER_TEMP'), 'beta-clippy');
  mkdirSync(dir, { recursive: true });
  const runEnv = withoutRunnerVariables(env);

  const toolchain = (await execute('cargo', ['clippy', '--version'], { env: runEnv, spawn, signal })).trim();
  const diagnostics = path.join(dir, 'clippy.json');
  writeFileSync(diagnostics, '');
  const failed = [];
  for (const args of PASSES) {
    const result = await runWith('cargo', args, { env: runEnv, spawn, signal, stdout: diagnostics });
    if (signal?.aborted) {
      fail('beta clippy was stopped before it finished');
      return undefined;
    }
    if (result.code !== 0) failed.push(`cargo ${args.join(' ')} ${exitText(result)}`);
  }
  io.stdout(rendered(readFileSync(diagnostics, 'utf8')));
  if (failed.length > 0) {
    for (const message of failed) fail(message);
    return undefined;
  }

  const census = path.join(dir, 'census.json');
  const table = path.join(dir, 'census.md');
  writeFileSync(census, '');
  const result = await runWith('python3', [path.join(root, CENSUS), '--root', root, '--toolchain', toolchain, '--summary', table], {
    env: runEnv,
    spawn,
    signal,
    stdin: diagnostics,
    stdout: census,
  });
  if (result.code !== 0) throw new Error(`${CENSUS} ${exitText(result)}`);
  appendFileSync(summary, readFileSync(table, 'utf8'));
  return census;
}
