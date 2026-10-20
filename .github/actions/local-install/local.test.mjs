import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import test from 'node:test';

import * as astap from '../install-astap/astap.mjs';
import * as omnisim from '../install-omnisim/omnisim.mjs';
import * as pebble from '../install-pebble/pebble.mjs';
import * as svbonySdk from '../install-svbony-sdk/svbony-sdk.mjs';
import * as zwoSdk from '../install-zwo-sdk/zwo-sdk.mjs';
import { hostKey } from '../lib/install.mjs';
import { capture, withEnv } from '../lib/testing.mjs';
import { main, parse, settings, TOOLS, usage, UsageError } from './local.mjs';

// --- Fixtures -----------------------------------------------------------------

// Absolute on every OS: resolve gives them Windows' drive, as parse's does.
const HOME = resolve('/', 'home', 'dev');
const CWD = resolve('/', 'work', 'rusty-photon');

// A tool that records what it was asked to do, installs nothing, and
// resolves to `variables` of the directory it was given.
function fakeTools({ fail, variables = (dir) => ({ TOOL_PATH: join(dir, 'tool'), TOOL_HOME: dir }) } = {}) {
  const calls = [];
  const tool = {
    about: 'a tool that records its calls',
    resolve: ({ runner }) => ({ runner }),
    installAt: async (dir, resolved, options) => {
      calls.push({ dir, resolved, options });
      if (fail) throw new Error('fetch failed', { cause: new Error('connect ECONNREFUSED') });
      return variables(dir);
    },
  };
  return { tools: { tool }, calls };
}

// --- The command line -----------------------------------------------------------

test('parse installs a tool into ~/.local/opt/<tool> by default', () => {
  assert.deepEqual(parse(['pebble'], { cwd: CWD, home: HOME }), { tool: 'pebble', dir: join(HOME, '.local', 'opt', 'pebble') });
});

test('parse installs the ZWO SDK where its build looks, but on Windows into ~/.local/opt/zwo-sdk', () => {
  assert.deepEqual(parse(['zwo-sdk'], { cwd: CWD, home: HOME, platform: 'linux' }), { tool: 'zwo-sdk', dir: zwoSdk.PREFIX });
  assert.deepEqual(parse(['zwo-sdk'], { cwd: CWD, home: HOME, platform: 'darwin' }).dir, zwoSdk.PREFIX);
  assert.deepEqual(parse(['zwo-sdk'], { cwd: CWD, home: HOME, platform: 'win32' }).dir, join(HOME, '.local', 'opt', 'zwo-sdk'));
  assert.deepEqual(parse(['zwo-sdk', '--prefix', 'sdk'], { cwd: CWD, home: HOME, platform: 'linux' }).dir, join(CWD, 'sdk'));
});

test('parse installs the SVBony SDK where its build looks, but on Windows into ~/.local/opt/svbony-sdk', () => {
  assert.deepEqual(parse(['svbony-sdk'], { cwd: CWD, home: HOME, platform: 'linux' }), { tool: 'svbony-sdk', dir: svbonySdk.PREFIX });
  assert.deepEqual(parse(['svbony-sdk'], { cwd: CWD, home: HOME, platform: 'darwin' }).dir, svbonySdk.PREFIX);
  assert.deepEqual(parse(['svbony-sdk'], { cwd: CWD, home: HOME, platform: 'win32' }).dir, join(HOME, '.local', 'opt', 'svbony-sdk'));
});

test('parse takes --prefix, a relative one from where bazel ran', () => {
  assert.deepEqual(parse(['omnisim', '--prefix', 'tools/omnisim'], { cwd: CWD, home: HOME }), {
    tool: 'omnisim',
    dir: join(CWD, 'tools', 'omnisim'),
  });
  const absolute = resolve('/', 'opt', 'pebble');
  assert.deepEqual(parse([`--prefix=${absolute}`, 'pebble'], { cwd: CWD, home: HOME }).dir, absolute);
});

test('parse expands a leading ~ in --prefix=, which no shell expands there', () => {
  assert.equal(parse(['pebble', '--prefix=~/tools/pebble'], { cwd: CWD, home: HOME }).dir, join(HOME, 'tools', 'pebble'));
  assert.equal(parse(['pebble', '--prefix=~'], { cwd: CWD, home: HOME }).dir, HOME);
  assert.equal(parse(['pebble', '--prefix=~dev/x'], { cwd: CWD, home: HOME }).dir, join(CWD, '~dev', 'x'));
  assert.equal(parse(['pebble', '--prefix=a/~'], { cwd: CWD, home: HOME }).dir, join(CWD, 'a', '~'));
});

test('parse answers --help and -h', () => {
  assert.deepEqual(parse(['--help'], { cwd: CWD, home: HOME }), { help: true });
  assert.deepEqual(parse(['pebble', '-h'], { cwd: CWD, home: HOME }), { help: true });
});

