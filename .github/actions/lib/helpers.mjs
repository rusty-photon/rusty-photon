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
    parts.push(explain(e));
  }
  return parts.join(': ');
}

// One error's own text. fetch's cause for a refused connection to a host
// with both an IPv4 and an IPv6 address is an AggregateError with an empty
// message: the reason is in the errors it holds, one per address.
function explain(e) {
  if (!(e instanceof Error)) return String(e);
  if (e.message) return e.message;
  if (e instanceof AggregateError && e.errors.length > 0) return e.errors.map(explain).join('; ');
  return e.code ?? e.name;
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
// its value into entries of their own. One ending in `<` would join the
// `<<` after it, and the runner, splitting at the first `<<`, would look
// for a delimiter that never comes and refuse the whole file.
function checkName(variable, name) {
  if (name === '' || /[=\r\n]|<<|<$/.test(name)) {
    throw new Error(`${variable}: ${JSON.stringify(name)} cannot be a name (empty, holds '=', '<<' or a line break, or ends in '<')`);
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
  if (typeof dir !== 'string' || dir === '' || /[\r\n]/.test(dir)) {
    throw new Error(`GITHUB_PATH: ${JSON.stringify(dir)} cannot be a directory (not a string, empty, or holds a line break)`);
  }
  appendFileSync(commandFile('GITHUB_PATH'), `${dir}${EOL}`, 'utf8');
  process.env.PATH = process.env.PATH ? `${dir}${PATH_DELIMITER}${process.env.PATH}` : dir;
}

// --- HTTP ---------------------------------------------------------------------

// A URL as it may appear in a log: no query string, which is where a
// pre-signed URL carries its credentials, and no user or password.
function forLog(url) {
  try {
    const { origin, pathname } = new URL(url);
    return `${origin}${pathname}`;
  } catch {
    return '(a URL that does not parse)';
  }
}

// The URL a request goes to. The errors Node gives for one that does not
// parse, or for one with a user or password, carry the whole URL (the
// first in an `input` property, which `run` logs), so neither is let
// through: the query string can be a pre-signed URL's signature.
function requestUrl(method, url) {
  let target;
  try {
    target = new URL(url);
  } catch {
    throw new TypeError(`${method}: the URL does not parse (it is not shown, as it may hold credentials)`);
  }
  if (target.username || target.password) {
    throw new TypeError(`${method} ${forLog(target)}: a URL cannot carry a user or password; send them in a header`);
  }
  return target;
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

// The wait a response asks for: its Retry-After, or else, from the GitHub
// API, the time until its rate limit resets. GitHub's REST docs ("Rate
// limits for the REST API"): with `x-ratelimit-remaining` at 0, "you
// should not retry your request until after the time specified by the
// `x-ratelimit-reset` header", which is in UTC epoch seconds.
function requestedWait(headers) {
  const retryAfter = parseRetryAfter(headers.get('retry-after'));
  if (retryAfter !== undefined) return retryAfter;
  const reset = headers.get('x-ratelimit-reset');
  if (headers.get('x-ratelimit-remaining') !== '0' || !/^\d+$/.test(reset ?? '')) return undefined;
  return Math.max(0, Number(reset) * 1000 - Date.now());
}

// The start of an error response's body: no more of it is read than is
// kept, and a body that breaks off keeps what arrived.
async function errorBody(response) {
  if (!response.body) return '';
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let text = '';
  try {
    while (text.length < ERROR_BODY_LIMIT) {
      const { done, value } = await reader.read();
      if (done) break;
      text += decoder.decode(value, { stream: true });
    }
  } catch {
    // Keep what arrived.
  } finally {
    reader.cancel().catch(() => {});
  }
  return text.slice(0, ERROR_BODY_LIMIT);
}

// Answers that a later attempt may not get: a request timeout, a rate
// limit, and the server's or a gateway's failure or overload. 501 and 505
// say the server cannot do it at all, and are not here.
const TRANSIENT_STATUSES = new Set([408, 429, 500, 502, 503, 504]);

// A wait longer than this, asked for by a Retry-After, is not waited out.
const MAX_RETRY_AFTER_MS = 120_000;

// The codes of a request that got no answer: the connection was refused,
// reset or cut, it or the network timed out or could not be reached, the
// name lookup failed, or undici's socket closed or timed out. ENOTFOUND is
// here though it also means a name that does not exist: a resolver's
// hiccup can report it too, and a wrong name costs only the retries. A
// certificate the server presents (CERT_HAS_EXPIRED,
// ERR_TLS_CERT_ALTNAME_INVALID) fails the same way every time, and is not
// here.
const NO_ANSWER_CODES = new Set([
  'ECONNREFUSED',
  'ECONNRESET',
  'ECONNABORTED',
  'EPIPE',
  'ETIMEDOUT',
  'ENETUNREACH',
  'EHOSTUNREACH',
  'EAI_AGAIN',
  'ENOTFOUND',
  'UND_ERR_SOCKET',
  'UND_ERR_CLOSED',
  'UND_ERR_CONNECT_TIMEOUT',
  'UND_ERR_HEADERS_TIMEOUT',
  'UND_ERR_BODY_TIMEOUT',
]);

/**
 * Whether a failed attempt is worth repeating: an answer in
 * TRANSIENT_STATUSES, a 403 that says how long to wait, a timeout, or a
 * request that got no answer.
 *
 * GitHub's API answers a rate limit with a 403 or a 429, and gives the wait
 * in Retry-After or the reset headers (requestedWait); a 403 with neither
 * is a refusal, which no later attempt passes. fetch reports a request that
 * got no answer as `TypeError('fetch failed')`, with the network's error as
 * its cause, whose code is in NO_ANSWER_CODES. Its other TypeErrors are
 * requests it refused to send, such as a GET with a body, and a TypeError
 * from the caller's own code is a bug: neither passes on a second try.
 */
export function isTransient(err) {
  if (err instanceof HttpError) {
    return TRANSIENT_STATUSES.has(err.status) || (err.status === 403 && err.retryAfterMs !== undefined);
  }
  if (err?.name === 'TimeoutError') return true;
  return err instanceof TypeError && NO_ANSWER_CODES.has(err.cause?.code);
}

/**
 * Calls `fn(attempt)` until it returns, up to `attempts` times, waiting
 * `delayMs`, then twice that, and so on, between attempts, or longer when
 * the error carries a `retryAfterMs`. An error that `isRetryable` rejects,
 * one whose `retryAfterMs` is over two minutes, or the last attempt's error
 * is thrown. Once `signal` aborts, no attempt starts and nothing is
 * retried, and a wait under way ends with an AbortError whose cause is the
 * error it was waiting to retry. `label` names the operation in the warning
 * each retry logs. A fetchWithRetry inside `fn` should get `attempts: 1`,
 * or the two retry counts multiply.
 */
export async function retry(fn, { attempts = 3, delayMs = 1000, isRetryable = isTransient, label = 'operation', signal } = {}) {
  for (let attempt = 1; ; attempt++) {
    signal?.throwIfAborted();
    try {
      return await fn(attempt);
    } catch (err) {
      if (attempt >= attempts || signal?.aborted || !isRetryable(err) || err?.retryAfterMs > MAX_RETRY_AFTER_MS) {
        throw err;
      }
      const wait = Math.max(delayMs * 2 ** (attempt - 1), err?.retryAfterMs ?? 0);
      warning(`${label}: attempt ${attempt} of ${attempts} failed (${describe(err)}); retrying in ${wait} ms`);
      try {
        await sleep(wait, undefined, { signal });
      } catch (abort) {
        const stopped = new Error(`${label}: stopped while waiting to retry (${describe(abort)})`, { cause: err });
        stopped.name = 'AbortError';
        throw stopped;
      }
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
  // Upper-cased for fetch too, which sends any method but the six the
  // Fetch standard normalises (PATCH is not one) exactly as written.
  const method = (init.method ?? 'GET').toUpperCase();
  const target = requestUrl(method, url);
  const tries = attempts ?? (IDEMPOTENT_METHODS.has(method) ? 3 : 1);
  if (tries > 1 && !isReusableBody(init.body)) {
    throw new TypeError(`${method} ${forLog(target)}: a stream body is spent by the first attempt; pass attempts: 1`);
  }
  return retry(
    async () => {
      const timeout = AbortSignal.timeout(timeoutMs);
      const signal = init.signal ? AbortSignal.any([init.signal, timeout]) : timeout;
      const response = await fetch(target, { ...init, method, signal });
      if (!response.ok) {
        throw new HttpError(method, target, response.status, response.statusText, {
          body: await errorBody(response),
          retryAfterMs: requestedWait(response.headers),
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
