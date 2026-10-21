import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { BUILD, HEAVY, SHARDED, baseEnv, build, main, shardCount, streams, suites } from './bdd.mjs';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'bdd-'));
}

// Every package with a bdd target in the workspace, as cargo's messages name
// them: rp, session-runner, polar-align, filemonitor and 21 more.
const WORKSPACE = [
  'calibrator-flats', 'doctor', 'dsd-fp2', 'filemonitor', 'focus-model', 'operation-watchdog-e2e',
  'pa-falcon-rotator', 'pa-scops-oag', 'phd2-guider', 'planetarium-bridge', 'plate-solver',
  'polar-align', 'ppba-driver', 'qhy-camera', 'qhy-focuser', 'rp', 'sentinel', 'session-runner',
  'sky-survey-camera', 'star-adventurer-gti', 'svbony-camera', 'ui-htmx', 'upbv2-driver',
  'zwo-camera', 'zwo-focuser',
];

// What `cargo test --no-run --message-format=json` prints for `artifacts`:
// a bdd test binary per `{ name, exe }`, among messages of the kinds cargo
// prints around them.
function cargoOutput(artifacts, { root = '/w/services' } = {}) {
  const lines = [
    { reason: 'compiler-artifact', target: { name: 'bdd_infra', kind: ['lib'] }, manifest_path: '/w/crates/bdd-infra/Cargo.toml', executable: null },
    ...artifacts.map(({ name, exe }) => ({
      reason: 'compiler-artifact',
      target: { name: 'bdd', kind: ['test'] },
      manifest_path: `${root}/${name}/Cargo.toml`,
      executable: exe ?? `fake:pass:${name}`,
    })),
    { reason: 'compiler-artifact', target: { name: 'rp', kind: ['bin'] }, manifest_path: '/w/services/rp/Cargo.toml', executable: '/w/target/debug/rp' },
    { reason: 'build-finished', success: true },
  ];
  return `${lines.map((l) => JSON.stringify(l)).join('\n')}\n`;
}

// A program the fake spawn starts in a suite's place: `node -e FAKE <cmd>`.
// As cargo it prints FAKE_CARGO_OUT and exits FAKE_CARGO_EXIT. As a suite,
// `fake:<what>:<label>`, it prints its label, BDD_PACKAGE_DIR and its shard,
// tells the test it is ready, then does `what`: pass, fail, fail-shard-<n>
// (fail as shard n only), hang, or stubborn (hang through SIGTERM). The fake
// spawn throws for `fake:throw:<label>`, as spawn does on arguments it
// refuses.
const FAKE = `
const cmd = process.argv[1];
if (cmd === 'cargo') {
  process.stdout.write(process.env.FAKE_CARGO_OUT ?? '');
  process.exit(Number(process.env.FAKE_CARGO_EXIT ?? 0));
}
const [, what, label] = cmd.split(':');
const shard = process.env.TEST_TOTAL_SHARDS ? process.env.TEST_SHARD_INDEX + '/' + process.env.TEST_TOTAL_SHARDS : 'whole';
console.log(label + ' in ' + process.env.BDD_PACKAGE_DIR + ', ' + shard);
if (what === 'stubborn') process.on('SIGTERM', () => {});
process.send('ready', () => {
  if (what === 'pass') process.exit(0);
  if (what === 'fail') process.exit(1);
  if (what.startsWith('fail-shard-')) process.exit(process.env.TEST_SHARD_INDEX === what.slice(11) ? 1 : 0);
  setInterval(() => {}, 1000);
});
`;

// A spawn that runs FAKE in place of each program, recording what it was
// asked to start. `ready` is called with each suite's child once it has
// started and set itself up.
function fakeSpawn({ ready } = {}) {
  const calls = [];
  const children = [];
  const spawnFake = (cmd, args, options) => {
    calls.push({ cmd, args, env: options.env, stdio: options.stdio });
    if (cmd.startsWith('fake:throw:')) throw new TypeError(`cannot spawn ${cmd}`);
    const child = spawn(process.execPath, ['-e', FAKE, cmd, ...args], { ...options, stdio: [...options.stdio, 'ipc'] });
    children.push(child);
    child.on('message', () => ready?.(child));
    return child;
  };
  return { spawn: spawnFake, calls, children };
}

function suiteCalls(calls) {
  return calls.filter(({ cmd }) => cmd !== 'cargo');
}

