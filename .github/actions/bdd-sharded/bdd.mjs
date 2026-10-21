// The workspace's BDD suites as parallel processes, for the Cargo safety net's
// BDD step (test.yml, scheduled.yml): one build of every `bdd` target, then
// each binary run directly, the suites that can be sharded split across
// processes.
//
// cargo runs test binaries one after another, and cucumber serializes every
// scenario of a `@serial` feature, so one process spends most of the suite's
// wall clock with a single scenario in flight. Every BDD process starts its
// own OmniSim (`--multi-instance`, a port of its own, a settings directory of
// its own), so processes are the lever. `bdd_infra::sharding` partitions a
// suite's scenarios by TEST_SHARD_INDEX and TEST_TOTAL_SHARDS, Bazel's names
// but nothing Bazel needs, and `@serial` still holds within a shard, which is
// the scope it protects: one process's simulator. Only a suite that routes its
// cucumber filter through `scenario_in_current_shard` reads them; any other
// would run in full in every shard, so they are set only on the processes of
// the suites in SHARDED. Conventions: docs/skills/ci-logic.md.

import { spawn as nodeSpawn } from 'node:child_process';
import { closeSync, mkdirSync, openSync, readFileSync } from 'node:fs';
import path from 'node:path';

import { fail, io } from '../lib/helpers.mjs';
import { ended, exitText, replay, runStreams } from '../lib/processes.mjs';

/** The suites split across processes, each by an input of its own. */
export const SHARDED = ['rp', 'session-runner'];

// polar-align can be sharded but is small, and filemonitor is the largest
// suite that cannot be: together about one rp shard's work, so they share a
// stream. Every other suite runs in one more, about five minutes in all.
export const HEAVY = ['polar-align', 'filemonitor'];

/**
 * Builds every bdd target once. Running the binaries it built needs no cargo
 * process, where `cargo test -p <package>` per stream would be a trap twice
 * over: one package's `--all-features` unifies features differently from the
 * workspace's, so every stream would rebuild dozens of crates, and cargo holds
 * the build directory's lock while it builds, so the streams would wait on
 * each other. `bdd_main!` changes to BDD_PACKAGE_DIR, as cargo would.
 */
export const BUILD = ['test', '--locked', '--workspace', '--all-features', '--test', 'bdd', '--no-run', '--message-format=json'];

/** `value`, the input `name`, as a number of shards: a whole number of 1 or more. */
export function shardCount(name, value) {
  if (!/^[0-9]+$/.test(value) || Number(value) < 1) {
    throw new Error(`${name} must be a whole number of 1 or more; got ${JSON.stringify(value)}`);
  }
  return Number(value);
}

// cargo's JSON messages, one object per line.
function messages(text) {
  const lines = text.split('\n');
  const parsed = [];
  for (const [i, line] of lines.entries()) {
    if (line.trim() === '') continue;
    try {
      parsed.push(JSON.parse(line));
    } catch {
      throw new Error(`line ${i + 1} of cargo's output is not a JSON message: ${line.slice(0, 200)}`);
    }
  }
  return parsed;
}

/**
 * The BDD binaries cargo's JSON messages `text` say it built, as `{ name,
 * dir, exe }`: `dir` is the package's directory and `name` that directory's,
 * which is every suite's package name.
 */
export function suites(text) {
  return messages(text)
    .filter((m) => m.reason === 'compiler-artifact' && m.target?.name === 'bdd' && m.executable)
    .map((m) => {
      const dir = m.manifest_path.replace(/[\\/]Cargo\.toml$/, '');
      return { name: dir.split(/[\\/]/).pop(), dir, exe: m.executable };
    });
}

/**
 * The streams that run `list`, the suites `suites` found, as `{ name, runs }`:
 * each suite in SHARDED split into the number of shards `counts` gives it, a
 * stream per shard (a count of 1 runs the suite whole), HEAVY in one stream
 * and every other suite in one more. A run is a suite, and for a shard
 * `shard: { index, total }`. Throws when a suite in SHARDED is missing, or
 * when two binaries have its name, since only one could be the suite.
 */
