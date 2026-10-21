import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdirSync, mkdtempSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { main, services, words } from './run.mjs';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'conformu-run-'));
}

// Every service's command today has this shape.
const command = (name) => `cargo test -p ${name} --features conformu --test conformu_integration -- --nocapture`;

const SERVICES = ['filemonitor', 'qhy-camera', 'zwo-camera'];

function input(names = SERVICES) {
  return JSON.stringify(names.map((name) => ({ name, command: command(name) })));
}

// A program the fake spawn starts in cargo's place: `node -e FAKE <args>`. It
// prints the package its `-p` names, tells the test it is ready, then exits 1
// if FAKE_FAIL names the package, hangs if FAKE_HANG does, and passes
// otherwise.
const FAKE = `
const args = process.argv.slice(1);
const name = args[args.indexOf('-p') + 1];
console.log('suite of ' + name);
process.send('ready', () => {
  if ((process.env.FAKE_HANG ?? '').split(',').includes(name)) return setInterval(() => {}, 1000);
  process.exit((process.env.FAKE_FAIL ?? '').split(',').includes(name) ? 1 : 0);
});
`;

function fakeSpawn({ ready } = {}) {
  const calls = [];
  const children = [];
  const spawnFake = (program, args, options) => {
    calls.push({ program, args, options });
    const child = spawn(process.execPath, ['-e', FAKE, ...args], { ...options, stdio: [...options.stdio, 'ipc'] });
    children.push(child);
    child.on('message', () => ready?.(child));
    return child;
  };
  return { spawn: spawnFake, calls, children };
}

// Runs main with `fake` and resolves to what it printed, the exit code it
// set and the error it threw, if it threw.
async function runMain(fake, { text = input(), env = {}, ...options } = {}) {
  const saved = process.exitCode;
  try {
    const { value: error, stdout } = await capture(() =>
      main({ services: text, env: { ...process.env, ...env }, tmp: tempDir(), spawn: fake.spawn, ...options }).then(
        () => undefined,
        (err) => err,
      ),
    );
    return { stdout, exitCode: process.exitCode, error };
  } finally {
    process.exitCode = saved;
  }
}

// --- The input ----------------------------------------------------------------------

test('a service’s command is its program and the words after it', () => {
  assert.deepEqual(words('zwo-camera', command('zwo-camera')), {
    program: 'cargo',
    args: ['test', '-p', 'zwo-camera', '--features', 'conformu', '--test', 'conformu_integration', '--', '--nocapture'],
  });
  assert.deepEqual(words('x', '  cargo\ttest  -p x '), { program: 'cargo', args: ['test', '-p', 'x'] });
});

test('a command a shell would read otherwise is refused, naming the service and the word', () => {
  for (const word of ['"a b"', "'x'", '$HOME', '`id`', 'a;b', 'a|b', 'a&&b', '>out', '<in', '*.rs', '~/x', 'a\\b', '(x)', '#x', 'a\nb', '{a,b}', '[ab]', '!x']) {
    assert.throws(() => words('zwo-camera', `cargo test ${word}`), (err) => {
      assert.match(err.message, /^the ConformU command of zwo-camera, .* has .* in it, which a shell would read: conformu-run runs a command without one, as words split at its spaces$/);
      return true;
    }, word);
  }
});

test('an empty command is refused', () => {
  assert.throws(() => words('x', ' \t '), { message: 'the ConformU command of x is empty' });
});

test('the services are the input’s, in its order', () => {
  assert.deepEqual(
    services(input()).map(({ name, program, args }) => [name, program, args[2]]),
    SERVICES.map((name) => [name, 'cargo', name]),
  );
});

test('services input that is not a list of named commands is refused', () => {
  const cases = [
    ['not json', /^the services input is not JSON: /],
    ['{"name":"x","command":"cargo test"}', /^the services input is not a JSON list$/],
    ['[]', /^the services input lists no service$/],
    ['[null]', /^service 1 of the services input has no name of letters, digits, _ and -: null$/],
    ['[{"command":"cargo test"}]', /^service 1 of the services input has no name/],
    ['[{"name":"../x","command":"cargo test"}]', /^service 1 of the services input has no name/],
    ['[{"name":"x"}]', /^the service x has no command$/],
    ['[{"name":"x","command":"cargo test"},{"name":"x","command":"cargo test"}]', /^the services input lists x twice$/],
  ];
  for (const [text, message] of cases) assert.throws(() => services(text), { message }, text);
});

