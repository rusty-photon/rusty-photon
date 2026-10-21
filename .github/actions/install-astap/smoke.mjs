// install-astap.yml's smoke test of the install-astap action on each runner:
// the action exported ASTAP_BINARY, the file it names can run, and run with no
// arguments it prints ASTAP's banner. Conventions: docs/skills/ci-logic.md.

import { spawn as nodeSpawn } from 'node:child_process';
import { constants } from 'node:fs';
import { access } from 'node:fs/promises';

import { io, withoutRunnerVariables } from '../lib/helpers.mjs';

/**
 * Checks the ASTAP CLI at `binary`, the ASTAP_BINARY the action exported:
 * that it is set, that the file is executable (on Windows, where Node checks
 * no execute bit, that it exists), and that what it prints when run with no
 * arguments, where ASTAP prints its banner and exits, names ASTAP. Its exit
 * status is not checked. It gets `env`, by default the step's without the
 * runner's variables, as the `run:` step this replaces gave it. Throws,
 * saying which check failed.
 */
export async function smoke(binary, { spawn = nodeSpawn, env = withoutRunnerVariables(process.env), signal } = {}) {
  if (!binary) throw new Error('ASTAP_BINARY is not set: the install-astap action sets it');
  try {
    await access(binary, constants.X_OK);
  } catch {
    throw new Error(`${binary} is not an executable file`);
  }
  io.stdout(`Running ${binary} (no arguments: ASTAP prints its banner and exits)\n`);
  const output = await printed(binary, { spawn, env, signal });
  io.stdout(output);
  if (!/astap/i.test(output)) throw new Error('ASTAP banner not detected in output');
}

// What `program` prints on stdout and stderr, in the order it came, whatever
// its exit status. Rejects when it cannot start, or when `signal` aborts.
function printed(program, { spawn, env, signal }) {
  return new Promise((resolve, reject) => {
    const child = spawn(program, [], { env, signal, stdio: ['ignore', 'pipe', 'pipe'] });
    let text = '';
    for (const stream of [child.stdout, child.stderr]) {
      stream.setEncoding('utf8');
      stream.on('data', (chunk) => {
        text += chunk;
      });
    }
    child.on('error', reject);
    child.on('close', () => resolve(text));
  });
}
