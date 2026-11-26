import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { appendFileSync, mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { afterEach, test } from 'node:test';

import { io } from '../lib/helpers.mjs';
import { capture } from '../lib/testing.mjs';
import { callGraph, commandPort, connections, diagnose, ENOBUFS, JVM_OUT_LINES, jvmOutSince, LOG_LINES, MAX_DUMPS, redact, watch } from './probe.mjs';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'bazel-stall-probe-'));
}

const SILENCE = 300_000;

// A test whose assertion fails before its fake Bazel exits would leave the
// fake running, and its IPC channel would hold the file open until Bazel's
// timeout instead of reporting the failure. So each test ends every fake it
// started, and puts back the step's log.
const LIVE = new Set();
const IO = { ...io };
afterEach(() => {
  for (const child of LIVE) child.kill('SIGKILL');
  LIVE.clear();
  Object.assign(io, IO);
});

// A program the fake spawn starts in Bazel's place: `node -e FAKE`. It says
// when it is ready, then prints and exits as the test's messages tell it.
const FAKE = `
process.on('message', (m) => {
  if (m.ignoreTerm) process.on('SIGTERM', () => {});
  if (m.out !== undefined) process.stdout.write(m.out);
  if (m.err !== undefined) process.stderr.write(m.err);
  if (m.exit !== undefined) process.exit(m.exit);
});
process.send('ready');
`;

// Starts `watch` with a fake Bazel, a clock the test moves and a check it
// runs, and the step's log recorded. `log.seen(text)` resolves once `text`
// has been printed, so the test acts on output that has arrived, not on time.
async function started({ diagnose: dump, ...options } = {}) {
  const calls = [];
  let ready;
  const isReady = new Promise((resolve) => {
    ready = resolve;
  });
  let child;
  const fakeSpawn = (program, args, opts) => {
    calls.push({ program, args, opts });
    child = spawn(process.execPath, ['-e', FAKE], { ...opts, stdio: [...opts.stdio, 'ipc'] });
    LIVE.add(child);
    child.on('close', () => LIVE.delete(child));
    child.on('message', () => ready());
    return child;
  };
  const clock = { t: 0, check: null, cancelled: false };
  const dumps = [];
  const printed = [];
  const waiters = [];
  const saved = { ...io };
  const record = (text) => {
    printed.push(text);
    const all = printed.join('');
    for (const waiter of [...waiters]) {
      if (all.includes(waiter.text)) {
        waiters.splice(waiters.indexOf(waiter), 1);
        waiter.resolve();
      }
    }
  };
  io.stdout = record;
  io.stderr = record;
  const log = {
    text: () => printed.join(''),
    seen: (text) =>
      printed.join('').includes(text)
        ? Promise.resolve()
        : new Promise((resolve) => {
            waiters.push({ text, resolve });
          }),
  };
  const result = watch({
    args: ['build', '//...'],
    silenceMs: SILENCE,
    diagnose:
      dump ??
      (async (seen) => {
        dumps.push(seen);
      }),
    spawn: fakeSpawn,
    now: () => clock.t,
    repeat: (ms, fn) => {
      clock.check = fn;
      clock.period = ms;
      return () => {
        clock.cancelled = true;
      };
    },
    ...options,
  }).finally(() => Object.assign(io, saved));
  await isReady;
  // Moves the clock on by `ms`, runs the check, and lets a dump it starts finish.
  const advance = async (ms) => {
    clock.t += ms;
    clock.check();
    await new Promise((resolve) => setImmediate(resolve));
  };
  const exited = new Promise((resolve) => child.on('close', resolve));
  return { send: (message) => child.send(message), pid: child.pid, result, exited, calls, clock, dumps, log, advance };
}

// --- watch ----------------------------------------------------------------------------