test('parse refuses a command line it cannot run', () => {
  const refused = [
    [[], 'name the tool to install'],
    [['pebble', 'omnisim'], 'one tool at a time; got pebble, omnisim'],
    [['nothing'], 'no tool named nothing; there are astap, omnisim, pebble, svbony-sdk, zwo-sdk'],
    [['toString'], 'no tool named toString; there are astap, omnisim, pebble, svbony-sdk, zwo-sdk'],
    [['pebble', '--prefix'], /Option '--prefix <value>' argument missing/],
    [['pebble', '--prefix='], '--prefix needs a directory'],
    [['pebble', '--dir', 'x'], /Unknown option '--dir'/],
  ];
  for (const [args, message] of refused) {
    assert.throws(() => parse(args, { cwd: CWD, home: HOME }), (err) => {
      assert.ok(err instanceof UsageError, `${args.join(' ')}: ${err}`);
      if (typeof message === 'string') assert.equal(err.message, message);
      else assert.match(err.message, message);
      return true;
    });
  }
});

test('settings writes lines for the shell profile that a POSIX shell reads back as the same value', () => {
  assert.deepEqual(settings({ A_PATH: "/home/o'brien/a b/a" }, 'linux'), {
    how: 'Set these in your shell profile',
    lines: [`export A_PATH='/home/o'\\''brien/a b/a'`],
  });
  const sh = spawnSync('sh', ['-c', `${settings({ A_PATH: "/x/o'b $HOME/y" }, 'linux').lines[0]}; printf %s "$A_PATH"`], {
    encoding: 'utf8',
  });
  if (!sh.error) assert.equal(sh.stdout, "/x/o'b $HOME/y");
});

test('settings puts a library directory first on LD_LIBRARY_PATH and keeps what the shell had', () => {
  assert.deepEqual(settings({ LD_LIBRARY_PATH: '/usr/local/lib' }, 'linux').lines, [
    `export LD_LIBRARY_PATH='/usr/local/lib'"\${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"`,
  ]);
  const line = settings({ LD_LIBRARY_PATH: "/x/o'b" }, 'linux').lines[0];
  const read = (before) =>
    spawnSync('sh', ['-c', `${before}${line}; printf %s "$LD_LIBRARY_PATH"`], { encoding: 'utf8', env: { PATH: process.env.PATH ?? '' } });
  const unset = read('unset LD_LIBRARY_PATH; ');
  if (!unset.error) {
    assert.equal(unset.stdout, "/x/o'b");
    assert.equal(read("LD_LIBRARY_PATH='/opt/a b'; ").stdout, "/x/o'b:/opt/a b");
  }
});

test('settings writes setx lines on Windows, to run once before opening a new shell', () => {
  assert.deepEqual(settings({ A_PATH: 'C:\\Users\\dev\\a', B_PATH: 'C:\\b' }, 'win32'), {
    how: 'Run these once, then open a new shell, which has them',
    lines: ['setx A_PATH "C:\\Users\\dev\\a"', 'setx B_PATH "C:\\b"'],
  });
});

