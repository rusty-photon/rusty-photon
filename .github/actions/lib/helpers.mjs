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

/**
 * Sets environment variable `name` for the job's later steps, and for this
 * process. NODE_OPTIONS is refused: the runner drops it from GITHUB_ENV
 * with an error of its own (FileCommandManager.cs, `_setEnvBlockList`), so
 * later steps would never see it.
 */
export function exportVariable(name, value) {
  if (name.toUpperCase() === 'NODE_OPTIONS') {
    throw new Error('GITHUB_ENV: the runner does not let a step set NODE_OPTIONS for later steps');
  }
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

// How much of an error response's body an HttpError keeps.
const ERROR_BODY_LIMIT = 4096;

/**
 * A response outside 200–299. `status` is the HTTP status, and
 * `retryAfterMs` the wait its Retry-After header asked for, if it had one.
 *
 * `body` holds the start of the response's body, which is where an API says
 * why it refused. It stays out of the message, and out of what `inspect`
 * prints, because a server may echo the request's credentials there: S3's
 * SignatureDoesNotMatch answer carries the signature it was sent. A caller
 * that trusts what its server returns, such as the GitHub API's error JSON,
 * can put it in an error of its own.
 */
export class HttpError extends Error {
  constructor(method, url, status, statusText, { body = '', retryAfterMs } = {}) {
    super(`${method} ${forLog(url)}: ${status} ${statusText}`);
    this.name = 'HttpError';
    this.status = status;
    this.retryAfterMs = retryAfterMs;
    Object.defineProperty(this, 'body', { value: body, enumerable: false });
  }
}

// Retry-After holds seconds or an HTTP date (RFC 9110, section 10.2.3).
function parseRetryAfter(header) {
  if (header === null) return undefined;
  const text = header.trim();
  if (/^\d+$/.test(text)) return Number(text) * 1000;
  const date = Date.parse(text);
  return Number.isNaN(date) ? undefined : Math.max(0, date - Date.now());
}

// The start of an error response's body, or '' if it cannot be read.
async function errorBody(response) {
  try {
    return (await response.text()).slice(0, ERROR_BODY_LIMIT);
  } catch {
    return '';
  }
}

// Answers that a later attempt may not get: a request timeout, a rate
// limit, and the server's or a gateway's failure or overload. 501 and 505
// say the server cannot do it at all, and are not here.
const TRANSIENT_STATUSES = new Set([408, 429, 500, 502, 503, 504]);

// A server that asks for a longer wait than this is not waited for.
const MAX_RETRY_AFTER_MS = 120_000;

/**
 * Whether a failed attempt is worth repeating: an answer in
 * TRANSIENT_STATUSES (unless its Retry-After is longer than two minutes),
 * a timeout, or a request that never got an answer. fetch reports that last
 * one as `TypeError('fetch failed')`, with the network's error as its cause,
 * which carries a code (ECONNRESET, UND_ERR_SOCKET, ...). Its other
 * TypeErrors are requests it refused to send, such as a GET with a body, and
 * a TypeError from the caller's own code is a bug: neither passes on a
 * second try.
 */
export function isTransient(err) {
  if (err instanceof HttpError) {
    return TRANSIENT_STATUSES.has(err.status) && !(err.retryAfterMs > MAX_RETRY_AFTER_MS);
  }
  if (err?.name === 'TimeoutError') return true;
  return err instanceof TypeError && typeof err.cause?.code === 'string';
}

/**
 * Calls `fn(attempt)` until it returns, up to `attempts` times, waiting
 * `delayMs`, then twice that, and so on, between attempts, or longer when
 * the error carries a `retryAfterMs`. An error that `isRetryable` rejects,
 * or the last attempt's error, is thrown. Once `signal` aborts, nothing is
 * retried and a wait under way ends. `label` names the operation in the
 * warning each retry logs.
 */
export async function retry(fn, { attempts = 3, delayMs = 1000, isRetryable = isTransient, label = 'operation', signal } = {}) {
  for (let attempt = 1; ; attempt++) {
    try {
      return await fn(attempt);
    } catch (err) {
      if (attempt >= attempts || signal?.aborted || !isRetryable(err)) throw err;
      const wait = Math.max(delayMs * 2 ** (attempt - 1), err?.retryAfterMs ?? 0);
      warning(`${label}: attempt ${attempt} of ${attempts} failed (${describe(err)}); retrying in ${wait} ms`);
      await sleep(wait, undefined, { signal });
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

// The methods RFC 9110 (section 9.2.2) calls idempotent: sending one twice
// has the effect of sending it once, so repeating one whose answer was lost
// is safe. A POST or PATCH the server already acted on would act twice.
const IDEMPOTENT_METHODS = new Set(['GET', 'HEAD', 'OPTIONS', 'PUT', 'DELETE']);

/**
 * `fetch`, with a timeout on each attempt and bounded retries of transient
 * failures (isTransient). Resolves to a Response with a 2xx status; any
 * other status throws an HttpError.
 *
 * - `attempts` defaults to 3 for an idempotent method and to 1 for POST and
 *   PATCH. Pass more for one of those only when a repeat is harmless.
 * - `timeoutMs` covers the whole exchange, reading the body included, since
 *   the timeout's signal stays attached to the body: size it for the
 *   download.
 * - A `signal` of the caller's aborts the request, and ends the retries.
 * - A retry sends the body again, so a stream body needs `attempts: 1`.
 *
 * Every other option goes to fetch.
 */
export async function fetchWithRetry(url, { attempts, delayMs = 1000, timeoutMs = 30_000, ...init } = {}) {
  const target = new URL(url);
  const method = (init.method ?? 'GET').toUpperCase();
  const tries = attempts ?? (IDEMPOTENT_METHODS.has(method) ? 3 : 1);
  if (tries > 1 && !isReusableBody(init.body)) {
    throw new TypeError(`${method} ${forLog(target)}: a stream body is spent by the first attempt; pass attempts: 1`);
  }
  return retry(
    async () => {
      const timeout = AbortSignal.timeout(timeoutMs);
      const signal = init.signal ? AbortSignal.any([init.signal, timeout]) : timeout;
      const response = await fetch(target, { ...init, signal });
      if (!response.ok) {
        throw new HttpError(method, target, response.status, response.statusText, {
          body: await errorBody(response),
          retryAfterMs: parseRetryAfter(response.headers.get('retry-after')),
        });
      }
      return response;
    },
    { attempts: tries, delayMs, label: `${method} ${forLog(target)}`, signal: init.signal },
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