test('watch runs bazel without a shell or the runner variables, copies its output and resolves to its exit code', async () => {
  const run = await started({ env: { KEPT: '1' } });
  run.send({ out: 'one\n', err: 'two\n' });
  await run.log.seen('one\n');
  await run.log.seen('two\n');
  run.send({ exit: 3 });
  assert.deepEqual(await run.result, { code: 3, signal: null });
  assert.equal(run.calls.length, 1);
  assert.equal(run.calls[0].program, 'bazel');
  assert.deepEqual(run.calls[0].args, ['build', '//...']);
  assert.deepEqual(run.calls[0].opts.env, { KEPT: '1' });
  assert.equal(run.calls[0].opts.shell, undefined);
  assert.equal(run.clock.cancelled, true);
});

test('watch checks for silence at most every 15 s', async () => {
  const run = await started();
  assert.equal(run.clock.period, 15_000);
  run.send({ exit: 0 });
  await run.result;
});

test('watch dumps nothing while the output keeps coming', async () => {
  const run = await started();
  run.send({ out: 'one\n' });
  await run.log.seen('one\n');
  await run.advance(SILENCE - 1);
  run.send({ out: 'two\n' });
  await run.log.seen('two\n');
  await run.advance(SILENCE - 1);
  run.send({ exit: 0 });
  assert.deepEqual(await run.result, { code: 0, signal: null });
  assert.deepEqual(run.dumps, []);
});

test('watch dumps when the output stops for the silence, again at each further silence, and no more than its limit', async () => {
  const run = await started();
  run.send({ out: 'start\n' });
  await run.log.seen('start\n');
  await run.advance(SILENCE);
  assert.ok(Number.isInteger(run.pid) && run.pid > 0);
  assert.deepEqual(run.dumps, [{ silentForMs: SILENCE, enobufs: null, bazelPid: run.pid }]);
  await run.advance(SILENCE / 2);
  assert.equal(run.dumps.length, 1);
  await run.advance(SILENCE / 2);
  await run.advance(SILENCE);
  await run.advance(SILENCE * 10);
  assert.deepEqual(
    run.dumps.map((dump) => dump.silentForMs),
    [SILENCE, 2 * SILENCE, 3 * SILENCE],
  );
  assert.equal(run.dumps.length, MAX_DUMPS);
  run.send({ exit: 0 });
  assert.deepEqual(await run.result, { code: 0, signal: null });
});

test('watch counts the silence again from output that comes after a dump', async () => {
  const run = await started();
  run.send({ out: 'start\n' });
  await run.log.seen('start\n');
  await run.advance(SILENCE);
  run.send({ out: 'moving again\n' });
  await run.log.seen('moving again\n');
  await run.advance(SILENCE);
  assert.deepEqual(
    run.dumps.map((dump) => dump.silentForMs),
    [SILENCE, SILENCE],
  );
  run.send({ exit: 0 });
  await run.result;
});

test('watch hands a dump the last ENOBUFS line, even one that came in two chunks, and how long ago it came', async () => {
  const run = await started();
  run.send({ err: 'WARNING: Remote Cache: No buffer ' });
  await run.log.seen('No buffer ');
  run.send({ err: 'space available\nat waitForBulkTransfer\n' });
  await run.log.seen('waitForBulkTransfer');
  await run.advance(SILENCE);
  assert.deepEqual(run.dumps, [{ silentForMs: SILENCE, enobufs: { line: `WARNING: Remote Cache: ${ENOBUFS}`, agoMs: SILENCE }, bazelPid: run.pid }]);
  run.send({ exit: 0 });
  await run.result;
});

test('watch waits for a dump in progress before it resolves, and a dump that fails ends nothing', async () => {
  let finish;
  const run = await started({
    diagnose: () =>
      new Promise((resolve, reject) => {
        finish = () => reject(new Error('jstack fell over'));
      }),
  });
  await run.advance(SILENCE);
  run.send({ exit: 0 });
  let settled = false;
  run.result.then(() => {
    settled = true;
  });
  // Bazel has ended; only the dump keeps `watch` from resolving.
  await run.exited;
  for (let turn = 0; turn < 5; turn += 1) await new Promise((resolve) => setImmediate(resolve));
  assert.equal(settled, false);
  finish();
  assert.deepEqual(await run.result, { code: 0, signal: null });
  assert.match(run.log.text(), /bazel-stall-probe: the dump failed: jstack fell over/);
});