function shardOf({ env }) {
  return env.TEST_TOTAL_SHARDS === undefined && env.TEST_SHARD_INDEX === undefined ? null : `${env.TEST_SHARD_INDEX}/${env.TEST_TOTAL_SHARDS}`;
}

function running(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

// Runs main with `fake` and resolves to what it printed, the exit code it
// set and the error it threw, if it threw.
async function runMain(fake, { artifacts = WORKSPACE.map((name) => ({ name })), env = {}, cargoExit = 0, out, ...options } = {}) {
  const tmp = tempDir();
  const saved = process.exitCode;
  try {
    const { value: error, stdout } = await capture(() =>
      main({
        rpShards: '4',
        sessionRunnerShards: '2',
        rustflags: '',
        env: { ...process.env, FAKE_CARGO_OUT: out ?? cargoOutput(artifacts), FAKE_CARGO_EXIT: String(cargoExit), ...env },
        tmp,
        spawn: fake.spawn,
        ...options,
      }).then(
        () => undefined,
        (err) => err,
      ),
    );
    return { stdout, exitCode: process.exitCode, error, tmp };
  } finally {
    process.exitCode = saved;
  }
}

// --- The inputs -------------------------------------------------------------------

test('a shard count is a whole number of 1 or more', () => {
  assert.equal(shardCount('rp-shards', '4'), 4);
  assert.equal(shardCount('rp-shards', '1'), 1);
  assert.equal(shardCount('rp-shards', '04'), 4);
  for (const bad of ['', '0', '00', '-1', '1.5', 'x', '4x', '1e2']) {
    assert.throws(() => shardCount('rp-shards', bad), { message: `rp-shards must be a whole number of 1 or more; got ${JSON.stringify(bad)}` }, bad);
  }
});

test('the build and the suites start without the job’s shard, and with the rustflags input over the job’s', () => {
  const job = { PATH: '/bin', RUSTFLAGS: '-Dwarnings', TEST_SHARD_INDEX: '0', TEST_TOTAL_SHARDS: '3' };
  assert.deepEqual(baseEnv(job, ''), { PATH: '/bin', RUSTFLAGS: '-Dwarnings' });
  assert.deepEqual(baseEnv(job, '-D deprecated'), { PATH: '/bin', RUSTFLAGS: '-D deprecated' });
  assert.equal(job.TEST_SHARD_INDEX, '0', 'the job’s own environment is left as it was');
});

// --- What cargo built --------------------------------------------------------------

test('the suites are the bdd test binaries cargo built, each named by its package directory', () => {
  assert.deepEqual(suites(cargoOutput([{ name: 'rp', exe: '/w/target/debug/deps/bdd-1' }, { name: 'session-runner', exe: '/w/target/debug/deps/bdd-2' }])), [
    { name: 'rp', dir: '/w/services/rp', exe: '/w/target/debug/deps/bdd-1' },
    { name: 'session-runner', dir: '/w/services/session-runner', exe: '/w/target/debug/deps/bdd-2' },
  ]);
});

test('a Windows path names its suite too', () => {
  const out = JSON.stringify({
    reason: 'compiler-artifact',
    target: { name: 'bdd' },
    manifest_path: 'D:\\a\\rusty-photon\\services\\rp\\Cargo.toml',
    executable: 'D:\\a\\rusty-photon\\target\\debug\\deps\\bdd-1.exe',
  });
  assert.deepEqual(suites(out), [{ name: 'rp', dir: 'D:\\a\\rusty-photon\\services\\rp', exe: 'D:\\a\\rusty-photon\\target\\debug\\deps\\bdd-1.exe' }]);
});

test('a line of cargo’s output that is not JSON is an error that shows it', () => {
  assert.throws(() => suites(`${cargoOutput([{ name: 'rp' }])}warning: not json\n`), { message: /line 5 of cargo's output is not a JSON message: warning: not json/ });
});

// --- The streams ------------------------------------------------------------------

test('rp and session-runner run as shards, HEAVY in one stream, and every other suite in one more', () => {
  const list = suites(cargoOutput(WORKSPACE.map((name) => ({ name }))));
  const planned = streams(list, { rp: 4, 'session-runner': 2 });
  assert.deepEqual(
    planned.map(({ name, runs }) => [name, runs.map((run) => (run.shard ? `${run.name} ${run.shard.index}/${run.shard.total}` : run.name))]),
    [
      ['rp-0', ['rp 0/4']],
      ['rp-1', ['rp 1/4']],
      ['rp-2', ['rp 2/4']],
      ['rp-3', ['rp 3/4']],
      ['session-runner-0', ['session-runner 0/2']],
      ['session-runner-1', ['session-runner 1/2']],
      ['heavy', ['filemonitor', 'polar-align']],
      ['rest', WORKSPACE.filter((name) => !SHARDED.includes(name) && !HEAVY.includes(name))],
    ],
  );
});

test('every suite cargo built runs in exactly one stream', () => {
  const list = suites(cargoOutput(WORKSPACE.map((name) => ({ name }))));
  const runs = streams(list, { rp: 1, 'session-runner': 1 }).flatMap(({ runs }) => runs.map((run) => run.name));
  assert.deepEqual(runs.toSorted(), WORKSPACE.toSorted());
});

test('a shard count of 1 runs the suite whole, without the shard variables', () => {
  const planned = streams(suites(cargoOutput([{ name: 'rp' }, { name: 'session-runner' }])), { rp: 1, 'session-runner': 3 });
  assert.deepEqual(planned[0], { name: 'rp', runs: [{ name: 'rp', dir: '/w/services/rp', exe: 'fake:pass:rp' }] });
  assert.deepEqual(planned.slice(1).map(({ name }) => name), ['session-runner-0', 'session-runner-1', 'session-runner-2']);
});

test('a stream with no suite is left out', () => {
  const planned = streams(suites(cargoOutput([{ name: 'rp' }, { name: 'session-runner' }])), { rp: 1, 'session-runner': 1 });
  assert.deepEqual(planned.map(({ name }) => name), ['rp', 'session-runner']);
});

test('a missing rp or session-runner is an error', () => {
  for (const name of SHARDED) {
    const list = suites(cargoOutput(WORKSPACE.filter((n) => n !== name).map((n) => ({ name: n }))));
    assert.throws(() => streams(list, { rp: 4, 'session-runner': 2 }), { message: `no bdd binary for ${name}` });
  }
});

test('two binaries named as a sharded suite are an error naming both', () => {
  for (const name of SHARDED) {
    const list = [
      ...suites(cargoOutput([{ name: 'rp' }, { name: 'session-runner' }])),
      ...suites(cargoOutput([{ name }], { root: '/w/plugins' })),
    ];
    assert.throws(() => streams(list, { rp: 4, 'session-runner': 2 }), { message: `two bdd binaries are named ${name}: in /w/services/${name} and in /w/plugins/${name}` });
  }
});

test('two binaries named as any other suite both run', () => {
  const list = [
    ...suites(cargoOutput([{ name: 'rp' }, { name: 'session-runner' }, { name: 'doctor' }, { name: 'filemonitor' }])),
    ...suites(cargoOutput([{ name: 'doctor' }, { name: 'filemonitor' }], { root: '/w/plugins' })),
  ];
  const planned = streams(list, { rp: 1, 'session-runner': 1 });
  assert.deepEqual(planned.slice(2).map(({ name, runs }) => [name, runs.map((run) => run.dir)]), [
    ['heavy', ['/w/services/filemonitor', '/w/plugins/filemonitor']],
    ['rest', ['/w/services/doctor', '/w/plugins/doctor']],
  ]);
});

// --- The build ----------------------------------------------------------------------

test('the build is one cargo test of every bdd target, its JSON kept in the file', async () => {
  const fake = fakeSpawn();
  const file = path.join(tempDir(), 'build.json');
  const out = cargoOutput([{ name: 'rp' }]);
  const built = await build({ env: { ...process.env, FAKE_CARGO_OUT: out }, file, spawn: fake.spawn });
  assert.deepEqual(fake.calls.map(({ cmd, args }) => [cmd, args]), [['cargo', BUILD]]);
  assert.deepEqual(BUILD, ['test', '--locked', '--workspace', '--all-features', '--test', 'bdd', '--no-run', '--message-format=json']);
  assert.equal(fake.calls[0].stdio[0], 'ignore');
  assert.equal(readFileSync(file, 'utf8'), out);
  assert.deepEqual(built, [{ name: 'rp', dir: '/w/services/rp', exe: 'fake:pass:rp' }]);
});

const COMPILE_ERROR = `${JSON.stringify({ reason: 'compiler-message', message: { rendered: 'error[E0425]: cannot find value `x`\n' } })}\n`;

test('a failed build prints the compiler’s messages and throws', async () => {
  const env = { ...process.env, FAKE_CARGO_OUT: COMPILE_ERROR, FAKE_CARGO_EXIT: '101' };
  const { stdout } = await capture(() =>
    assert.rejects(build({ env, file: path.join(tempDir(), 'build.json'), spawn: fakeSpawn().spawn }), {
      message: 'the bdd build failed: cargo exited with 101',
    }),
  );
  assert.equal(stdout, 'error[E0425]: cannot find value `x`\n');
});

test('a failed build runs no suite', async () => {
  const fake = fakeSpawn();
  const { error } = await runMain(fake, { out: COMPILE_ERROR, cargoExit: 101 });
  assert.equal(error.message, 'the bdd build failed: cargo exited with 101');
  assert.deepEqual(suiteCalls(fake.calls), []);
});

test('a stopped build is an error that says so', async () => {
  const stop = new AbortController();
  stop.abort();
  const env = { ...process.env, FAKE_CARGO_OUT: cargoOutput([{ name: 'rp' }]) };
  await assert.rejects(build({ env, file: path.join(tempDir(), 'build.json'), spawn: fakeSpawn().spawn, signal: stop.signal }), {
    message: 'the bdd build was stopped',
  });
});

test('a build that prints no bdd binary is an error', async () => {
  const { error } = await runMain(fakeSpawn(), { artifacts: [] });
  assert.equal(error.message, 'cargo built no bdd binaries');
});

test('a missing rp suite fails before any suite starts', async () => {
  const fake = fakeSpawn();
  const { error } = await runMain(fake, { artifacts: WORKSPACE.filter((n) => n !== 'rp').map((name) => ({ name })) });
  assert.equal(error.message, 'no bdd binary for rp');
  assert.deepEqual(suiteCalls(fake.calls), []);
});

test('a shard count that is not a number fails before the build', async () => {
  const fake = fakeSpawn();
  const { error } = await runMain(fake, { sessionRunnerShards: 'two' });
  assert.equal(error.message, 'session-runner-shards must be a whole number of 1 or more; got "two"');
  assert.deepEqual(fake.calls, []);
});

// --- The run ------------------------------------------------------------------------

test('only rp’s and session-runner’s processes get the shard variables, and every suite runs', async () => {
  const fake = fakeSpawn();
  const { exitCode, error } = await runMain(fake, { env: { TEST_SHARD_INDEX: '1', TEST_TOTAL_SHARDS: '5' } });
  assert.equal(error, undefined);
  assert.equal(exitCode, undefined);
  assert.equal(shardOf(fake.calls[0]), null, 'the build');
  const runs = suiteCalls(fake.calls).map((call) => `${call.env.BDD_PACKAGE_DIR.split('/').pop()} ${shardOf(call) ?? 'whole'}`);
  const expected = [
    ...['0/4', '1/4', '2/4', '3/4'].map((s) => `rp ${s}`),
    ...['0/2', '1/2'].map((s) => `session-runner ${s}`),
    ...WORKSPACE.filter((n) => !SHARDED.includes(n)).map((n) => `${n} whole`),
  ];
  assert.deepEqual(runs.toSorted(), expected.toSorted());
});

test('each suite starts in its package directory, with no stdin of the step’s, and the rustflags input', async () => {
  const fake = fakeSpawn();
  await runMain(fake, { rustflags: '-D deprecated', env: { RUSTFLAGS: '-Dwarnings' } });
  assert.equal(fake.calls[0].env.RUSTFLAGS, '-D deprecated', 'the build');
  for (const call of suiteCalls(fake.calls)) {
    const name = call.cmd.split(':')[2];
    assert.equal(call.env.BDD_PACKAGE_DIR, `/w/services/${name}`);
    assert.equal(call.env.RUSTFLAGS, '-D deprecated', name);
    assert.equal(call.stdio[0], 'ignore', name);
  }
});

test('one failing shard fails the step, by its stream’s name alone', async () => {
  const artifacts = WORKSPACE.map((name) => ({ name, exe: name === 'rp' ? 'fake:fail-shard-2:rp' : undefined }));
  const { stdout, exitCode } = await runMain(fakeSpawn(), { artifacts });
  assert.equal(exitCode, 1);
  assert.deepEqual(stdout.match(/^::error::.*$/gm), ['::error::BDD stream rp-2 failed']);
});

test('a stream runs every suite in it after one fails, and fails', async () => {
  const fake = fakeSpawn();
  const artifacts = WORKSPACE.map((name) => ({ name, exe: name === 'filemonitor' ? 'fake:fail:filemonitor' : undefined }));
  const { stdout, exitCode } = await runMain(fake, { artifacts });
  assert.equal(exitCode, 1);
  assert.deepEqual(stdout.match(/^::error::.*$/gm), ['::error::BDD stream heavy failed']);
  const heavy = suiteCalls(fake.calls).map(({ cmd }) => cmd.split(':')[2]).filter((name) => HEAVY.includes(name));
  assert.deepEqual(heavy, ['filemonitor', 'polar-align']);
});

test('each stream’s output is printed as a group of its own, its suites in order', async () => {
  const { stdout } = await runMain(fakeSpawn());
  const groups = [...stdout.matchAll(/^::group::bdd (.*)\n([\s\S]*?)^::endgroup::$/gm)].map(([, name, text]) => [name, text]);
  assert.deepEqual(groups.map(([name]) => name), ['rp-0', 'rp-1', 'rp-2', 'rp-3', 'session-runner-0', 'session-runner-1', 'heavy', 'rest']);
  assert.equal(groups[1][1], 'rp in /w/services/rp, 1/4\n');
  assert.equal(groups[6][1], 'filemonitor in /w/services/filemonitor, whole\npolar-align in /w/services/polar-align, whole\n');
  assert.equal(groups[7][1].split('\n').length - 1, 21);
  assert.match(stdout, /^built 25 bdd binaries$/m);
});

test('a stopped run stops every suite, starts no other, and still prints what each printed', async () => {
  const stop = new AbortController();
  // Every suite hangs, so each of the eight streams has one running when the
  // last of them is ready, and none has started its second.
  const artifacts = WORKSPACE.map((name) => ({ name, exe: `fake:hang:${name}` }));
  let ready = 0;
  const fake = fakeSpawn({ ready: () => ++ready === 8 && stop.abort() });
  const { stdout, exitCode } = await runMain(fake, { artifacts, signal: stop.signal, graceMs: 600_000 });
  assert.equal(exitCode, 1);
  assert.deepEqual(stdout.match(/^::error::.*$/gm), ['::error::the BDD streams were stopped before they finished']);
  for (const child of fake.children.slice(1)) assert.equal(child.signalCode, 'SIGTERM', `pid ${child.pid}`);
  assert.deepEqual(suiteCalls(fake.calls).map(({ cmd }) => cmd.split(':')[2]).toSorted(), [
    'calibrator-flats', 'filemonitor', 'rp', 'rp', 'rp', 'rp', 'session-runner', 'session-runner',
  ]);
  for (const child of fake.children) assert.equal(running(child.pid), false, `pid ${child.pid}`);
  assert.match(stdout, /^::group::bdd rp-0\nrp in \/w\/services\/rp, 0\/4\n::endgroup::$/m);
  assert.match(stdout, /^::group::bdd rest\ncalibrator-flats in \/w\/services\/calibrator-flats, whole\n::endgroup::$/m);
});

test('a stream that throws stops the others, starts no other suite, and still prints the logs', async () => {
  // HEAVY's first suite throws as it starts; every other suite hangs until stopped.
  const artifacts = WORKSPACE.map((name) => ({ name, exe: `fake:${name === 'filemonitor' ? 'throw' : 'hang'}:${name}` }));
  const fake = fakeSpawn();
  const { stdout, error } = await runMain(fake, { artifacts, graceMs: 600_000 });
  assert.equal(error.message, 'cannot spawn fake:throw:filemonitor');
  assert.equal(suiteCalls(fake.calls).length, 8, 'no stream started a second suite');
  for (const child of fake.children) assert.equal(running(child.pid), false, `pid ${child.pid}`);
  // Stopped as they started, the suites may have printed nothing, but each
  // stream's group is there.
  assert.deepEqual([...stdout.matchAll(/^::group::bdd (.*)$/gm)].map(([, name]) => name), [
    'rp-0', 'rp-1', 'rp-2', 'rp-3', 'session-runner-0', 'session-runner-1', 'heavy', 'rest',
  ]);
});
