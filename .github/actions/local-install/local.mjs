// A developer's install of a tool CI installs: the same release, from the
// same URL, checked against the same pin, by the module the action runs.
// `bazel run //.github/actions:install -- <tool>` starts it (index.mjs), on the
// Node MODULE.bazel pins (ADR-022).
//
// It downloads on the developer's machine, from where CI downloads, and ships
// nothing: none of these tools is bundled into a package or mirrored for
// developers. Nor does it download from the project's private mirror
// (ADR-018 §7): the SVBony SDK installs on Linux and macOS, and on Windows,
// whose SDK only the mirror holds, the install says where to get it.

import os from 'node:os';
import path from 'node:path';
import { parseArgs } from 'node:util';

import * as astap from '../install-astap/astap.mjs';
import * as conformu from '../install-conformu/conformu.mjs';
import * as omnisim from '../install-omnisim/omnisim.mjs';
import * as pebble from '../install-pebble/pebble.mjs';
import * as svbonySdk from '../install-svbony-sdk/svbony-sdk.mjs';
import * as zwoSdk from '../install-zwo-sdk/zwo-sdk.mjs';
import { describe, io } from '../lib/helpers.mjs';
import { hostKey } from '../lib/install.mjs';

/**
 * The tools it installs, each by the install-* action's own module: `resolve`
 * chooses the assets for a runner (ConformU's asks GitHub's API, so it may
 * answer with a promise), `installAt` installs them into a directory and
 * resolves to the variables that locate them. A tool with a `dir` names
 * the directory it installs into by default, where ~/.local/opt/<tool> would
 * not be found.
 */
export const TOOLS = {
  astap: {
    about: "ASTAP's CLI and D05 star database, for plate-solver's @requires-astap scenarios",
    resolve: astap.resolve,
    installAt: astap.installAt,
  },
  conformu: {
    about: "ASCOM ConformU's latest release, which the conformu_integration tests drive; on Windows by its installer",
    resolve: conformu.resolve,
    installAt: conformu.installAt,
    // Where its Windows installer puts it, which nothing here can move.
    dir: ({ platform }) => (platform === 'win32' ? conformu.windowsDir() : undefined),
  },
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
  'svbony-sdk': {
    about: `SVBony's SDK, which svbony-camera links; into ${svbonySdk.PREFIX}, on Linux and Intel Macs only`,
    resolve: svbonySdk.resolve,
    installAt: svbonySdk.installAt,
    // Where libsvbony-sys's build script looks, as CI installs it.
    dir: ({ platform }) => (platform === 'win32' ? undefined : svbonySdk.PREFIX),
  },
  'zwo-sdk': {
    about: `ZWO's SDKs, which zwo-camera and zwo-focuser link; into ${zwoSdk.PREFIX}, but on Windows ~/.local/opt/zwo-sdk`,
    resolve: zwoSdk.resolve,
    installAt: zwoSdk.installAt,
    // Where libzwo-sys's build script looks, as CI installs it.
    dir: ({ platform }) => (platform === 'win32' ? undefined : zwoSdk.PREFIX),
  },
};

/** How to run it, naming each of `tools`. */
export function usage(tools = TOOLS) {
  const width = Math.max(...Object.keys(tools).map((name) => name.length));
  return [
    'usage: bazel run //.github/actions:install -- <tool> [--prefix DIR]',
    '',
    'Installs a tool CI installs: the same release, checked against the same SHA-256.',
    '',
    ...Object.entries(tools).map(([name, { about }]) => `  ${name.padEnd(width)} ${about}`),
    '',
    'DIR defaults to ~/.local/opt/<tool>, unless the tool says otherwise; a relative one is taken',
    'from where bazel ran.',
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
export function parse(args, { cwd, home, platform = process.platform, tools = TOOLS }) {
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
  if (values.prefix === undefined) return { tool, dir: tools[tool].dir?.({ platform }) ?? path.join(home, '.local', 'opt', tool) };
  // A shell expands a leading ~ only in a word of its own, not in --prefix=~/x.
  const prefix = values.prefix.replace(/^~(?=$|[\\/])/, () => home);
  return { tool, dir: path.resolve(cwd, prefix) };
}

// The variables that hold a list of directories, where a profile line puts
// the new one first and keeps what the shell had.
const SEARCH_PATHS = new Set(['LD_LIBRARY_PATH']);

/**
 * What to do with `variables`, and the lines that do it: on Windows, setx
 * lines to run once, which set them for every shell opened after; elsewhere,
 * POSIX sh lines for the shell's profile.
 */
export function settings(variables, platform = process.platform) {
  const entries = Object.entries(variables);
  if (platform === 'win32') {
    return {
      how: 'Run these once, then open a new shell, which has them',
      lines: entries.map(([name, value]) => `setx ${name} "${value}"`),
    };
  }
  return {
    how: 'Set these in your shell profile',
    lines: entries.map(([name, value]) => {
      const quoted = `'${value.replaceAll("'", "'\\''")}'`;
      return SEARCH_PATHS.has(name) ? `export ${name}=${quoted}"\${${name}:+:\$${name}}"` : `export ${name}=${quoted}`;
    }),
  };
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
    request = parse(args, { cwd, home, platform, tools });
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
    const variables = await tool.installAt(request.dir, await tool.resolve({ runner: runner ?? hostKey() }), { download });
    if (Object.keys(variables).length === 0) {
      io.stdout(`\n${request.tool} installed in ${request.dir}. Nothing needs setting to find it there.\n`);
      return 0;
    }
    const { how, lines } = settings(variables, platform);
    io.stdout(`\n${request.tool} installed in ${request.dir}. ${how}:\n\n${lines.map((line) => `  ${line}`).join('\n')}\n`);
    return 0;
  } catch (err) {
    io.stderr(`${request.tool} was not installed: ${describe(err)}\n`);
    return 1;
  }
}