// Bounded so that a stop that never reaches Bazel fails by this test's name;
// a stop that works ends the fake in milliseconds.
test('watch stops bazel when the step is stopped', { timeout: 30_000 }, async () => {
  const stop = new AbortController();
  const run = await started({ signal: stop.signal });
  stop.abort();
  const ended = await run.result;
  assert.notEqual(ended.code, 0);
});

// Bounded like the test above: a stop that never escalates fails by name.
test('watch kills bazel when it outlives the stop’s grace', { timeout: 30_000 }, async () => {
  const stop = new AbortController();
  const run = await started({ signal: stop.signal, graceMs: 50 });
  run.send({ ignoreTerm: true, out: 'ignoring SIGTERM\n' });
  await run.log.seen('ignoring SIGTERM');
  stop.abort();
  const ended = await run.result;
  // Windows has no SIGTERM to ignore: the first stop kills it.
  if (process.platform !== 'win32') assert.equal(ended.signal, 'SIGKILL');
  assert.notEqual(ended.code, 0);
});

test('watch rejects when bazel cannot start', async () => {
  const cancelled = { value: false };
  await assert.rejects(
    watch({
      args: ['build'],
      silenceMs: SILENCE,
      diagnose: async () => {},
      spawn: (program, args, opts) => spawn('bazel-stall-probe-no-such-program', args, opts),
      repeat: () => () => {
        cancelled.value = true;
      },
    }),
    /ENOENT/,
  );
  assert.equal(cancelled.value, true);
});

// --- diagnose -------------------------------------------------------------------------

// An output base holding a server's pid file, its port file and both logs.
function outputBase({ pid = '4242', port = '127.0.0.1:9000', jvm = 'thread dump\n', javaLog = 'log line\n' } = {}) {
  const dir = tempDir();
  mkdirSync(path.join(dir, 'server'));
  if (pid !== null) writeFileSync(path.join(dir, 'server', 'server.pid.txt'), pid);
  if (port !== null) writeFileSync(path.join(dir, 'server', 'command_port'), port);
  if (jvm !== null) writeFileSync(path.join(dir, 'server', 'jvm.out'), jvm);
  if (javaLog !== null) writeFileSync(path.join(dir, 'java.log'), javaLog);
  return dir;
}

// The text under each `--- title ---` line of a dump, by title.
function sections(stdout) {
  const found = {};
  let title = null;
  for (const line of stdout.split('\n')) {
    const header = /^--- (.*) ---$/.exec(line);
    if (header) {
      title = header[1];
      found[title] = [];
    } else if (title !== null) {
      found[title].push(line);
    }
  }
  return Object.fromEntries(Object.entries(found).map(([name, lines]) => [name, lines.join('\n')]));
}

const ANSWERS = {
  pgrep: '17 bazel --remote_header=Authorization=Bearer s3cret\n',
  netstat:
    'Active Internet connections\nProto Recv-Q Send-Q  Local Address          Foreign Address        (state)\n' +
    'tcp4       0  52000  10.0.0.1.50000         1.2.3.4.443            ESTABLISHED\n' +
    'tcp4       0      0  127.0.0.1.9000         127.0.0.1.9001         ESTABLISHED\n' +
    'tcp4  130000      0  127.0.0.1.9001         127.0.0.1.9000         ESTABLISHED\n' +
    'tcp4       0      0  127.0.0.1.9000         *.*                    LISTEN\n' +
    'tcp4       0      0  10.0.0.1.50001         1.2.3.4.443            TIME_WAIT\n' +
    'Active LOCAL (UNIX) domain sockets\nAddress          Type   Recv-Q Send-Q\n',
  vm_stat: 'Pages free: 1234.\n',
  children: '2182\n',
  sample: 'Call graph:\n    1 Thread_1\n      1 __recvmsg\n\nBinary Images:\n    0x100000000 - 0x100ffffff +bazel\n',
  jstack: '"netty-event-loop-1" RUNNABLE\n',
};