export function streams(list, counts) {
  const planned = [];
  for (const name of SHARDED) {
    const found = list.filter((suite) => suite.name === name);
    if (found.length === 0) throw new Error(`no bdd binary for ${name}`);
    if (found.length > 1) throw new Error(`two bdd binaries are named ${name}: in ${found[0].dir} and in ${found[1].dir}`);
    const [suite] = found;
    const total = counts[name];
    if (total === 1) {
      planned.push({ name, runs: [suite] });
      continue;
    }
    for (let index = 0; index < total; index++) {
      planned.push({ name: `${name}-${index}`, runs: [{ ...suite, shard: { index, total } }] });
    }
  }
  const heavy = list.filter((suite) => HEAVY.includes(suite.name));
  const rest = list.filter((suite) => !SHARDED.includes(suite.name) && !HEAVY.includes(suite.name));
  if (heavy.length > 0) planned.push({ name: 'heavy', runs: heavy });
  if (rest.length > 0) planned.push({ name: 'rest', runs: rest });
  return planned;
}

/**
 * The environment the build and every suite start from: `env`, with RUSTFLAGS
 * set to `rustflags` unless that is empty, and without a shard of its own. A
 * TEST_SHARD_INDEX and TEST_TOTAL_SHARDS the job had would narrow every run
 * that sets none (the HEAVY and other streams, and a count of 1) to one slice
 * of its scenarios, and nothing would notice.
 */
export function baseEnv(env, rustflags) {
  const base = { ...env };
  delete base.TEST_SHARD_INDEX;
  delete base.TEST_TOTAL_SHARDS;
  if (rustflags !== '') base.RUSTFLAGS = rustflags;
  return base;
}

// A run of a stream `streams` planned, `{ name, dir, exe, shard }`, as the
// program runStreams starts: the suite's binary, in an environment of its own.
function suiteRun(base, run) {
  const env = { ...base, BDD_PACKAGE_DIR: run.dir };
  if (run.shard) {
    env.TEST_SHARD_INDEX = String(run.shard.index);
    env.TEST_TOTAL_SHARDS = String(run.shard.total);
  }
  return { program: run.exe, args: [], env };
}

// The compiler's messages in cargo's JSON `text`, as it would have printed
// them, or `text` itself when it does not parse.
function rendered(text) {
  try {
    return messages(text)
      .filter((m) => m.reason === 'compiler-message')
      .map((m) => m.message?.rendered ?? '')
      .join('');
  } catch {
    return text;
  }
}

/**
 * Builds every bdd target, keeping cargo's JSON messages in `file`, and
 * resolves to the suites it built. cargo's progress goes to the job's log as
 * it builds. A failed build prints the compiler's messages, which are in the
 * JSON, and throws. `signal` stops cargo.
 */
export async function build({ env, file, spawn = nodeSpawn, signal }) {
  const out = openSync(file, 'w');
  let result;
  try {
    result = await ended(spawn('cargo', BUILD, { env, stdio: ['ignore', out, 'inherit'], signal }));
  } finally {
    closeSync(out);
  }
  if (signal?.aborted) throw new Error('the bdd build was stopped');
  const text = readFileSync(file, 'utf8');
  if (result.error || result.code !== 0) {
    io.stdout(rendered(text));
    throw new Error(`the bdd build failed: cargo ${exitText(result)}`);
  }
  return suites(text);
}

/**
 * The action: checks the inputs, builds every bdd target, runs the streams,
 * and prints each stream's log however the run ends, a stopped one included,
 * since a stream that hangs to the job's timeout would otherwise show
 * nothing. Fails the step for each stream that failed, by name, or once when
 * `signal` stopped them. Its files go in `tmp`.
 */
export async function main({ rpShards, sessionRunnerShards, rustflags, env = process.env, tmp, spawn, signal, graceMs }) {
  const counts = {
    rp: shardCount('rp-shards', rpShards),
    'session-runner': shardCount('session-runner-shards', sessionRunnerShards),
  };
  const base = baseEnv(env, rustflags);
  const logs = path.join(tmp, 'bdd-shard-logs');
  mkdirSync(logs, { recursive: true });

  const built = await build({ env: base, file: path.join(tmp, 'bdd-build.json'), spawn, signal });
  if (built.length === 0) throw new Error('cargo built no bdd binaries');
  const planned = streams(built, counts);
  io.stdout(`built ${built.length} bdd binaries\n`);

  const programs = planned.map(({ name, runs }) => ({ name, runs: runs.map((run) => suiteRun(base, run)) }));
  let result;
  try {
    result = await runStreams(programs, { logs, spawn, signal, graceMs });
  } finally {
    await replay(programs, logs, (name) => `bdd ${name}`);
  }
  if (result.stopped) {
    fail('the BDD streams were stopped before they finished');
    return;
  }
  for (const name of result.failed) fail(`BDD stream ${name} failed`);
}