test('a refused input runs nothing', async () => {
  const fake = fakeSpawn();
  const { error } = await runMain(fake, { text: '[]' });
  assert.equal(error.message, 'the services input lists no service');
  assert.deepEqual(fake.calls, []);
});

// --- The run ------------------------------------------------------------------------

test('every suite runs at once, by its command, without the runner’s variables or a stdin of the step’s', async () => {
  // Every suite hangs until all are ready, so they cannot have run one after
  // another.
  const stop = new AbortController();
  let ready = 0;
  const fake = fakeSpawn({ ready: () => ++ready === SERVICES.length && stop.abort() });
  const env = { FAKE_HANG: SERVICES.join(','), ACTIONS_RUNTIME_TOKEN: 'secret', INPUT_SERVICES: 'x', CONFORMU_PATH: '/opt/conformu' };
  await runMain(fake, { env, signal: stop.signal, graceMs: 600_000 });
  assert.deepEqual(fake.calls.map(({ program, args }) => [program, ...args].join(' ')), SERVICES.map(command));
  for (const { options } of fake.calls) {
    assert.equal(options.env.ACTIONS_RUNTIME_TOKEN, undefined);
    assert.equal(options.env.INPUT_SERVICES, undefined);
    assert.equal(options.env.CONFORMU_PATH, '/opt/conformu');
    assert.equal(options.stdio[0], 'ignore');
  }
});

test('the suites that pass are named, and each one that fails fails the step by name', async () => {
  const { stdout, exitCode, error } = await runMain(fakeSpawn(), { env: { FAKE_FAIL: 'filemonitor,zwo-camera' } });
  assert.equal(error, undefined);
  assert.equal(exitCode, 1);
  assert.deepEqual(stdout.match(/^::error::.*$/gm), ['::error::ConformU filemonitor failed', '::error::ConformU zwo-camera failed']);
  assert.match(stdout, /^ConformU qhy-camera passed$/m);
});

test('every suite passing passes the step', async () => {
  const { stdout, exitCode, error } = await runMain(fakeSpawn());
  assert.equal(error, undefined);
  assert.equal(exitCode, undefined);
  assert.equal(stdout.match(/^::error::/gm), null);
  assert.match(stdout, /^running 3 ConformU suites at once: filemonitor, qhy-camera, zwo-camera$/m);
});

test('each suite’s output is printed as a group of its own, in the input’s order', async () => {
  const { stdout } = await runMain(fakeSpawn());
  const groups = [...stdout.matchAll(/^::group::ConformU (.*)\n([\s\S]*?)^::endgroup::$/gm)].map(([, name, text]) => [name, text]);
  assert.deepEqual(groups, SERVICES.map((name) => [name, `suite of ${name}\n`]));
});

test('a stopped run fails the step once, and still prints what each suite printed', async () => {
  const stop = new AbortController();
  let ready = 0;
  const fake = fakeSpawn({ ready: () => ++ready === SERVICES.length && stop.abort() });
  const { stdout, exitCode } = await runMain(fake, { env: { FAKE_HANG: SERVICES.join(',') }, signal: stop.signal, graceMs: 600_000 });
  assert.equal(exitCode, 1);
  assert.deepEqual(stdout.match(/^::error::.*$/gm), ['::error::the ConformU suites were stopped before they finished']);
  for (const child of fake.children) assert.equal(child.signalCode, 'SIGTERM', `pid ${child.pid}`);
  for (const name of SERVICES) assert.match(stdout, new RegExp(`^::group::ConformU ${name}\\nsuite of ${name}\\n::endgroup::$`, 'm'));
  assert.equal(stdout.match(/ passed$/gm), null);
});