test('TOOLS installs each tool by its install-* action module', () => {
  assert.deepEqual(Object.keys(TOOLS), ['astap', 'omnisim', 'pebble', 'svbony-sdk', 'zwo-sdk']);
  assert.equal(TOOLS.astap.resolve, astap.resolve);
  assert.equal(TOOLS.astap.installAt, astap.installAt);
  assert.equal(TOOLS.omnisim.resolve, omnisim.resolve);
  assert.equal(TOOLS.omnisim.installAt, omnisim.installAt);
  assert.equal(TOOLS.pebble.resolve, pebble.resolve);
  assert.equal(TOOLS.pebble.installAt, pebble.installAt);
  assert.equal(TOOLS['svbony-sdk'].resolve, svbonySdk.resolve);
  assert.equal(TOOLS['svbony-sdk'].installAt, svbonySdk.installAt);
  assert.equal(TOOLS['zwo-sdk'].resolve, zwoSdk.resolve);
  assert.equal(TOOLS['zwo-sdk'].installAt, zwoSdk.installAt);
  // Each description starts in one column, past the longest name.
  assert.match(usage(), /\n {2}astap {6}ASTAP's CLI and D05 star database/);
  assert.match(usage(), /\n {2}omnisim {4}ASCOM OmniSim/);
  assert.match(usage(), /\n {2}pebble {5}Pebble and pebble-challtestsrv/);
  assert.match(usage(), /\n {2}svbony-sdk SVBony's SDK, which svbony-camera links; into \/usr\/local, on Linux and Intel Macs only/);
  assert.match(usage(), /\n {2}zwo-sdk {4}ZWO's SDKs, which zwo-camera and zwo-focuser link; into \/usr\/local/);
});

// --- Running it -------------------------------------------------------------------

test('main installs the tool for the runner given, and prints the variables to set', async () => {
  const { tools, calls } = fakeTools();
  const download = { delayMs: 1 };
  const { value, stdout, stderr } = await capture(() =>
    main(['tool', '--prefix', 'opt/tool'], { cwd: CWD, home: HOME, platform: 'linux', runner: 'Linux-ARM64', tools, download }),
  );
  assert.equal(value, 0);
  assert.equal(stderr, '');
  const dir = join(CWD, 'opt', 'tool');
  assert.deepEqual(calls, [{ dir, resolved: { runner: 'Linux-ARM64' }, options: { download } }]);
  assert.equal(
    stdout,
    `\ntool installed in ${dir}. Set these in your shell profile:\n\n` +
      `  export TOOL_PATH='${join(dir, 'tool')}'\n  export TOOL_HOME='${dir}'\n`,
  );
});

test('main tells a Windows developer to run the setx lines once and open a new shell', async () => {
  const { tools } = fakeTools();
  const { value, stdout } = await capture(() => main(['tool'], { cwd: CWD, home: HOME, platform: 'win32', tools }));
  assert.equal(value, 0);
  const dir = join(HOME, '.local', 'opt', 'tool');
  assert.equal(
    stdout,
    `\ntool installed in ${dir}. Run these once, then open a new shell, which has them:\n\n` +
      `  setx TOOL_PATH "${join(dir, 'tool')}"\n  setx TOOL_HOME "${dir}"\n`,
  );
});

test('main says so when a tool needs no variable set where it installed', async () => {
  const { tools } = fakeTools({ variables: () => ({}) });
  const { value, stdout } = await capture(() => main(['tool', '--prefix', '/opt/x'], { cwd: CWD, home: HOME, platform: 'linux', tools }));
  assert.equal(value, 0);
  assert.equal(stdout, `\ntool installed in ${resolve('/opt/x')}. Nothing needs setting to find it there.\n`);
});

test('main resolves for this machine when no runner is given', async () => {
  const { tools, calls } = fakeTools();
  await capture(() => main(['tool'], { cwd: CWD, home: HOME, tools }));
  assert.deepEqual(calls[0].resolved, { runner: hostKey() });
  assert.equal(calls[0].dir, join(HOME, '.local', 'opt', 'tool'));
});

test('main takes a relative --prefix from BUILD_WORKING_DIRECTORY, where bazel run started', async () => {
  const { tools, calls } = fakeTools();
  await withEnv({ BUILD_WORKING_DIRECTORY: CWD }, () => capture(() => main(['tool', '--prefix', 'here'], { home: HOME, tools })));
  assert.equal(calls[0].dir, join(CWD, 'here'));
});

test('main says why an install failed, with its causes, and exits 1', async () => {
  const { tools } = fakeTools({ fail: true });
  const { value, stdout, stderr } = await capture(() => main(['tool'], { cwd: CWD, home: HOME, tools }));
  assert.equal(value, 1);
  assert.equal(stdout, '');
  assert.equal(stderr, 'tool was not installed: fetch failed: connect ECONNREFUSED\n');
});

test('main prints the usage for --help, and with the reason for a bad command line', async () => {
  const { tools, calls } = fakeTools();
  const help = await capture(() => main(['--help'], { cwd: CWD, home: HOME, tools }));
  assert.equal(help.value, 0);
  assert.equal(help.stdout, `${usage(tools)}\n`);
  const bad = await capture(() => main(['nothing'], { cwd: CWD, home: HOME, tools }));
  assert.equal(bad.value, 2);
  assert.equal(bad.stderr, `no tool named nothing; there are tool\n\n${usage(tools)}\n`);
  assert.deepEqual(calls, []);
});

// --- The launcher -----------------------------------------------------------------

// The launcher `bazel run //.github/actions:install` starts, run from the
// runfiles tree as bazel run does: what it is given after `--` reaches main,
// and main's exit code comes back.
function launch(args) {
  const windows = process.platform === 'win32';
  const launcher = join('.github', 'actions', windows ? 'install.bat' : 'install.sh');
  assert.ok(existsSync(launcher), `${launcher} is not in the runfiles`);
  // cmd runs a batch file; the command line is built here, since Node warns
  // that it joins a shell's arguments without quoting them.
  return windows
    ? spawnSync(`${launcher} ${args.join(' ')}`, { encoding: 'utf8', shell: true })
    : spawnSync(launcher, args, { encoding: 'utf8' });
}

test('the launcher passes its arguments through to main', () => {
  const help = launch(['--help']);
  assert.equal(help.status, 0, help.stderr);
  assert.match(help.stdout, /^usage: bazel run \/\/\.github\/actions:install -- <tool> \[--prefix DIR\]/);
  const bad = launch(['pebble', '--dir']);
  assert.equal(bad.status, 2, bad.stderr);
  assert.match(bad.stderr, /Unknown option '--dir'/);
});
