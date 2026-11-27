// One Bazel command, watched for the silence of the macOS wedge: the build
// stops printing mid-build and the step prints nothing until its timeout.
// When the command prints nothing for `silenceMs`, `watch` hands `diagnose`
// what it saw, and `diagnose` prints what the wedge leaves to read while it
// lasts: the Bazel server's threads and logs, the TCP connections to the
// cache and between the Bazel client and its server, the client's stacks
// and the kernel's network buffers. On macOS `trace` also watches the
// connection between the client and its server from before the build, so a
// dump can say when the server's end of it went and which packets crossed
// it. The probe retries nothing and stops nothing but its own tracing: the
// step's timeout ends a wedged build as it would without the probe.
// Conventions: docs/skills/ci-logic.md §An action that runs processes.

import { spawn as nodeSpawn } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { setTimeout as sleepFor } from 'node:timers/promises';

import { command, describe, group, io, withoutRunnerVariables } from '../lib/helpers.mjs';
import { ended, exitText, kill } from '../lib/processes.mjs';

/** What Bazel prints when the kernel runs out of socket buffers (ENOBUFS). */
export const ENOBUFS = 'No buffer space available';

/** The most dumps one command gets, so a build that keeps stalling cannot flood the log. */
export const MAX_DUMPS = 3;

/** How many lines of java.log a dump prints. */
export const LOG_LINES = 600;

/** The most lines of jvm.out a dump prints: all it gained since the last dump, short of a runaway. */
export const JVM_OUT_LINES = 10_000;

/** How often `trace` lists the TCP connections between the Bazel client and its server. */
export const TRACE_MS = 1000;

/** The most listings `trace` keeps, the latest, so connections that keep changing cannot flood a dump. */
export const TRACE_LISTINGS = 100;

/** How many of the last packets a dump prints, beside every SYN, FIN and reset. */
export const LAST_PACKETS = 100;

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

/** The Bazel server's port, from server/command_port in `outputBase` (`commandPort`), or null. */
function serverPort(outputBase) {
  return commandPort(read(path.join(outputBase, 'server', 'command_port')));
}

/**
 * A `connections` listing without its column header and its queues: the
 * connections and their states, which `trace` compares from one listing to
 * the next. The queues move with every write.
 */
export function endpoints(listing) {
  return listing
    .split('\n')
    .filter((line) => !/^Proto\s+Recv-Q/.test(line))
    .map((line) => {
      const fields = line.trim().split(/\s+/);
      return /^tcp/.test(fields[0]) ? [fields[0], ...fields.slice(3)].join(' ') : line;
    })
    .join('\n');
}

/**
 * Watches the TCP connections on the Bazel server's port, read from
 * `outputBase`, so that a dump can say when the server's end of the
 * client's connection went and which packets crossed it. Returns null off
 * macOS, since only macOS wedges, and before the server has written its
 * port.
 *
 * It lists the connections (`connections`) every TRACE_MS and keeps, with
 * its time, each listing whose connections or states (`endpoints`) differ
 * from the one before, the latest TRACE_LISTINGS of them. It also starts
 * tcpdump on lo0 through sudo without a password (`-n`), which a hosted
 * runner's user may run. tcpdump takes each packet to or from the port as it
 * arrives (`--immediate-mode`, where macOS would otherwise hand them over in
 * batches) and writes its headers (`-s 128`) into `file` at once (`-U`), so a
 * dump reads every packet up to its own moment. It starts as the build does,
 * so it may miss the build's connect; what a wedge leaves to read is how the
 * connection ends.
 *
 * Returns `{ port, file, listings, failure, stop }`. `listings()` is
 * `{ kept, dropped }`, oldest first. `failure()` says how tcpdump ended, once
 * it has, or is null. `stop()` ends the listing and sends tcpdump SIGTERM
 * through sudo, since it runs as root, and resolves once that is sent.
 */