function fakes({ answers = ANSWERS, alive = true } = {}) {
  const runs = [];
  const signals = [];
  const sleeps = [];
  return {
    runs,
    signals,
    sleeps,
    run: async (program, args) => {
      runs.push([program, ...args]);
      let answer = answers[program];
      if (program === 'netstat' && args[0] === '-m') answer = '512 mbufs in use\n';
      if (program === 'pgrep' && args[0] === '-P') answer = answers.children;
      if (answer instanceof Error) throw answer;
      return answer ?? '';
    },
    kill: (pid, sig) => {
      signals.push([pid, sig]);
      if (!alive) throw Object.assign(new Error('kill ESRCH'), { code: 'ESRCH' });
      return true;
    },
    sleep: async (ms) => {
      sleeps.push(ms);
    },
  };
}

test('diagnose dumps the processes, the connections to the cache and to the server, the network buffers, the client’s stacks, the server threads and its logs, in a group', async () => {
  const f = fakes();
  const dir = outputBase({ jvm: 'JVM warning\nException in thread "grpc-command-0"\nFull thread dump OpenJDK\n"main" WAITING\nAuthorization: Bearer abc123\n' });
  const { stdout } = await capture(() =>
    diagnose({ silentForMs: SILENCE, enobufs: { line: 'WARNING: No buffer space available', agoMs: 12_000 }, bazelPid: 2181, outputBase: dir, platform: 'darwin', ...f }),
  );
  const dump = sections(stdout);
  assert.match(stdout, /^::group::bazel-stall-probe: no output for 300 s\n/);
  assert.match(stdout, /ENOBUFS came 12 s before this dump: WARNING: No buffer space available\n/);
  const cache = dump['TCP connections to port 443 (the remote cache)'];
  assert.match(cache, /Proto Recv-Q Send-Q/);
  assert.match(cache, /1\.2\.3\.4\.443 +ESTABLISHED/);
  assert.doesNotMatch(cache, /127\.0\.0\.1|TIME_WAIT +\n|UNIX/);
  assert.match(cache, /\(1 in TIME_WAIT left out\)/);
  // Both ends of the client's connection to the server, and the server's listener.
  const server = dump['TCP connections to port 9000 (the Bazel client and its server)'];
  assert.match(server, /Proto Recv-Q Send-Q/);
  assert.match(server, /127\.0\.0\.1\.9000 +127\.0\.0\.1\.9001 +ESTABLISHED/);
  assert.match(server, /130000 +0 +127\.0\.0\.1\.9001 +127\.0\.0\.1\.9000 +ESTABLISHED/);
  assert.match(server, /127\.0\.0\.1\.9000 +\*\.\* +LISTEN/);
  assert.doesNotMatch(server, /443/);
  assert.match(dump['network buffers (netstat -m)'], /512 mbufs in use/);
  assert.match(dump['memory (vm_stat)'], /Pages free: 1234\./);
  // The bazel process and its child, each to the end of its call graph.
  for (const pid of [2181, 2182]) {
    assert.match(dump[`stacks of ${pid} (sample, 1 s)`], /Call graph:\n +1 Thread_1\n +1 __recvmsg/);
    assert.doesNotMatch(dump[`stacks of ${pid} (sample, 1 s)`], /Binary Images/);
  }
  assert.match(dump['jstack 4242'], /"netty-event-loop-1" RUNNABLE/);
  // The first dump prints jvm.out from its first line.
  assert.match(dump['server/jvm.out'], /^JVM warning\nException in thread "grpc-command-0"\nFull thread dump OpenJDK\n"main" WAITING\n/);
  assert.match(dump[`last ${LOG_LINES} lines of java.log`], /log line/);
  assert.doesNotMatch(stdout, /s3cret|abc123/);
  assert.match(stdout, /Authorization=Bearer \[REDACTED\]/);
  assert.match(stdout, /::endgroup::\n$/);
  assert.deepEqual(f.runs, [
    ['pgrep', '-fl', 'bazel|java'],
    ['netstat', '-an'],
    ['netstat', '-an'],
    ['netstat', '-m'],
    ['vm_stat'],
    ['pgrep', '-P', '2181'],
    ['sample', '2181', '1'],
    ['sample', '2182', '1'],
    ['jstack', '4242'],
  ]);
  // The server gets a liveness check and SIGQUIT, and nothing that could stop it.
  assert.deepEqual(f.signals, [
    [4242, 0],
    [4242, 'SIGQUIT'],
  ]);
  assert.deepEqual(f.sleeps, [3000]);
});

