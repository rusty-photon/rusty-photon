// A developer's install of a tool CI installs: the same release, from the
// same URL, checked against the same pin, by the module the action runs.
// `bazel run //.github/actions:install -- <tool>` starts it (index.mjs), on the
// Node MODULE.bazel pins (ADR-022).
//
// It downloads on the developer's machine, from where CI downloads, and ships
// nothing: none of these tools is bundled into a package or mirrored for
// developers. A tool whose CI download comes from the project's private
// mirror (ADR-018) gets no entry in TOOLS.

import os from 'node:os';
import path from 'node:path';
import { parseArgs } from 'node:util';

import * as omnisim from '../install-omnisim/omnisim.mjs';
import * as pebble from '../install-pebble/pebble.mjs';
import { describe, io } from '../lib/helpers.mjs';
import { hostKey } from '../lib/install.mjs';

/**
 * The tools it installs, each by the install-* action's own module: `resolve`
 * chooses the assets for a runner, `installAt` installs them into a directory
 * and resolves to the variables that locate them.
 */
export const TOOLS = {
  omnisim: {
    about: 'ASCOM OmniSim, the Alpaca simulators the BDD suites spawn',
    resolve: omnisim.resolve,
    installAt: omnisim.installAt,
  },
  pebble: {
    about: "Pebble and pebble-challtestsrv, for doctor's @pebble BDD scenarios",
    resolve: pebble.resolve,
    installAt: pebble.installAt,
  },
};

/** How to run it, naming each of `tools`. */
export function usage(tools = TOOLS) {
  return [
    'usage: bazel run //.github/actions:install -- <tool> [--prefix DIR]',
    '',
    'Installs a tool CI installs: the same release, checked against the same SHA-256.',
    '',
    ...Object.entries(tools).map(([name, { about }]) => `  ${name.padEnd(9)} ${about}`),
    '',
    'DIR defaults to ~/.local/opt/<tool>; a relative one is taken from where bazel ran.',
    'It prints the variables that point the tests at what it installed.',
  ].join('\n');
}

/** A command line it cannot run. */
export class UsageError extends Error {
  constructor(message) {
    super(message);
    this.name = 'UsageError';
  }
}

/**
 * What the command line `args` asks for: `{ help: true }`, or the tool and the
 * directory to install it in. `cwd` is where a relative `--prefix` starts.
 */
export function parse(args, { cwd, home, tools = TOOLS }) {
  let parsed;
  try {
    parsed = parseArgs({
      args,
      options: { prefix: { type: 'string' }, help: { type: 'boolean', short: 'h' } },
      allowPositionals: true,
    });
  } catch (err) {
    throw new UsageError(err.message);
  }
  const { values, positionals } = parsed;
  if (values.help) return { help: true };
  if (positionals.length !== 1) {
    throw new UsageError(positionals.length === 0 ? 'name the tool to install' : `one tool at a time; got ${positionals.join(', ')}`);
  }
  const [tool] = positionals;
  if (!Object.hasOwn(tools, tool)) {
    throw new UsageError(`no tool named ${tool}; there are ${Object.keys(tools).join(', ')}`);
  }
  if (values.prefix === '') throw new UsageError('--prefix needs a directory');
  const dir = values.prefix === undefined ? path.join(home, '.local', 'opt', tool) : path.resolve(cwd, values.prefix);
  return { tool, dir };
}

/** The lines that set `variables` in the developer's shell: POSIX sh, or cmd's setx on Windows. */
export function settings(variables, platform = process.platform) {
  return Object.entries(variables).map(([name, value]) =>
    platform === 'win32' ? `setx ${name} "${value}"` : `export ${name}='${value.replaceAll("'", "'\\''")}'`,
  );
}

/**
 * Runs the command line `args` and resolves to its exit code: 0, 1 when the
 * install fails, 2 for a command line it cannot run. Bazel runs it in the
 * runfiles tree and says in BUILD_WORKING_DIRECTORY where it was started.
 */
export async function main(
  args,
  {
    cwd = process.env.BUILD_WORKING_DIRECTORY || process.cwd(),
    home = os.homedir(),
    platform = process.platform,
    runner,
    tools = TOOLS,
    download,
  } = {},
) {
  let request;
  try {
    request = parse(args, { cwd, home, tools });
  } catch (err) {
    if (!(err instanceof UsageError)) throw err;
    io.stderr(`${err.message}\n\n${usage(tools)}\n`);
    return 2;
  }
  if (request.help) {
    io.stdout(`${usage(tools)}\n`);
    return 0;
  }
  try {
    const tool = tools[request.tool];
    const variables = await tool.installAt(request.dir, tool.resolve({ runner: runner ?? hostKey() }), { download });
    const lines = settings(variables, platform).map((line) => `  ${line}`);
    io.stdout(`\n${request.tool} installed in ${request.dir}. Set these in your shell profile:\n\n${lines.join('\n')}\n`);
    return 0;
  } catch (err) {
    io.stderr(`${request.tool} was not installed: ${describe(err)}\n`);
    return 1;
  }
}