export function trace({ outputBase, file, platform = process.platform, run = command, spawn = nodeSpawn, now = Date.now, repeat = every }) {
  const port = platform === 'darwin' && outputBase ? serverPort(outputBase) : null;
  if (port === null) return null;

  const kept = [];
  let dropped = 0;
  let previous = null;
  let listing = false;
  const list = async () => {
    // A netstat slower than TRACE_MS is not run twice at once.
    if (listing) return;
    listing = true;
    let text;
    try {
      text = connections(await run('netstat', ['-an']), port);
    } catch (err) {
      text = `(${describe(err)})`;
    }
    listing = false;
    const key = endpoints(text);
    if (key === previous) return;
    previous = key;
    kept.push({ at: now(), listing: text });
    if (kept.length > TRACE_LISTINGS) {
      kept.shift();
      dropped += 1;
    }
  };
  const cancel = repeat(TRACE_MS, list);
  list();

  const tcpdump = spawn('sudo', ['-n', 'tcpdump', '-i', 'lo0', '--immediate-mode', '-U', '-s', '128', '-w', file, 'tcp', 'port', String(port)], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let said = '';
  tcpdump.stderr?.setEncoding('utf8');
  tcpdump.stderr?.on('data', (text) => {
    said = (said + text).slice(-4096);
  });
  let end = null;
  ended(tcpdump).then((result) => {
    end = result;
  });

  return {
    port,
    file,
    listings: () => ({ kept: [...kept], dropped }),
    failure: () => {
      if (end === null) return null;
      const why = said.trim().split('\n').at(-1);
      return `tcpdump ${exitText(end)}${why ? `: ${why}` : ''}`;
    },
    stop: async () => {
      cancel();
      if (end === null && tcpdump.pid !== undefined) {
        // sudo relays the SIGTERM it gets to tcpdump (sudo(8), Signal handling).
        await run('sudo', ['-n', 'kill', '-TERM', String(tcpdump.pid)]).catch((err) =>
          io.stderr(`bazel-stall-probe: tcpdump could not be stopped: ${describe(err)}\n`),
        );
      }
      // Neither holds Node, and so the step, open.
      tcpdump.stderr?.destroy();
      tcpdump.unref();
    },
  };
}

/**
 * From what `tcpdump -r` prints, one line a packet: every packet that opens,
 * closes or resets a connection (its flags hold S, F or R) and the last
 * LAST_PACKETS, in order, under how many there are in all.
 */
export function packets(text) {
  const lines = text.split('\n').filter((line) => line.trim() !== '');
  const from = lines.length - LAST_PACKETS;
  const kept = lines.filter((line, i) => i >= from || /Flags \[[^\]]*[SFR]/.test(line));
  return [`(${lines.length} packets: each SYN, FIN and reset, and the last ${LAST_PACKETS})`, ...kept].join('\n');
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
 * carry the build's output to the client; when `trace`, what the function
 * `trace` returned, watches that port, each change it kept, with its time,
 * and the packets tcpdump has written (`packets`), or how tcpdump ended; on
 * macOS the kernel's network buffers (`netstat -m`) and memory (`vm_stat`),
 * and the stacks of the bazel process `bazelPid` and of its children
 * (`sample`, `callGraph`), since bazelisk runs the Bazel client as its
 * child; and, when `outputBase`
 * is known and its server alive, the server's threads (jstack, then
 * SIGQUIT, which makes the JVM print them into server/jvm.out and flush
 * java.log, and works where jstack cannot attach), what jvm.out gained
 * since the last dump (`jvmOutSince`, whose place `printed` keeps from one
 * dump to the next) and java.log's tail.
 * A command that fails is reported and the dump goes on. The server gets no
 * signal but 0 and SIGQUIT.
 */
export async function diagnose({ silentForMs, enobufs, bazelPid, outputBase, printed = { jvmOut: 0 }, trace = null, platform = process.platform, run = command, kill = process.kill, sleep = sleepFor }) {
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
    const port = outputBase ? serverPort(outputBase) : null;
    if (port !== null) {
      await show(`TCP connections to port ${port} (the Bazel client and its server)`, 'netstat', ['-an'], (out) => connections(out, port));
    } else if (outputBase) {
      io.stdout('--- TCP connections between the Bazel client and its server ---\n(no port in server/command_port)\n');
    }
    if (trace) {
      const { kept, dropped } = trace.listings();
      io.stdout(`--- each change to the TCP connections on port ${trace.port} since before the build (listed every ${TRACE_MS / 1000} s) ---\n`);
      if (dropped > 0) io.stdout(`(${dropped} earlier changes left out)\n`);
      for (const { at, listing } of kept) io.stdout(`${new Date(at).toISOString()}\n${listing}\n`);
      const failure = trace.failure();
      const title = `packets on lo0 to or from port ${trace.port} (tcpdump)`;
      if (failure === null) await show(title, 'tcpdump', ['-n', '-tttt', '-r', trace.file], packets);
      else io.stdout(`--- ${title} ---\n(${failure})\n`);
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
