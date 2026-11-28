import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { appendFileSync, mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { afterEach, test } from 'node:test';

import { io } from '../lib/helpers.mjs';
import { capture } from '../lib/testing.mjs';
import {
  callGraph,
  commandPort,
  connections,
  diagnose,
  endpoints,
  ENOBUFS,
  JVM_OUT_LINES,
  jvmOutSince,
  LAST_PACKETS,
  LOG_LINES,
  MAX_DUMPS,
  packets,
  redact,
  tcpCounters,
  trace,
  TRACE_LISTINGS,
  TRACE_MS,
  watch,
} from './probe.mjs';

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
  tcp: 'tcp:\n\t9000 packets sent\n\t\t1 bad reset\n\t72 retransmit timeouts\n\t\t1 connection dropped by rexmit timeout\n',
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
      if (program === 'netstat' && args[0] === '-s') answer = answers.tcp;
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

test('diagnose dumps the processes, the connections to the cache and to the server, the network buffers, the TCP counters, the client’s stacks, the server threads and its logs, in a group', async () => {
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
  assert.equal(dump['TCP retransmissions and drops (netstat -s -p tcp)'], '1 bad reset\n72 retransmit timeouts\n1 connection dropped by rexmit timeout');
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
    ['netstat', '-s', '-p', 'tcp'],
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

// A stand-in for what `trace` returns, without netstat or tcpdump behind it.
function traced({ kept = [], dropped = 0, failure = null } = {}) {
  return { port: 9000, file: '/runner/temp/bazel-stall-probe.pcap', listings: () => ({ kept, dropped }), failure: () => failure };
}

const PACKETS = [
  '2026-11-27 05:16:23.500000 IP6 ::1.9001 > ::1.9000: Flags [S], seq 1, win 65535, length 0',
  '2026-11-27 05:16:23.500100 IP6 ::1.9000 > ::1.9001: Flags [S.], seq 7, ack 2, win 65535, length 0',
  '2026-11-27 05:18:28.100000 IP6 ::1.9000 > ::1.9001: Flags [P.], seq 8:120, ack 2, win 6371, length 112',
  '',
].join('\n');

test('diagnose prints each change the trace kept, with its time, and the packets tcpdump has written', async () => {
  const f = fakes({ answers: { ...ANSWERS, tcpdump: PACKETS } });
  const kept = [
    { at: Date.UTC(2026, 10, 27, 5, 16, 23, 400), listing: 'listener only' },
    { at: Date.UTC(2026, 10, 27, 5, 18, 29, 500), listing: 'client end only' },
  ];
  const { stdout } = await capture(() => diagnose({ silentForMs: SILENCE, enobufs: null, outputBase: null, trace: traced({ kept, dropped: 2 }), platform: 'darwin', ...f }));
  const dump = sections(stdout);
  assert.equal(
    dump[`each change to the TCP connections on port 9000 since before the build (listed every ${TRACE_MS / 1000} s)`],
    '(2 earlier changes left out)\n2026-11-27T05:16:23.400Z\nlistener only\n2026-11-27T05:18:29.500Z\nclient end only',
  );
  assert.equal(dump['packets on lo0 to or from port 9000 (tcpdump)'], packets(PACKETS));
  assert.deepEqual(
    f.runs.filter(([program]) => program === 'tcpdump'),
    [['tcpdump', '-n', '-tttt', '-r', '/runner/temp/bazel-stall-probe.pcap']],
  );
});

test('diagnose prints how tcpdump ended instead of reading its file, and nothing of a trace there is not', async () => {
  const ended = fakes();
  const { stdout } = await capture(() =>
    diagnose({ silentForMs: SILENCE, enobufs: null, outputBase: null, trace: traced({ failure: 'tcpdump exited with 1: sudo: a password is required' }), platform: 'darwin', ...ended }),
  );
  assert.equal(sections(stdout)['packets on lo0 to or from port 9000 (tcpdump)'], '(tcpdump exited with 1: sudo: a password is required)');
  assert.deepEqual(
    ended.runs.filter(([program]) => program === 'tcpdump'),
    [],
  );
  const none = fakes();
  const without = await capture(() => diagnose({ silentForMs: SILENCE, enobufs: null, outputBase: null, platform: 'linux', ...none }));
  assert.doesNotMatch(without.stdout, /each change to|tcpdump/);
});

// --- trace ----------------------------------------------------------------------------

const HEADER = 'Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)';
const LISTENER = 'tcp6       0      0  ::1.9000               *.*                    LISTEN';
const serverEnd = (queue) => `tcp6  ${queue}      0  ::1.9000               ::1.9001               ESTABLISHED`;
const clientEnd = (queue) => `tcp6       0 ${queue}  ::1.9001               ::1.9000               ESTABLISHED`;
const netstat = (...lines) => ['Active Internet connections', HEADER, ...lines, 'tcp4       0      0  10.0.0.1.50000         1.2.3.4.443            ESTABLISHED'].join('\n');

// Starts `trace` on an output base whose server listens on [::1]:9000, with
// `node -e FAKE`, the fake Bazel's program, in tcpdump's place, netstat
// answering from `listings` in turn (the listener alone once they run out),
// and a clock the test moves and a listing it runs. Records every program
// run; the stop's `sudo kill` sends the fake SIGTERM. The stop lets go of the
// fake, so no test waits for it to exit after one.
async function tracing({ listings = [], sayOnStart = null } = {}) {
  const runs = [];
  const spawns = [];
  let child;
  let ready;
  const isReady = new Promise((resolve) => {
    ready = resolve;
  });
  const fakeSpawn = (program, args, opts) => {
    spawns.push({ program, args, opts });
    child = spawn(process.execPath, ['-e', FAKE], { ...opts, stdio: [...opts.stdio, 'ipc'] });
    LIVE.add(child);
    child.on('close', () => LIVE.delete(child));
    child.on('message', () => ready());
    return child;
  };
  const queue = [...listings];
  const clock = { t: 0, list: null, period: null, cancelled: false };
  const run = async (program, args) => {
    runs.push([program, ...args]);
    if (program === 'sudo') {
      process.kill(Number(args.at(-1)), 'SIGTERM');
      return '';
    }
    const answer = queue.length > 0 ? queue.shift() : netstat(LISTENER);
    if (answer instanceof Error) throw answer;
    return answer;
  };
  const started = trace({
    outputBase: outputBase({ port: '[::1]:9000' }),
    file: '/runner/temp/bazel-stall-probe.pcap',
    platform: 'darwin',
    run,
    spawn: fakeSpawn,
    now: () => clock.t,
    repeat: (ms, fn) => {
      clock.list = fn;
      clock.period = ms;
      return () => {
        clock.cancelled = true;
      };
    },
  });
  await isReady;
  const exited = new Promise((resolve) => child.on('close', resolve));
  if (sayOnStart) child.send(sayOnStart);
  // Moves the clock on by TRACE_MS and runs one listing to its end.
  const tick = async () => {
    clock.t += TRACE_MS;
    await clock.list();
  };
  return { traced: started, runs, spawns, clock, tick, exited, pid: child.pid };
}

// Bounded so that a stop that never reaches the fake tcpdump fails by this test's name.
test('trace keeps each listing whose connections or states change, with its time, not one whose queues alone move, and stops tcpdump through sudo', { timeout: 30_000 }, async () => {
  const listings = [
    netstat(LISTENER),
    netstat(serverEnd(0), clientEnd(0), LISTENER),
    netstat(serverEnd(130000), clientEnd(52), LISTENER),
    netstat(clientEnd(0), LISTENER),
  ];
  // The first listing runs as the trace starts, at 0 ms.
  const t = await tracing({ listings });
  assert.equal(t.clock.period, TRACE_MS);
  assert.equal(t.traced.port, 9000);
  assert.equal(t.traced.file, '/runner/temp/bazel-stall-probe.pcap');
  assert.deepEqual(
    t.spawns.map(({ program, args }) => [program, ...args]),
    [['sudo', '-n', 'tcpdump', '-i', 'lo0', '--immediate-mode', '-U', '-s', '128', '-w', '/runner/temp/bazel-stall-probe.pcap', 'tcp', 'port', '9000']],
  );
  assert.equal(t.spawns[0].opts.stdio[0], 'ignore');
  assert.equal(t.spawns[0].opts.shell, undefined);
  await t.tick();
  await t.tick();
  await t.tick();
  assert.deepEqual(t.traced.listings(), {
    kept: [
      { at: 0, listing: connections(listings[0], 9000) },
      { at: TRACE_MS, listing: connections(listings[1], 9000) },
      { at: 3 * TRACE_MS, listing: connections(listings[3], 9000) },
    ],
    dropped: 0,
  });
  assert.equal(t.traced.failure(), null);
  await t.traced.stop();
  assert.deepEqual(t.runs.at(-1), ['sudo', '-n', 'kill', '-TERM', String(t.pid)]);
  assert.equal(t.clock.cancelled, true);
});

test('trace keeps the latest listings, counts those it dropped, and keeps a netstat that fails once', { timeout: 30_000 }, async () => {
  const changing = Array.from({ length: TRACE_LISTINGS + 5 }, (_, i) => netstat(i % 2 ? clientEnd(0) : LISTENER));
  const failed = new Error('netstat exited with 1');
  const t = await tracing({ listings: [...changing, failed, failed] });
  for (let i = 0; i < TRACE_LISTINGS + 6; i += 1) await t.tick();
  const { kept, dropped } = t.traced.listings();
  assert.equal(kept.length, TRACE_LISTINGS);
  assert.equal(dropped, 6);
  assert.deepEqual(kept.at(-1), { at: (TRACE_LISTINGS + 5) * TRACE_MS, listing: '(netstat exited with 1)' });
  assert.equal(kept.at(-2).at, (TRACE_LISTINGS + 4) * TRACE_MS);
  assert.equal(kept[0].at, 6 * TRACE_MS);
  await t.traced.stop();
});

test('trace says how tcpdump ended when it ends early, and sends it no stop then', { timeout: 30_000 }, async () => {
  const t = await tracing({ sayOnStart: { err: 'tcpdump: listening on lo0\nsudo: a password is required\n', exit: 1 } });
  await t.exited;
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(t.traced.failure(), 'tcpdump exited with 1: sudo: a password is required');
  await t.traced.stop();
  assert.deepEqual(
    t.runs.filter(([program]) => program === 'sudo'),
    [],
  );
});

test('trace watches nothing off macOS, without an output base, or before the server has written its port', () => {
  const never = () => {
    throw new Error('nothing may run');
  };
  const options = { file: '/runner/temp/bazel-stall-probe.pcap', run: never, spawn: never, repeat: never };
  assert.equal(trace({ ...options, outputBase: outputBase(), platform: 'linux' }), null);
  assert.equal(trace({ ...options, outputBase: null, platform: 'darwin' }), null);
  assert.equal(trace({ ...options, outputBase: outputBase({ port: null }), platform: 'darwin' }), null);
});

test('endpoints drops the column header and the queues, and keeps the rest of each line', () => {
  const listing = [HEADER, serverEnd(130000), clientEnd(52), LISTENER, '(2 in TIME_WAIT left out)'].join('\n');
  assert.equal(
    endpoints(listing),
    ['tcp6 ::1.9000 ::1.9001 ESTABLISHED', 'tcp6 ::1.9001 ::1.9000 ESTABLISHED', 'tcp6 ::1.9000 *.* LISTEN', '(2 in TIME_WAIT left out)'].join('\n'),
  );
  assert.equal(endpoints('(netstat exited with 1)'), '(netstat exited with 1)');
});

test('packets keeps every SYN, FIN and reset and the last packets, in order, under the count', () => {
  const packet = (i, flags) => `2026-11-27 05:18:00.${String(i).padStart(6, '0')} IP6 ::1.9000 > ::1.9001: Flags [${flags}], seq ${i}, length 1`;
  const flags = (i) => ({ 0: 'S', 1: 'S.', 10: 'F.', 11: 'FP.', 20: 'R', 21: 'R.' })[i] ?? (i % 2 ? 'P.' : '.');
  const total = LAST_PACKETS + 50;
  const lines = Array.from({ length: total }, (_, i) => packet(i, flags(i)));
  const shown = packets(`${lines.join('\n')}\n`).split('\n');
  assert.equal(shown[0], `(${total} packets: each SYN, FIN and reset, and the last ${LAST_PACKETS})`);
  assert.deepEqual(shown.slice(1), [0, 1, 10, 11, 20, 21].map((i) => lines[i]).concat(lines.slice(-LAST_PACKETS)));
  assert.equal(packets(''), `(0 packets: each SYN, FIN and reset, and the last ${LAST_PACKETS})`);
});

test('tcpCounters keeps the counts of the wedge’s chain, trimmed and in order, and no other line', () => {
  const stats = [
    'tcp:',
    '\t12345 packets sent',
    '\t\t2 challenge ACKs sent due to unexpected SYN',
    '\t\t1 challenge ACK sent due to unexpected RST',
    '\t\t0 challenge ACKs sent due to unexpected RST',
    '\t\t1169 received packets dropped due to low memory',
    '\t\t1 bad reset',
    '\t72 retransmit timeouts',
    '\t\t1 connection dropped by rexmit timeout',
    '\t\t3 times retransmit timeout triggered after probe',
    '\t0 persist timeouts',
    '',
  ].join('\n');
  assert.equal(
    tcpCounters(stats),
    [
      '1169 received packets dropped due to low memory',
      '1 bad reset',
      '72 retransmit timeouts',
      '1 connection dropped by rexmit timeout',
    ].join('\n'),
  );
  assert.equal(tcpCounters(''), '');
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