test('diagnose reports a command that fails and goes on, and dumps no threads of a server that is gone', async () => {
  const f = fakes({ answers: { ...ANSWERS, pgrep: new Error('pgrep exited with 1') }, alive: false });
  const dir = outputBase({ port: null, javaLog: null });
  const { stdout } = await capture(() => diagnose({ silentForMs: 600_000, enobufs: null, bazelPid: 2181, outputBase: dir, platform: 'linux', ...f }));
  assert.match(stdout, /no output for 600 s/);
  assert.match(stdout, /No ENOBUFS warning came before the silence\./);
  assert.match(stdout, /\(pgrep exited with 1\)/);
  assert.match(stdout, /--- TCP connections between the Bazel client and its server ---\n\(no port in server\/command_port\)\n/);
  assert.match(stdout, /No live Bazel server for /);
  assert.match(stdout, /thread dump/);
  assert.match(stdout, /last 600 lines of java\.log ---\n\(cannot be read\)/);
  // No macOS-only commands on Linux, sample among them, and no jstack or SIGQUIT without a live server.
  assert.deepEqual(f.runs, [
    ['pgrep', '-fl', 'bazel|java'],
    ['netstat', '-an'],
  ]);
  assert.deepEqual(f.signals, [[4242, 0]]);
  assert.deepEqual(f.sleeps, []);
});

test('diagnose without an output base dumps the processes and connections only', async () => {
  const f = fakes();
  const { stdout } = await capture(() => diagnose({ silentForMs: SILENCE, enobufs: null, outputBase: null, platform: 'darwin', ...f }));
  assert.match(stdout, /The output base is unknown, so the server is not dumped\./);
  assert.doesNotMatch(stdout, /Bazel client and its server/);
  assert.deepEqual(f.signals, []);
  assert.match(stdout, /::endgroup::\n$/);
});

test('diagnose samples the bazel process alone when it has no children, and samples nothing without its pid', async () => {
  const alone = fakes({ answers: { ...ANSWERS, children: new Error('pgrep exited with 1') } });
  await capture(() => diagnose({ silentForMs: SILENCE, enobufs: null, bazelPid: 2181, outputBase: null, platform: 'darwin', ...alone }));
  assert.deepEqual(
    alone.runs.filter(([program]) => program === 'sample'),
    [['sample', '2181', '1']],
  );
  const unknown = fakes();
  await capture(() => diagnose({ silentForMs: SILENCE, enobufs: null, outputBase: null, platform: 'darwin', ...unknown }));
  assert.deepEqual(
    unknown.runs.filter(([program, flag]) => program === 'sample' || flag === '-P'),
    [],
  );
});

test('diagnose prints all of jvm.out at its first dump and only what the file gained at each later one', async () => {
  const f = fakes();
  const dir = outputBase({ jvm: 'JVM warning\nFull thread dump 1\n' });
  const jvmOut = path.join(dir, 'server', 'jvm.out');
  const printed = { jvmOut: 0 };
  const dump = async () => sections((await capture(() => diagnose({ silentForMs: SILENCE, enobufs: null, outputBase: dir, printed, platform: 'linux', ...f }))).stdout);

  assert.match((await dump())['server/jvm.out'], /^JVM warning\nFull thread dump 1\n/);
  appendFileSync(jvmOut, 'Exception in thread "grpc-command-0"\nFull thread dump 2\n');
  const second = (await dump())['server/jvm.out since the last dump'];
  assert.match(second, /^Exception in thread "grpc-command-0"\nFull thread dump 2\n/);
  assert.doesNotMatch(second, /JVM warning|dump 1/);
  assert.equal((await dump())['server/jvm.out since the last dump'], '(nothing new)');
  // A new server's shorter jvm.out is printed from its start.
  writeFileSync(jvmOut, 'new server\n');
  assert.match((await dump())['server/jvm.out'], /^new server\n/);
});

