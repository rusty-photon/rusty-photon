// The ConformU suites of the services conformu.yml's plan job found, all run
// at once, for its Linux and macOS job. Each service's command is the one its
// `[package.metadata.conformu]` gives, which cargo-workspace/packages read:
// a `cargo test` of its conformu_integration test. Each suite's output goes to
// a log of its own, printed once they all end, a cancelled or timed-out run
// included. Conventions: docs/skills/ci-logic.md.

import { mkdirSync } from 'node:fs';
import path from 'node:path';

import { fail, io, withoutRunnerVariables } from '../lib/helpers.mjs';
import { replay, runStreams } from '../lib/processes.mjs';

// A service's name names its log file too.
const NAME = /^[A-Za-z0-9_-]+$/;

// A word of a command that no shell would read otherwise: the command runs
// without one, split at its spaces, so a quote, a variable, a glob or an
// operator would not do what the command says.
const WORD = /^[A-Za-z0-9_.,:=+@%/-]+$/;

/** `command`, the command `name`'s table gives, as its program and arguments. */
export function words(name, command) {
  const list = command.split(/[ \t]+/).filter((word) => word !== '');
  if (list.length === 0) throw new Error(`the ConformU command of ${name} is empty`);
  const odd = list.find((word) => !WORD.test(word));
  if (odd !== undefined) {
    throw new Error(
      `the ConformU command of ${name}, ${JSON.stringify(command)}, has ${JSON.stringify(odd)} in it, which a shell would read: ` +
        'conformu-run runs a command without one, as words split at its spaces',
    );
  }
  const [program, ...args] = list;
  return { program, args };
}

/**
 * The services input `text`: a JSON list of `{ name, command }`, as
 * cargo-workspace/packages gives them, as `{ name, program, args }`. Throws for
 * anything else, for an empty list, which would run nothing and pass, and for
 * a name given twice.
 */
export function services(text) {
  let list;
  try {
    list = JSON.parse(text);
  } catch (err) {
    throw new Error(`the services input is not JSON: ${err.message}`);
  }
  if (!Array.isArray(list)) throw new Error('the services input is not a JSON list');
  if (list.length === 0) throw new Error('the services input lists no service');
  const seen = new Set();
  return list.map((service, i) => {
    const { name, command } = service ?? {};
    if (typeof name !== 'string' || !NAME.test(name)) {
      throw new Error(`service ${i + 1} of the services input has no name of letters, digits, _ and -: ${JSON.stringify(service)}`);
    }
    if (typeof command !== 'string') throw new Error(`the service ${name} has no command`);
    if (seen.has(name)) throw new Error(`the services input lists ${name} twice`);
    seen.add(name);
    return { name, ...words(name, command) };
  });
}

/**
 * The action: runs the suite of every service the services input `text`
 * lists at once, in `env` without the runner's variables, as the `run:` step
 * it replaces ran them, and prints each one's log as a group of its own
 * however the run ends. Fails the step for each suite that failed, by name,
 * or once when `signal` stopped them. Its logs go in `tmp`.
 */
export async function main({ services: text, env = process.env, tmp, spawn, signal, graceMs }) {
  const list = services(text);
  const logs = path.join(tmp, 'conformu-logs');
  mkdirSync(logs, { recursive: true });
  const runEnv = withoutRunnerVariables(env);
  const planned = list.map(({ name, program, args }) => ({ name, runs: [{ program, args, env: runEnv }] }));
  io.stdout(`running ${list.length} ConformU suites at once: ${list.map(({ name }) => name).join(', ')}\n`);

  let result;
  try {
    result = await runStreams(planned, { logs, spawn, signal, graceMs });
  } finally {
    await replay(planned, logs, (name) => `ConformU ${name}`);
  }
  if (result.stopped) {
    fail('the ConformU suites were stopped before they finished');
    return;
  }
  for (const { name } of list) {
    if (result.failed.includes(name)) fail(`ConformU ${name} failed`);
    else io.stdout(`ConformU ${name} passed\n`);
  }
}
