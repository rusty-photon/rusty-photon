// Shared helpers for the CI's local JavaScript actions (ADR-022): the parts of
// @actions/core and @actions/http-client the actions need, written on Node's
// built-ins so no action needs npm, a package.json or a build step. An action
// imports this file by a relative path:
//
//   import { getInput, run, setOutput } from '../lib/helpers.mjs';
//
// The command formats follow the runner's parsers in actions/runner:
// src/Runner.Common/ActionCommand.cs for the `::name::data` commands on
// stdout, src/Runner.Worker/FileCommandManager.cs for the files that
// GITHUB_OUTPUT, GITHUB_ENV and GITHUB_PATH name. Conventions:
// docs/skills/ci-logic.md.

import { createHash, randomUUID } from 'node:crypto';
import { appendFileSync } from 'node:fs';
import { EOL } from 'node:os';
import { delimiter as PATH_DELIMITER } from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { inspect } from 'node:util';

// --- Inputs -------------------------------------------------------------------

// The runner passes input `name` in INPUT_<NAME>: upper-cased, spaces turned
// into underscores, hyphens kept.
function inputVariable(name) {
  return `INPUT_${name.replace(/ /g, '_').toUpperCase()}`;
}

/**
 * The action's input `name`, trimmed. An input the workflow did not set and
 * action.yml gives no default reads as ''; `required` makes that an error.
 */
export function getInput(name, { required = false } = {}) {
  const value = (process.env[inputVariable(name)] ?? '').trim();
  if (required && value === '') {
    throw new Error(`Input required and not supplied: ${name}`);
  }
  return value;
}

const TRUE_VALUES = ['true', 'True', 'TRUE'];
const FALSE_VALUES = ['false', 'False', 'FALSE'];

/**
 * A boolean input, accepting what YAML 1.2's core schema does (as
 * @actions/core's getBooleanInput). Anything else throws, empty included, so
 * a typo fails the step instead of reading as false: give the input a
 * default in action.yml.
 */
export function getBooleanInput(name) {
  const value = getInput(name);
  if (TRUE_VALUES.includes(value)) return true;
  if (FALSE_VALUES.includes(value)) return false;
  throw new TypeError(
    `Input ${name} must be one of ${[...TRUE_VALUES, ...FALSE_VALUES].join(', ')}; got ${JSON.stringify(value)}`,
  );
}

// --- Workflow commands on stdout ----------------------------------------------

/**
 * Where the helpers write. The runner reads workflow commands from stdout;
 * tests swap these out to read them back, since under `node --test` stdout
 * also carries the test runner's own reports.
 */
export const io = {
  stdout: (text) => process.stdout.write(text),
  stderr: (text) => process.stderr.write(text),
};

// The runner unescapes these three in a command's data, and reads a command
// only at the start of a line, so an unescaped line break would end it early.
function escapeData(text) {
  return String(text).replace(/%/g, '%25').replace(/\r/g, '%0D').replace(/\n/g, '%0A');
}

function command(name, data) {
  io.stdout(`::${name}::${escapeData(data)}\n`);
}

/**
 * Masks `secret` in every later line of the job's log. Call it before
 * anything could print the value. The runner masks each line of a multi-line
 * value on its own.
 */
export function setSecret(secret) {
  command('add-mask', secret);
}

/** A warning annotation on the run, and a line in the log. */
export function warning(message) {
  command('warning', message);
}

/** Runs `fn` inside a collapsible log group, closing the group however it ends. */
export async function group(name, fn) {
  command('group', name);
  try {
    return await fn();
  } finally {
    io.stdout('::endgroup::\n');
  }
}

/** Fails the step with `message` as an error annotation, once the process exits. */
export function fail(message) {
  command('error', message);
  process.exitCode = 1;
}

// An error's message followed by its causes', which is where fetch says why
// a request failed: "fetch failed: connect ECONNREFUSED 127.0.0.1:443".
function describe(err) {
  const parts = [];
  const seen = new Set();
  for (let e = err; e !== undefined && e !== null && !seen.has(e); e = e.cause) {
    seen.add(e);
    parts.push(e instanceof Error ? e.message : String(e));
  }
  return parts.join(': ');
}

/**
 * Runs an action's `main` and fails the step if it throws: the entry point of
 * every action. The annotation carries the message and its causes; the stack
 * goes to the log.
 */
export async function run(main) {
  try {
    await main();
  } catch (err) {
    fail(describe(err));
    if (err instanceof Error) io.stderr(`${inspect(err)}\n`);
  }
}

// --- File commands ------------------------------------------------------------

function commandFile(variable) {
  const file = process.env[variable];
  if (!file) {
    throw new Error(`${variable} is not set: this runs only as a GitHub Actions step`);
  }
  return file;
}

// The runner reads an entry's first line as NAME=VALUE when its first `=`
// comes before any `<<`, and as NAME<<DELIMITER otherwise. A name holding
// either, or a line break, would end the entry early and turn the lines of
// its value into entries of their own.
function checkName(variable, name) {
  if (name === '' || /[=\r\n]|<</.test(name)) {
    throw new Error(`${variable}: ${JSON.stringify(name)} cannot be a name (empty, or holds '=', '<<' or a line break)`);
  }
}

function toValue(variable, name, value) {
  if (!['string', 'number', 'boolean'].includes(typeof value)) {
    throw new TypeError(`${variable}: ${name} must be a string, number or boolean; got ${typeof value}`);
  }
  return String(value);
}