test('connections keeps the column header and the live connections to port 443, and counts those in TIME_WAIT', () => {
  const netstat = [
    'Active Internet connections (including servers)',
    'Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)',
    'tcp4  9000      0  10.0.0.1.50000         1.2.3.4.443            ESTABLISHED',
    'tcp4       0      0  10.0.0.1.50001         1.2.3.4.443            TIME_WAIT',
    'tcp4       0      0  10.0.0.1.50002         1.2.3.4.443            TIME_WAIT',
    'tcp4       0      0  10.0.0.1.50003         1.2.3.4.4430           ESTABLISHED',
    'tcp4       0      0  127.0.0.1.9000         127.0.0.1.9001         CLOSE_WAIT',
    'Active Kernel Control sockets',
    'Proto Recv-Q Send-Q   unit     id name',
  ].join('\n');
  assert.equal(
    connections(netstat),
    [
      'Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)',
      'tcp4  9000      0  10.0.0.1.50000         1.2.3.4.443            ESTABLISHED',
      '(2 in TIME_WAIT left out)',
    ].join('\n'),
  );
});

test('connections on the server’s port keeps both ends of a connection and the listener, and no other port', () => {
  const header = 'Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)';
  const live = [
    'tcp4       0      0  127.0.0.1.52345        127.0.0.1.61000        ESTABLISHED',
    'tcp4  130000      0  127.0.0.1.61000        127.0.0.1.52345        ESTABLISHED',
    'tcp4       0      0  127.0.0.1.52345        *.*                    LISTEN',
  ];
  const netstat = [
    header,
    ...live,
    'tcp4       0      0  127.0.0.1.61001        127.0.0.1.52345        TIME_WAIT',
    'tcp4       0      0  127.0.0.1.523456       127.0.0.1.61002        ESTABLISHED',
    'tcp4       0      0  10.0.0.1.50000         1.2.3.4.443            ESTABLISHED',
  ].join('\n');
  assert.equal(connections(netstat, 52345), [header, ...live, '(1 in TIME_WAIT left out)'].join('\n'));
});

test('commandPort reads the server’s port in either address family, and no port from anything else', () => {
  assert.equal(commandPort('127.0.0.1:52345'), 52345);
  assert.equal(commandPort('[::1]:52345\n'), 52345);
  assert.equal(commandPort('127.0.0.1'), null);
  assert.equal(commandPort(''), null);
  assert.equal(commandPort(null), null);
});

test('callGraph keeps sample’s report up to its list of loaded images, or all of it when it has none', () => {
  assert.equal(callGraph('Call graph:\n    1 __recvmsg\n\nBinary Images:\n    0x1 +bazel\n'), 'Call graph:\n    1 __recvmsg');
  assert.equal(callGraph('Call graph:\n    1 __recvmsg\n'), 'Call graph:\n    1 __recvmsg\n');
});

test('jvmOutSince keeps only the last lines of a runaway, which hold the latest thread dump', () => {
  const runaway = Array.from({ length: JVM_OUT_LINES + 10 }, (_, i) => `line ${i}`).join('\n');
  const whole = jvmOutSince(runaway, 0);
  assert.equal(whole.title, `last ${JVM_OUT_LINES} lines of server/jvm.out`);
  assert.equal(whole.text.split('\n').length, JVM_OUT_LINES);
  assert.match(whole.text, new RegExp(`^line 10\n[^]*\nline ${JVM_OUT_LINES + 9}$`));
  assert.equal(whole.end, runaway.length);
  assert.equal(jvmOutSince(runaway, 5).title, `last ${JVM_OUT_LINES} lines of server/jvm.out since the last dump`);
});

test('redact hides a bearer credential in either spelling', () => {
  assert.equal(redact('--remote_header=Authorization=Bearer abc'), '--remote_header=Authorization=Bearer [REDACTED]');
  assert.equal(redact('authorization: bearer abc def'), 'authorization: bearer [REDACTED] def');
});
