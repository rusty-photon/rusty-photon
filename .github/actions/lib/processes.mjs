// Programs run at once, for the actions that run processes (bdd-sharded,
// conformu-run): streams of runs, all started together, each stream's runs
// one after another and its output in a log file of its own, printed
// afterwards as a log group, so that the streams' lines do not interleave.
// How a program ended (`ended`, `exitText`) serves beta-clippy too.
// Conventions: docs/skills/ci-logic.md §An action that runs processes.

import { spawn as nodeSpawn } from 'node:child_process';
import { closeSync, openSync, readFileSync, writeSync } from 'node:fs';
import path from 'node:path';

import { describe, group, io } from './helpers.mjs';

/**
 * How `child` ended, as `{ code, signal, error }`: its exit code or signal,
 * and the error it could not start with, after which Node still emits
 * 'close'.
 */
export function ended(child) {
  return new Promise((resolve) => {
    let error;
    child.on('error', (err) => {
      error ??= err;
    });
    child.on('close', (code, signal) => resolve({ code, signal, error }));
  });
}

/** What `ended` resolved to, as the end of a sentence about the program. */
export function exitText({ code, signal, error }) {
  if (error) return `did not start: ${describe(error)}`;
  return signal ? `was stopped by ${signal}` : `exited with ${code}`;
}

// On Linux and macOS each program leads a process group of its own, and a
// stop goes to the group: the processes a program starts outlive a signal
// sent to it alone, as `cargo test`'s test binary outlives cargo's SIGTERM.
// A process that starts a session of its own, as bdd-infra starts ConformU,
// leaves the group, and the runner's cleanup at the job's end stops it. On
// Windows, which has no group a signal reaches, the program is killed.
const GROUPS = process.platform !== 'win32';

function kill(child, signal) {
  // A program that never started has no pid. One that has ended may have
  // given its pid to another process: Node sets exitCode or signalCode as it
  // reaps it, before any other JavaScript runs.
  if (child.pid === undefined || child.exitCode !== null || child.signalCode !== null) return;
  try {
    if (GROUPS) process.kill(-child.pid, signal);
    else child.kill(signal);
  } catch {
    // The group may have no process left to signal. A throw here would end
    // Node before the logs are printed.
  }
}

function logFile(logs, name) {
  return path.join(logs, `${name}.log`);
}

/**
 * Runs `planned` streams, `{ name, runs }`, at once, the runs of each one,
 * `{ program, args, env }`, after one another, never through a shell and with
 * no stdin, every run's output going to its stream's log in `logs`. A stream
 * runs all its runs, even after one fails. Resolves to `{ failed, stopped }`:
 * the streams that failed, by name, and whether `signal` stopped them. Then
 * no further run starts, each running one is sent SIGTERM, and one still
 * running `graceMs` later SIGKILL.
 */
export async function runStreams(planned, { logs, spawn = nodeSpawn, signal, graceMs = 5000 }) {
  // Every log opens before the first program starts, so a log that cannot
  // open fails the step with nothing left running.
  const outs = [];
  try {
    for (const { name } of planned) outs.push(openSync(logFile(logs, name), 'w'));
  } catch (err) {
    outs.forEach((out) => closeSync(out));
    throw err;
  }
  // Stopped by `signal`, or by a stream that threw.
  const halt = new AbortController();
  const running = new Set();
  halt.signal.addEventListener('abort', () => {
    for (const child of running) kill(child, 'SIGTERM');
    setTimeout(() => {
      for (const child of running) kill(child, 'SIGKILL');
    }, graceMs).unref();
  });
  const stop = () => halt.abort();
  if (signal?.aborted) stop();
  signal?.addEventListener('abort', stop, { once: true });

  const runStream = async ({ runs }, out) => {
    let ok = true;
    for (const { program, args, env } of runs) {
      if (halt.signal.aborted) return false;
      // stdin is not the step's: a program that reads it would take it from
      // the runs after it.
      const child = spawn(program, args, { env, stdio: ['ignore', out, out], detached: GROUPS });
      running.add(child);
      const result = await ended(child);
      running.delete(child);
      if (result.error) writeSync(out, `${program} ${exitText(result)}\n`);
      if (result.error || result.code !== 0) ok = false;
    }
    return ok;
  };

  try {
    // Settled, not raced: a stream that threw stops the others, and the step
    // goes on only once every program it started has ended.
    const settled = await Promise.allSettled(
      planned.map((stream, i) =>
        runStream(stream, outs[i]).catch((err) => {
          stop();
          throw err;
        }),
      ),
    );
    const thrown = settled.find((s) => s.status === 'rejected');
    if (thrown) throw thrown.reason;
    return {
      failed: planned.filter((_, i) => !settled[i].value).map(({ name }) => name),
      stopped: signal?.aborted ?? false,
    };
  } finally {
    signal?.removeEventListener('abort', stop);
    outs.forEach((out) => closeSync(out));
  }
}

/** Prints each stream's log in `logs` as a log group of its own, titled `title(name)`. */
export async function replay(planned, logs, title) {
  for (const { name } of planned) {
    await group(title(name), () => {
      let text = '';
      try {
        text = readFileSync(logFile(logs, name), 'utf8');
      } catch {
        // a stream whose log never opened printed nothing
      }
      io.stdout(text.endsWith('\n') || text === '' ? text : `${text}\n`);
    });
  }
}