// The multi-line form, always. The delimiter is a fresh random UUID per
// entry, so a value cannot end its entry early without knowing it.
function appendEntry(variable, name, value) {
  checkName(variable, name);
  const text = toValue(variable, name, value);
  const delimiter = `ghadelimiter_${randomUUID()}`;
  appendFileSync(commandFile(variable), `${name}<<${delimiter}${EOL}${text}${EOL}${delimiter}${EOL}`, 'utf8');
}

/** Sets the step's output `name`. */
export function setOutput(name, value) {
  appendEntry('GITHUB_OUTPUT', name, value);
}

/** Sets environment variable `name` for the job's later steps, and for this process. */
export function exportVariable(name, value) {
  appendEntry('GITHUB_ENV', name, value);
  process.env[name] = String(value);
}

/** Puts `dir` first on PATH for the job's later steps, and for this process. */
export function addPath(dir) {
  if (dir === '' || /[\r\n]/.test(dir)) {
    throw new Error(`GITHUB_PATH: ${JSON.stringify(dir)} cannot be a directory (empty, or holds a line break)`);
  }
  appendFileSync(commandFile('GITHUB_PATH'), `${dir}${EOL}`, 'utf8');
  process.env.PATH = process.env.PATH ? `${dir}${PATH_DELIMITER}${process.env.PATH}` : dir;
}

// --- HTTP ---------------------------------------------------------------------

// A URL as it may appear in a log: no query string, which is where a
// pre-signed URL carries its credentials, and no user or password.
function forLog(url) {
  return `${url.origin}${url.pathname}`;
}

/** A response outside 200–299. `status` is the HTTP status. */
export class HttpError extends Error {
  constructor(method, url, status, statusText) {
    super(`${method} ${forLog(url)}: ${status} ${statusText}`);
    this.name = 'HttpError';
    this.status = status;
  }
}

/**
 * Whether a failed attempt is worth repeating: the server answered 408, 429
 * or 5xx, the attempt timed out, or the request never got an answer (fetch
 * rejects with a TypeError). A URL that does not parse is a TypeError too,
 * which is why fetchWithRetry parses it before the first attempt.
 */
export function isTransient(err) {
  if (err instanceof HttpError) return err.status === 408 || err.status === 429 || err.status >= 500;
  return err?.name === 'TimeoutError' || err instanceof TypeError;
}

/**
 * Calls `fn(attempt)` until it returns, up to `attempts` times, waiting
 * `delayMs`, then twice that, and so on, between attempts. An error that
 * `isRetryable` rejects, or the last attempt's error, is thrown. `label`
 * names the operation in the warning each retry logs.
 */
export async function retry(fn, { attempts = 3, delayMs = 1000, isRetryable = isTransient, label = 'operation' } = {}) {
  for (let attempt = 1; ; attempt++) {
    try {
      return await fn(attempt);
    } catch (err) {
      if (attempt >= attempts || !isRetryable(err)) throw err;
      const wait = delayMs * 2 ** (attempt - 1);
      warning(`${label}: attempt ${attempt} of ${attempts} failed (${describe(err)}); retrying in ${wait} ms`);
      await sleep(wait);
    }
  }
}

// Bodies fetch can send more than once. Anything else, a stream above all,
// is spent by the first attempt.
function isReusableBody(body) {
  return (
    body === undefined ||
    body === null ||
    typeof body === 'string' ||
    body instanceof ArrayBuffer ||
    ArrayBuffer.isView(body) ||
    body instanceof Blob ||
    body instanceof URLSearchParams ||
    body instanceof FormData
  );
}

/**
 * `fetch`, with a timeout on each attempt and bounded retries of transient
 * failures (isTransient). Resolves to a Response with a 2xx status; any
 * other status throws an HttpError. `timeoutMs` covers the whole exchange,
 * reading the body included, since the timeout's signal stays attached to
 * the body: size it for the download. Every other option goes to fetch; a
 * `signal` of the caller's aborts the request as well, and is not retried.
 * A retry sends the body again, so a stream body needs `attempts: 1`.
 */
export async function fetchWithRetry(url, { attempts = 3, delayMs = 1000, timeoutMs = 30_000, ...init } = {}) {
  const target = new URL(url);
  const method = (init.method ?? 'GET').toUpperCase();
  if (attempts > 1 && !isReusableBody(init.body)) {
    throw new TypeError(`${method} ${forLog(target)}: a stream body is spent by the first attempt; pass attempts: 1`);
  }
  return retry(
    async () => {
      const timeout = AbortSignal.timeout(timeoutMs);
      const signal = init.signal ? AbortSignal.any([init.signal, timeout]) : timeout;
      const response = await fetch(target, { ...init, signal });
      if (!response.ok) {
        await response.body?.cancel();
        throw new HttpError(method, target, response.status, response.statusText);
      }
      return response;
    },
    { attempts, delayMs, label: `${method} ${forLog(target)}` },
  );
}

// --- Hashing ------------------------------------------------------------------

/**
 * The SHA-256 of everything `stream` yields, as lowercase hex. Takes any
 * async iterable of bytes: a Node stream (fs.createReadStream) or a fetch
 * Response's body.
 */
export async function sha256(stream) {
  const hash = createHash('sha256');
  for await (const chunk of stream) hash.update(chunk);
  return hash.digest('hex');
}
