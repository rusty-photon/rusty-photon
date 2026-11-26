// One Bazel command, watched for the silence of the macOS wedge: the build
// stops printing mid-build and the step prints nothing until its timeout.
// When the command prints nothing for `silenceMs`, `watch` hands `diagnose`
// what it saw, and `diagnose` prints what the wedge leaves to read while it
// lasts: the Bazel server's threads and logs, the TCP connections to the
// cache and between the Bazel client and its server, the client's stacks
// and the kernel's network buffers. The probe retries nothing and stops
// nothing: the step's timeout ends a wedged build as it would without the
// probe.
// Conventions: docs/skills/ci-logic.md §An action that runs processes.

import { spawn as nodeSpawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { setTimeout as sleepFor } from 'node:timers/promises';

import { command, describe, group, io, withoutRunnerVariables } from '../lib/helpers.mjs';
import { ended, kill } from '../lib/processes.mjs';

/** What Bazel prints when the kernel runs out of socket buffers (ENOBUFS). */
export const ENOBUFS = 'No buffer space available';

/** The most dumps one command gets, so a build that keeps stalling cannot flood the log. */
export const MAX_DUMPS = 3;

/** How many lines of java.log a dump prints. */
export const LOG_LINES = 600;

/** The most lines of jvm.out a dump prints: all it gained since the last dump, short of a runaway. */
export const JVM_OUT_LINES = 10_000;

// Bazel's own log and a JVM thread dump can carry the request headers Bazel
// sent; the runner masks the registered secrets, and this is the second line.
const BEARER = /([Aa]uthorization[=:]\s*[Bb]earer\s+)\S+/g;

/** `text` with any bearer credential replaced. */
export function redact(text) {
  return text.replace(BEARER, '$1[REDACTED]');
}

/** A cancellable repeating timer: `every(ms, fn)` returns its cancel. */
function every(ms, fn) {
  const timer = setInterval(fn, ms);
  return () => clearInterval(timer);
}

/**
 * Runs `bazel` with `args`, never through a shell, as a `run:` step would
 * (withoutRunnerVariables), and copies its output to the step's log as it
 * comes. Each time the output stops for `silenceMs`, and again at every
 * further `silenceMs` it stays silent, it awaits `diagnose({ silentForMs,
 * enobufs, bazelPid })`, where `enobufs` is the last line that named ENOBUFS
 * and how long before the dump it came, or null, and `bazelPid` is the pid of
 * the bazel process it started; at most MAX_DUMPS times in all.
 * When `signal` aborts, the command gets SIGTERM, at its process group on
 * Linux and macOS (lib/processes.mjs's `kill`), and SIGKILL if it is still
 * running `graceMs` later. Resolves to how the command ended, `{ code, signal }`,
 * once any dump in progress is printed; rejects when it cannot start.
 */
export async function watch({ args, silenceMs, diagnose, spawn = nodeSpawn, now = Date.now, repeat = every, signal, graceMs = 5000, env = withoutRunnerVariables(process.env) }) {
  // A group leader on Linux and macOS, so that a stop reaches what Bazel started.
  const child = spawn('bazel', args, { env, stdio: ['ignore', 'pipe', 'pipe'], detached: process.platform !== 'win32' });
  const end = ended(child);
  let last = now();
  let enobufs = null;
  let dumps = 0;
  let silentDumps = 0;
  let dumping = Promise.resolve();
  let busy = false;

  // A line can straddle two chunks, so each stream keeps its unfinished line.
  const tails = { out: '', err: '' };
  const forward = (name, write) => (text) => {
    write(text);
    last = now();
    silentDumps = 0;
    const seen = tails[name] + text;
    const at = seen.lastIndexOf(ENOBUFS);
    if (at >= 0) {
      const start = seen.lastIndexOf('\n', at) + 1;
      const stop = seen.indexOf('\n', at);
      enobufs = { line: seen.slice(start, stop < 0 ? undefined : stop).trim(), at: last };
    }
    tails[name] = seen.slice(seen.lastIndexOf('\n') + 1).slice(-4096);
  };
  child.stdout?.setEncoding('utf8');
  child.stderr?.setEncoding('utf8');
  child.stdout?.on('data', forward('out', io.stdout));
  child.stderr?.on('data', forward('err', io.stderr));

  const check = () => {
    const silentForMs = now() - last;
    if (busy || dumps >= MAX_DUMPS || silentForMs < silenceMs * (silentDumps + 1)) return;
    busy = true;
    dumps += 1;
    silentDumps += 1;
    const seen = enobufs && { line: enobufs.line, agoMs: now() - enobufs.at };
    dumping = Promise.resolve()
      .then(() => diagnose({ silentForMs, enobufs: seen, bazelPid: child.pid }))
      .catch((err) => io.stderr(`bazel-stall-probe: the dump failed: ${describe(err)}\n`))
      .finally(() => {
        busy = false;
      });
  };
  const cancel = repeat(Math.min(15_000, Math.max(1, Math.floor(silenceMs / 4))), check);

  const stop = () => {
    kill(child, 'SIGTERM');
    setTimeout(() => kill(child, 'SIGKILL'), graceMs).unref();
  };
  signal?.addEventListener('abort', stop, { once: true });
  try {
    const result = await end;
    if (result.error) throw result.error;
    await dumping;
    return { code: result.code, signal: result.signal };
  } finally {
    cancel();
    signal?.removeEventListener('abort', stop);
  }
}

/**
 * The TCP connections on `port`, at either end, in `netstat -an`'s output,
 * under its column header: the live ones, whose queues a wedge is read
 * from, and a count of those in TIME_WAIT, which a build's cache hits leave
 * by the hundred.
 */
export function connections(netstat, port = 443) {
  const lines = netstat.split('\n');
  const header = lines.find((line) => /^Proto\s+Recv-Q/.test(line));
  const onPort = new RegExp(`[.:]${port}\\s`);
  const matching = lines.filter((line) => onPort.test(line));
  const waiting = matching.filter((line) => /\bTIME_WAIT\b/.test(line)).length;
  return [header, ...matching.filter((line) => !/\bTIME_WAIT\b/.test(line)), `(${waiting} in TIME_WAIT left out)`].filter((line) => line !== undefined).join('\n');
}

/**
 * The port the Bazel server listens on for its client, from the text of
 * server/command_port, which the server writes as `<address>:<port>`; or
 * null.
 */
export function commandPort(text) {
  const match = /:([0-9]+)\s*$/.exec(text ?? '');
  return match ? Number(match[1]) : null;
}

/** `sample`'s report up to its list of loaded images, which says nothing about where a thread waits. */
export function callGraph(report) {
  const at = report.search(/^Binary Images:/m);
  return at < 0 ? report : report.slice(0, at).trimEnd();
}

/** The text of the file at `file`, or null when it cannot be read. */
function read(file) {
  try {
    return readFileSync(file, 'utf8');
  } catch {
    return null;
  }
}

/** The last `lines` lines of `text`. */
function last(text, lines) {
  return text.split('\n').slice(-lines - 1).join('\n');
}

/**
 * What jvm.out holds past `from`, the length of it that earlier dumps
 * printed, and `end`, the length to remember for the next. On the first
 * dump that is the whole file: what the server printed before any dump,
 * such as the exception a thread died of, and the thread dump SIGQUIT just
 * added. On a later one it is what the file gained since, that dump's own
 * thread dump included, so the dumps print each line once. A file shorter
 * than `from` is a new server's, and is read from its start. Past
 * JVM_OUT_LINES lines only the last that many are kept, which keeps the
 * latest thread dump whole.
 */
export function jvmOutSince(jvmOut, from) {
  const start = jvmOut.length < from ? 0 : from;
  const lines = jvmOut.slice(start).split('\n');
  const where = start === 0 ? 'server/jvm.out' : 'server/jvm.out since the last dump';
  const cut = lines.length > JVM_OUT_LINES;
  return {
    title: cut ? `last ${JVM_OUT_LINES} lines of ${where}` : where,
    text: (cut ? lines.slice(-JVM_OUT_LINES) : lines).join('\n'),
    end: jvmOut.length,
  };
}

/**
 * Prints one dump, in a log group: how long the build has been silent and
 * whether ENOBUFS came first; the Bazel and Java processes; the TCP
 * connections to port 443 (the remote cache, `connections`), whose queues
 * say whether the JVM stopped reading or the network stopped delivering,
 * and, when `outputBase` names the server's port (server/command_port,
 * `commandPort`), those between the Bazel client and its server, which
 * carry the build's output to the client; on macOS the kernel's network
 * buffers (`netstat -m`) and memory (`vm_stat`), and the stacks of the
 * bazel process `bazelPid` and of its children (`sample`, `callGraph`),
 * since bazelisk runs the Bazel client as its child; and, when `outputBase`
 * is known and its server alive, the server's threads (jstack, then
 * SIGQUIT, which makes the JVM print them into server/jvm.out and flush
 * java.log, and works where jstack cannot attach), what jvm.out gained
 * since the last dump (`jvmOutSince`, whose place `printed` keeps from one
 * dump to the next) and java.log's tail.
 * A command that fails is reported and the dump goes on. The server gets no
 * signal but 0 and SIGQUIT.
 */
export async function diagnose({ silentForMs, enobufs, bazelPid, outputBase, printed = { jvmOut: 0 }, platform = process.platform, run = command, kill = process.kill, sleep = sleepFor }) {
  const seconds = Math.round(silentForMs / 1000);
  await group(`bazel-stall-probe: no output for ${seconds} s`, async () => {
    io.stdout(
      enobufs
        ? `ENOBUFS came ${Math.round(enobufs.agoMs / 1000)} s before this dump: ${enobufs.line}\n`
        : 'No ENOBUFS warning came before the silence.\n',
    );
    const show = async (title, program, args, shape = (out) => out) => {
      io.stdout(`--- ${title} ---\n`);
      try {
        const out = await run(program, args);
        io.stdout(`${redact(shape(out))}\n`);
      } catch (err) {
        io.stdout(`(${describe(err)})\n`);
      }
    };
    await show('Bazel and Java processes', 'pgrep', ['-fl', 'bazel|java']);
    await show('TCP connections to port 443 (the remote cache)', 'netstat', ['-an'], connections);
    const port = outputBase ? commandPort(read(path.join(outputBase, 'server', 'command_port'))) : null;
    if (port !== null) {
      await show(`TCP connections to port ${port} (the Bazel client and its server)`, 'netstat', ['-an'], (out) => connections(out, port));
    } else if (outputBase) {
      io.stdout('--- TCP connections between the Bazel client and its server ---\n(no port in server/command_port)\n');
    }
    if (platform === 'darwin') {
      await show('network buffers (netstat -m)', 'netstat', ['-m']);
      await show('memory (vm_stat)', 'vm_stat', []);
      if (bazelPid) {
        // pgrep exits 1 when the process has no children.
        const children = await run('pgrep', ['-P', String(bazelPid)]).then(
          (out) => out.split(/\s+/).filter(Boolean),
          () => [],
        );
        for (const pid of [String(bazelPid), ...children]) await show(`stacks of ${pid} (sample, 1 s)`, 'sample', [pid, '1'], callGraph);
      }
    }
    if (!outputBase) {
      io.stdout('The output base is unknown, so the server is not dumped.\n');
      return;
    }
    const pid = Number(read(path.join(outputBase, 'server', 'server.pid.txt'))?.trim());
    let alive = false;
    try {
      alive = Number.isInteger(pid) && pid > 0 && kill(pid, 0);
    } catch {
      alive = false;
    }
    if (!alive) {
      io.stdout(`No live Bazel server for ${outputBase}.\n`);
    } else {
      await show(`jstack ${pid}`, 'jstack', [String(pid)]);
      io.stdout(`--- SIGQUIT to server ${pid}: thread dump into server/jvm.out ---\n`);
      try {
        kill(pid, 'SIGQUIT');
        await sleep(3000);
      } catch (err) {
        io.stdout(`(${describe(err)})\n`);
      }
    }
    const jvmOut = read(path.join(outputBase, 'server', 'jvm.out'));
    let gained = { title: 'server/jvm.out', text: null };
    if (jvmOut !== null) {
      const since = jvmOutSince(jvmOut, printed.jvmOut);
      printed.jvmOut = since.end;
      gained = { title: since.title, text: since.text === '' ? '(nothing new)' : since.text };
    }
    const javaLog = read(path.join(outputBase, 'java.log'));
    for (const { title, text } of [gained, { title: `last ${LOG_LINES} lines of java.log`, text: javaLog === null ? null : last(javaLog, LOG_LINES) }]) {
      io.stdout(`--- ${title} ---\n`);
      io.stdout(text === null ? '(cannot be read)\n' : `${redact(text)}\n`);
    }
  });
}
