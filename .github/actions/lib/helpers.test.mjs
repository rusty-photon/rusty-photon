import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { EOL, tmpdir } from 'node:os';
import { delimiter as PATH_DELIMITER, basename, join } from 'node:path';
import { Readable } from 'node:stream';
import test from 'node:test';
import { inspect } from 'node:util';

import {
  addPath,
  command,
  describe,
  error,
  execute,
  exportVariable,
  fail,
  fetchWithRetry,
  getBooleanInput,
  getInput,
  getState,
  group,
  HttpError,
  isTransient,
  maskUrl,
  notice,
  requestFailure,
  retry,
  run,
  saveState,
  setOutput,
  setSecret,
  sha256,
  warning,
  withoutRunnerVariables,
} from './helpers.mjs';
import { capture, serve, withEnv } from './testing.mjs';

// --- Fixtures -----------------------------------------------------------------

function emptyFile() {
  const dir = mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'helpers-'));
  const file = join(dir, 'command-file');
  writeFileSync(file, '');
  return file;
}

// A port of the runner's EnvFileKeyValuePairs (actions/runner,
// src/Runner.Worker/FileCommandManager.cs), which reads GITHUB_OUTPUT and
// GITHUB_ENV: a line is NAME=VALUE when its first `=` comes before any `<<`,
// NAME<<DELIMITER otherwise, and only Windows splits lines on CRLF.
function parseCommandFile(text) {
  let index = 0;
  let newline = null;
  const readLine = () => {
    if (index >= text.length) {
      newline = null;
      return null;
    }
    const start = index;
    const lf = text.indexOf('\n', index);
    if (lf < 0) {
      index = text.length;
      newline = null;
      return text.slice(start);
    }
    const crlf = process.platform === 'win32' ? text.indexOf('\r\n', index) : -1;
    if (crlf >= 0 && crlf < lf) {
      index = crlf + 2;
      newline = '\r\n';
      return text.slice(start, crlf);
    }
    index = lf + 1;
    newline = '\n';
    return text.slice(start, lf);
  };
  const entries = [];
  for (let line = readLine(); line !== null; line = readLine()) {
    if (line === '') continue;
    const equals = line.indexOf('=');
    const heredoc = line.indexOf('<<');
    if (equals >= 0 && (heredoc < 0 || equals < heredoc)) {
      entries.push([line.slice(0, equals), line.slice(equals + 1)]);
    } else if (heredoc >= 0) {
      const name = line.slice(0, heredoc);
      const delimiter = line.slice(heredoc + 2);
      assert.ok(name !== '' && delimiter !== '', `invalid entry line ${JSON.stringify(line)}`);
      const start = index;
      let end = index;
      for (let next = readLine(); next !== delimiter; next = readLine()) {
        assert.ok(next !== null, `no closing delimiter ${delimiter}`);
        assert.ok(newline !== null, 'delimiter without a line break after it');
        end = index - newline.length;
      }
      entries.push([name, end > start ? text.slice(start, end) : '']);
    } else {
      assert.fail(`invalid entry line ${JSON.stringify(line)}`);
    }
  }
  return entries;
}

// --- Inputs -------------------------------------------------------------------

test('getInput reads INPUT_<NAME>, upper-cased with spaces as underscores, and trims it', async () => {
  await withEnv({ 'INPUT_CACHE_KEY': '  v1-abc \n', 'INPUT_DRY-RUN': 'true' }, () => {
    assert.equal(getInput('cache key'), 'v1-abc');
    assert.equal(getInput('dry-run'), 'true');
  });
});

test('getInput reads an unset input as empty, and refuses it when required', async () => {
  await withEnv({ INPUT_TOKEN: undefined }, () => {
    assert.equal(getInput('token'), '');
    assert.throws(() => getInput('token', { required: true }), /^Error: Input required and not supplied: token$/);
  });
});

test('getBooleanInput accepts the YAML 1.2 core spellings only', async () => {
  for (const [raw, expected] of [['true', true], ['True', true], ['TRUE', true], ['false', false], ['False', false], ['FALSE', false]]) {
    await withEnv({ INPUT_FLAG: raw }, () => assert.equal(getBooleanInput('flag'), expected, raw));
  }
  for (const raw of ['yes', '1', 'tru', '']) {
    await withEnv({ INPUT_FLAG: raw }, () => assert.throws(() => getBooleanInput('flag'), TypeError, raw));
  }
});

// --- Workflow commands --------------------------------------------------------

test('setSecret escapes the three characters the runner unescapes', async () => {
  const { stdout } = await capture(() => setSecret('50%\r\nnext'));
  assert.equal(stdout, '::add-mask::50%25%0D%0Anext\n');
});

test('notice puts a multi-line message on one command line', async () => {
  const { stdout } = await capture(() => notice('first\nsecond'));
  assert.equal(stdout, '::notice::first%0Asecond\n');
});

test('warning puts a multi-line message on one command line', async () => {
  const { stdout } = await capture(() => warning('first\nsecond'));
  assert.equal(stdout, '::warning::first%0Asecond\n');
});

test('error annotates without failing the step', async (t) => {
  const saved = process.exitCode;
  t.after(() => {
    process.exitCode = saved;
  });
  process.exitCode = undefined;
  const { stdout } = await capture(() => error('first\nsecond'));
  assert.equal(stdout, '::error::first%0Asecond\n');
  assert.equal(process.exitCode, undefined);
});

test('group closes the group and returns what the function returns', async () => {
  const { value, stdout } = await capture(() => group('Fetch', async () => 42));
  assert.equal(value, 42);
  assert.equal(stdout, '::group::Fetch\n::endgroup::\n');
});

test('group closes the group when the function throws', async () => {
  const { stdout } = await capture(() =>
    assert.rejects(
      group('Fetch', async () => {
        throw new Error('broken');
      }),
      /broken/,
    ),
  );
  assert.equal(stdout, '::group::Fetch\n::endgroup::\n');
});

test('run fails the step with the error as an annotation, and the stack in the log', async (t) => {
  t.after(() => {
    process.exitCode = undefined;
  });
  const { stdout, stderr } = await capture(() =>
    run(async () => {
      throw new Error('no such release\nv1.2.3');
    }),
  );
  assert.equal(stdout, '::error::no such release%0Av1.2.3\n');
  assert.match(stderr, /^Error: no such release\nv1\.2\.3\n\s+at /);
  assert.equal(process.exitCode, 1);
});

test('run puts the causes in the annotation, where fetch says why it failed', async (t) => {
  t.after(() => {
    process.exitCode = undefined;
  });
  const { stdout } = await capture(() =>
    run(async () => {
      throw new TypeError('fetch failed', { cause: new Error('connect ECONNREFUSED 127.0.0.1:443') });
    }),
  );
  assert.equal(stdout, '::error::fetch failed: connect ECONNREFUSED 127.0.0.1:443\n');
});

test('run explains a cause with no message of its own, as fetch gives for a dual-stack host', async (t) => {
  t.after(() => {
    process.exitCode = undefined;
  });
  const refused = Object.assign(
    new AggregateError([new Error('connect ECONNREFUSED ::1:59999'), new Error('connect ECONNREFUSED 127.0.0.1:59999')], ''),
    { code: 'ECONNREFUSED' },
  );
  const { stdout } = await capture(() =>
    run(async () => {
      throw new TypeError('fetch failed', { cause: refused });
    }),
  );
  assert.equal(stdout, '::error::fetch failed: connect ECONNREFUSED ::1:59999; connect ECONNREFUSED 127.0.0.1:59999\n');
});

test('describe joins the causes, and stops at one it has already seen', () => {
  const inner = new Error('connect ETIMEDOUT');
  const outer = new TypeError('fetch failed', { cause: inner });
  inner.cause = outer;
  assert.equal(describe(outer), 'fetch failed: connect ETIMEDOUT');
  assert.equal(describe('a string'), 'a string');
});

test('run leaves a step that returns alone', async () => {
  const { stdout, stderr } = await capture(() => run(async () => {}));
  assert.equal(stdout, '');
  assert.equal(stderr, '');
  assert.equal(process.exitCode, undefined);
});

test('fail sets the exit code without ending the process', async (t) => {
  t.after(() => {
    process.exitCode = undefined;
  });
  const { stdout } = await capture(() => fail('pin mismatch'));
  assert.equal(stdout, '::error::pin mismatch\n');
  assert.equal(process.exitCode, 1);
});

// --- File commands ------------------------------------------------------------

test('setOutput writes an entry the runner reads back whole, line breaks and look-alike lines included', async () => {
  const file = emptyFile();
  const value = 'first line\nkey=value\nname<<EOF\nEOF\n';
  await withEnv({ GITHUB_OUTPUT: file }, () => {
    setOutput('report', value);
    setOutput('hit', true);
    setOutput('count', 3);
    setOutput('empty', '');
  });
  assert.deepEqual(parseCommandFile(readFileSync(file, 'utf8')), [
    ['report', value],
    ['hit', 'true'],
    ['count', '3'],
    ['empty', ''],
  ]);
});

test('setOutput refuses a name the runner would split, and writes nothing', async () => {
  const file = emptyFile();
  await withEnv({ GITHUB_OUTPUT: file }, () => {
    for (const name of ['', 'a=b', 'a<<b', 'a\nb', 'a\rb', 'a<']) {
      assert.throws(() => setOutput(name, 'x'), /cannot be a name/, JSON.stringify(name));
    }
  });
  assert.equal(readFileSync(file, 'utf8'), '');
});

test('setOutput refuses a value that would print as [object Object] or undefined', async () => {
  const file = emptyFile();
  await withEnv({ GITHUB_OUTPUT: file }, () => {
    assert.throws(() => setOutput('a', { b: 1 }), TypeError);
    assert.throws(() => setOutput('a', undefined), TypeError);
  });
  assert.equal(readFileSync(file, 'utf8'), '');
});

test('setOutput outside a runner says so', async () => {
  await withEnv({ GITHUB_OUTPUT: undefined }, () => {
    assert.throws(() => setOutput('a', 'b'), /^Error: GITHUB_OUTPUT is not set/);
  });
});

test('exportVariable writes GITHUB_ENV and sets the variable for this process too', async () => {
  const file = emptyFile();
  await withEnv({ GITHUB_ENV: file, HELPERS_TEST_VAR: undefined }, () => {
    exportVariable('HELPERS_TEST_VAR', 'one\ntwo');
    assert.equal(process.env.HELPERS_TEST_VAR, 'one\ntwo');
  });
  assert.deepEqual(parseCommandFile(readFileSync(file, 'utf8')), [['HELPERS_TEST_VAR', 'one\ntwo']]);
});

test('exportVariable refuses NODE_OPTIONS, which the runner would drop', async () => {
  const file = emptyFile();
  const before = process.env.NODE_OPTIONS;
  await withEnv({ GITHUB_ENV: file }, () => {
    for (const name of ['NODE_OPTIONS', 'node_options']) {
      assert.throws(() => exportVariable(name, '--max-old-space-size=4096'), /NODE_OPTIONS/);
    }
  });
  assert.equal(process.env.NODE_OPTIONS, before);
  assert.equal(readFileSync(file, 'utf8'), '');
});

test('saveState writes GITHUB_STATE in the form the runner reads, and getState reads STATE_<name>', async () => {
  const file = emptyFile();
  await withEnv({ GITHUB_STATE: file }, () => saveState('key', 'one\ntwo'));
  assert.deepEqual(parseCommandFile(readFileSync(file, 'utf8')), [['key', 'one\ntwo']]);
  await withEnv({ STATE_key: 'abc', STATE_other: undefined }, () => {
    assert.equal(getState('key'), 'abc');
    assert.equal(getState('other'), '');
  });
});

test('addPath appends a line to GITHUB_PATH and puts the directory first on PATH', async () => {
  const file = emptyFile();
  const dir = join('opt', 'tool', 'bin');
  await withEnv({ GITHUB_PATH: file, PATH: 'existing' }, () => {
    addPath(dir);
    assert.equal(process.env.PATH, `${dir}${PATH_DELIMITER}existing`);
  });
  assert.equal(readFileSync(file, 'utf8'), `${dir}${EOL}`);
});

test('addPath refuses a directory with a line break, which would add a second entry', async () => {
  const file = emptyFile();
  await withEnv({ GITHUB_PATH: file }, () => {
    assert.throws(() => addPath('/opt/a\n/tmp/evil'), /cannot be a directory/);
    assert.throws(() => addPath(''), /cannot be a directory/);
    assert.throws(() => addPath(undefined), /cannot be a directory/);
    assert.throws(() => addPath(42), /cannot be a directory/);
  });
  assert.equal(readFileSync(file, 'utf8'), '');
});

// --- Retries ------------------------------------------------------------------

// What fetch rejects with when a request gets no answer: the network's error,
// with its code, is the cause.
function networkFailure(code = 'ECONNRESET') {
  return new TypeError('fetch failed', { cause: Object.assign(new Error(`read ${code}`), { code }) });
}

test('isTransient retries what may pass on a second try, and nothing else', () => {
  const url = new URL('https://example.invalid/x');
  const answer = (status, retryAfterMs) => new HttpError('GET', url, status, '', { retryAfterMs });
  for (const status of [408, 429, 500, 502, 503, 504]) assert.equal(isTransient(answer(status)), true, status);
  for (const status of [400, 401, 403, 404, 409, 422, 501, 505]) assert.equal(isTransient(answer(status)), false, status);
  assert.equal(isTransient(answer(403, 0)), true, 'a 403 that says to retry now');
  assert.equal(isTransient(answer(403, 60_000)), true, 'a 403 that says to wait a minute');
  assert.equal(isTransient(new DOMException('timed out', 'TimeoutError')), true);
  assert.equal(isTransient(networkFailure()), true);
  assert.equal(isTransient(networkFailure('UND_ERR_SOCKET')), true);
  assert.equal(isTransient(networkFailure('EAI_AGAIN')), true);
  const dualStack = Object.assign(new AggregateError([], ''), { code: 'ECONNREFUSED' });
  assert.equal(isTransient(new TypeError('fetch failed', { cause: dualStack })), true);
  assert.equal(isTransient(networkFailure('CERT_HAS_EXPIRED')), false, 'a certificate fails the same way again');
  assert.equal(isTransient(networkFailure('ERR_TLS_CERT_ALTNAME_INVALID')), false);
  assert.equal(isTransient(networkFailure('ENOTFOUND')), true, 'a resolver hiccup can report it');
  assert.equal(isTransient(new TypeError('fetch failed')), false, 'no cause: not from the network');
  assert.equal(isTransient(new TypeError('Request with GET/HEAD method cannot have body.')), false);
  assert.equal(isTransient(new TypeError("Cannot read properties of undefined (reading 'x')")), false);
  assert.equal(isTransient(new DOMException('aborted', 'AbortError')), false);
  assert.equal(isTransient(new Error('pin mismatch')), false);
});

test('retry repeats a transient failure, doubling the wait, until a call returns, and logs each retry as a plain line', async () => {
  const calls = [];
  const { value, stdout } = await capture(() =>
    retry(
      async (attempt) => {
        calls.push(attempt);
        if (attempt < 3) throw networkFailure();
        return 'done';
      },
      { attempts: 3, delayMs: 1, label: 'GET x' },
    ),
  );
  assert.equal(value, 'done');
  assert.deepEqual(calls, [1, 2, 3]);
  assert.equal(
    stdout,
    'GET x: attempt 1 of 3 failed (fetch failed: read ECONNRESET); retrying in 1 ms\n' +
      'GET x: attempt 2 of 3 failed (fetch failed: read ECONNRESET); retrying in 2 ms\n',
  );
});

test('retry waits as long as an error asks, when that is longer', async () => {
  const { stdout } = await capture(() =>
    retry(
      async (attempt) => {
        if (attempt === 1) throw Object.assign(networkFailure(), { retryAfterMs: 5 });
        return 'done';
      },
      { delayMs: 1 },
    ),
  );
  assert.match(stdout, /retrying in 5 ms\n$/);
});

test('retry gives up after the last attempt and throws its error', async () => {
  let calls = 0;
  await capture(() =>
    assert.rejects(
      retry(
        async () => {
          calls += 1;
          throw Object.assign(networkFailure(), { attempt: calls });
        },
        { attempts: 2, delayMs: 1 },
      ),
      (err) => {
        assert.equal(err.attempt, 2);
        return true;
      },
    ),
  );
  assert.equal(calls, 2);
});

test('retry throws an error that is not transient at once', async () => {
  let calls = 0;
  const { stdout } = await capture(() =>
    assert.rejects(
      retry(
        async () => {
          calls += 1;
          throw new Error('pin mismatch');
        },
        { attempts: 5, delayMs: 1 },
      ),
      /pin mismatch/,
    ),
  );
  assert.equal(calls, 1);
  assert.equal(stdout, '');
});

test('retry stops once its signal aborts, ending a wait under way, and says what it was retrying', async () => {
  const controller = new AbortController();
  const failure = networkFailure();
  let calls = 0;
  const started = Date.now();
  await capture(() =>
    assert.rejects(
      retry(
        async () => {
          calls += 1;
          setTimeout(() => controller.abort(), 20);
          throw failure;
        },
        { attempts: 3, delayMs: 5_000, signal: controller.signal, label: 'GET x' },
      ),
      (err) => {
        assert.equal(err.name, 'AbortError');
        assert.match(err.message, /^GET x: stopped while waiting to retry/);
        assert.equal(err.cause, failure);
        return true;
      },
    ),
  );
  assert.equal(calls, 1);
  assert.ok(Date.now() - started < 2_500, 'the 5 s wait ran to its end');
});

test('retry refuses an attempts that is not a whole number of 1 or more, before any attempt', async () => {
  for (const attempts of [NaN, 0, 1.5, '3']) {
    let calls = 0;
    await assert.rejects(
      retry(
        async () => {
          calls += 1;
        },
        { attempts },
      ),
      TypeError,
      String(attempts),
    );
    assert.equal(calls, 0, String(attempts));
  }
});

test('retry starts no attempt once its signal has aborted', async () => {
  let calls = 0;
  await assert.rejects(
    retry(
      async () => {
        calls += 1;
      },
      { signal: AbortSignal.abort() },
    ),
    { name: 'AbortError' },
  );
  assert.equal(calls, 0);
});

test('retry does not wait out a Retry-After over two minutes, whatever isRetryable says', async () => {
  let calls = 0;
  const { stdout } = await capture(() =>
    assert.rejects(
      retry(
        async () => {
          calls += 1;
          throw Object.assign(new Error('rate limited'), { retryAfterMs: 120_001 });
        },
        { isRetryable: () => true, delayMs: 1 },
      ),
      /rate limited/,
    ),
  );
  assert.equal(calls, 1);
  assert.equal(stdout, '');
});

// --- HTTP ---------------------------------------------------------------------

test('fetchWithRetry retries 5xx answers and returns the first 2xx', async (t) => {
  const server = await serve((req, res, n) => {
    res.statusCode = n < 3 ? 503 : 200;
    res.end(n < 3 ? 'busy' : 'ok');
  });
  t.after(server.close);
  const { value: response } = await capture(() => fetchWithRetry(`${server.url}/file`, { delayMs: 1 }));
  assert.equal(await response.text(), 'ok');
  assert.equal(server.requests.length, 3);
});

test('fetchWithRetry throws a 4xx answer at once, with its body kept out of the message and the log', async (t) => {
  const reason = '{"message":"Validation Failed"}';
  const server = await serve((req, res) => {
    res.statusCode = 422;
    res.end(reason);
  });
  t.after(server.close);
  await assert.rejects(fetchWithRetry(`${server.url}/issues`, { delayMs: 1 }), (err) => {
    assert.ok(err instanceof HttpError);
    assert.equal(err.status, 422);
    assert.equal(err.body, reason);
    assert.equal(err.message, `GET ${server.url}/issues: 422 Unprocessable Entity`);
    assert.doesNotMatch(inspect(err), /Validation Failed/);
    return true;
  });
  assert.equal(server.requests.length, 1);
});

test('HttpError takes a URL as a string too', () => {
  const err = new HttpError('GET', 'https://example.invalid/x?sig=1', 404, 'Not Found');
  assert.equal(err.message, 'GET https://example.invalid/x: 404 Not Found');
});

test('fetchWithRetry reads no more of an error body than it keeps', async (t) => {
  const server = await serve((req, res) => {
    res.writeHead(500, { 'content-length': '100000' });
    res.write('x'.repeat(5000));
  });
  t.after(server.close);
  const started = Date.now();
  await assert.rejects(fetchWithRetry(`${server.url}/x`, { attempts: 1, timeoutMs: 10_000 }), (err) => {
    assert.equal(err.status, 500);
    assert.equal(err.body, 'x'.repeat(4096));
    return true;
  });
  // Under the 2 s that errorBody allows a stalled body: the first 4 KiB
  // ends the read, not the timer.
  assert.ok(Date.now() - started < 1_500, 'it read on past the 4 KiB it keeps');
});

test('fetchWithRetry gives up on an error body that stalls, keeping what arrived', async (t) => {
  const server = await serve((req, res) => {
    res.writeHead(503, { 'content-length': '1000' });
    res.write('overloaded');
  });
  t.after(server.close);
  const started = Date.now();
  await assert.rejects(fetchWithRetry(`${server.url}/x`, { attempts: 1, timeoutMs: 10_000 }), (err) => {
    assert.equal(err.status, 503);
    assert.equal(err.body, 'overloaded');
    return true;
  });
  assert.ok(Date.now() - started < 4_000, 'it waited on the body for the attempt timeout');
});

test("fetchWithRetry waits a minute after the GitHub API's secondary rate limit, when it sends no wait", async (t) => {
  const server = await serve((req, res) => {
    res.writeHead(req.url === '/limited' ? 403 : 429);
    res.end(req.url === '/forbidden' ? '{"message":"Resource not accessible by integration"}' : '{"message":"You have exceeded a secondary rate limit. Please wait a few minutes before you try again."}');
  });
  t.after(server.close);
  for (const path of ['/limited', '/too-many']) {
    await assert.rejects(fetchWithRetry(`${server.url}${path}`, { attempts: 1 }), (err) => {
      assert.equal(err.retryAfterMs, 60_000, path);
      assert.equal(isTransient(err), true, path);
      return true;
    });
  }
  await assert.rejects(fetchWithRetry(`${server.url}/forbidden`, { attempts: 1 }), (err) => {
    assert.equal(err.retryAfterMs, undefined);
    return true;
  });
});

test("fetchWithRetry measures a rate-limit reset against the server's clock, not the runner's", async (t) => {
  // The server's clock is an hour behind the runner's, and its limit resets
  // 30 s from its own now; Date has whole seconds, so this is exact.
  const serverNow = Math.floor((Date.now() - 3_600_000) / 1000);
  const server = await serve((req, res) => {
    res.writeHead(403, {
      date: new Date(serverNow * 1000).toUTCString(),
      'x-ratelimit-remaining': '0',
      'x-ratelimit-reset': String(serverNow + 30),
    });
    res.end();
  });
  t.after(server.close);
  await assert.rejects(fetchWithRetry(`${server.url}/x`, { attempts: 1 }), (err) => {
    assert.equal(err.retryAfterMs, 31_000, 'thirty seconds, and one for rounding');
    return true;
  });
});

test('fetchWithRetry sends the method upper-cased, as its logs and errors name it', async (t) => {
  const methods = [];
  const server = await serve((req, res) => {
    methods.push(req.method);
    res.end();
  });
  t.after(server.close);
  await fetchWithRetry(`${server.url}/issues/1`, { method: 'patch', body: '{}' });
  assert.deepEqual(methods, ['PATCH']);
});

test('fetchWithRetry does not retry a 501, which no later attempt can pass', async (t) => {
  const server = await serve((req, res) => {
    res.statusCode = 501;
    res.end();
  });
  t.after(server.close);
  await assert.rejects(fetchWithRetry(`${server.url}/x`, { delayMs: 1 }), { name: 'HttpError', status: 501 });
  assert.equal(server.requests.length, 1);
});

test('fetchWithRetry stops after its attempts and throws the last answer', async (t) => {
  const server = await serve((req, res) => {
    res.statusCode = 500;
    res.end();
  });
  t.after(server.close);
  await capture(() => assert.rejects(fetchWithRetry(`${server.url}/x`, { attempts: 2, delayMs: 1 }), { name: 'HttpError', status: 500 }));
  assert.equal(server.requests.length, 2);
});

test('fetchWithRetry sends a POST once by default, since the server may have acted on it', async (t) => {
  const server = await serve((req, res) => {
    res.statusCode = 502;
    res.end();
  });
  t.after(server.close);
  await assert.rejects(fetchWithRetry(`${server.url}/issues`, { method: 'POST', body: '{}', delayMs: 1 }), { status: 502 });
  assert.equal(server.requests.length, 1);
  await capture(() =>
    assert.rejects(fetchWithRetry(`${server.url}/issues`, { method: 'POST', body: '{}', attempts: 2, delayMs: 1 }), { status: 502 }),
  );
  assert.equal(server.requests.length, 3, 'an explicit attempts count retries a POST');
});

test('fetchWithRetry waits out a Retry-After, and gives up on one longer than two minutes', async (t) => {
  const later = new Date(Date.now() + 3_600_000).toUTCString();
  const server = await serve((req, res, n) => {
    if (req.url === '/soon') {
      res.statusCode = n === 1 ? 429 : 200;
      if (n === 1) res.setHeader('retry-after', '1');
    } else {
      res.statusCode = 503;
      res.setHeader('retry-after', later);
    }
    res.end();
  });
  t.after(server.close);
  const started = Date.now();
  const { stdout } = await capture(() => fetchWithRetry(`${server.url}/soon`, { delayMs: 1 }));
  assert.ok(Date.now() - started >= 900, 'the second attempt came before the second Retry-After asked for');
  assert.match(stdout, /retrying in 1000 ms/);
  await assert.rejects(fetchWithRetry(`${server.url}/later`, { delayMs: 1 }), (err) => {
    assert.equal(err.status, 503);
    assert.ok(err.retryAfterMs > 3_500_000 && err.retryAfterMs <= 3_600_000, `retryAfterMs ${err.retryAfterMs}`);
    return true;
  });
  assert.equal(server.requests.length, 3);
});

test("fetchWithRetry retries a 403 only when it says how long to wait, as the GitHub API's rate limits do", async (t) => {
  const resetNow = String(Math.floor(Date.now() / 1000));
  const server = await serve((req, res, n) => {
    const first = server.requests.filter((r) => r.url === req.url).length === 1;
    if (req.url === '/retry-after' && first) {
      res.writeHead(403, { 'retry-after': '0' });
    } else if (req.url === '/reset' && first) {
      res.writeHead(403, { 'x-ratelimit-remaining': '0', 'x-ratelimit-reset': resetNow });
    } else if (req.url === '/refused') {
      res.writeHead(403);
    } else {
      res.writeHead(200);
    }
    res.end();
  });
  t.after(server.close);
  await capture(async () => {
    assert.equal((await fetchWithRetry(`${server.url}/retry-after`, { delayMs: 1 })).status, 200);
    assert.equal((await fetchWithRetry(`${server.url}/reset`, { delayMs: 1 })).status, 200);
  });
  await assert.rejects(fetchWithRetry(`${server.url}/refused`, { delayMs: 1 }), { status: 403 });
  assert.deepEqual(
    server.requests.map((r) => r.url),
    ['/retry-after', '/retry-after', '/reset', '/reset', '/refused'],
  );
});

test('fetchWithRetry retries a request whose connection drops before an answer', async (t) => {
  const server = await serve((req) => req.socket.destroy());
  t.after(server.close);
  const { stdout } = await capture(() => assert.rejects(fetchWithRetry(`${server.url}/x`, { attempts: 2, delayMs: 1 }), TypeError));
  assert.equal(server.requests.length, 2);
  assert.match(stdout, /^GET http:\/\/127\.0\.0\.1:\d+\/x: attempt 1 of 2 failed \(fetch failed: .+\)/);
});

test('fetchWithRetry does not retry a request fetch refuses to send', async (t) => {
  const server = await serve((req, res) => res.end());
  t.after(server.close);
  const { stdout } = await capture(() => assert.rejects(fetchWithRetry(`${server.url}/x`, { body: 'x', delayMs: 1 }), TypeError));
  assert.equal(stdout, '');
  assert.equal(server.requests.length, 0);
});

test('fetchWithRetry times out an attempt that gets no response, and retries it', async (t) => {
  const server = await serve(() => {});
  t.after(server.close);
  const { stdout } = await capture(() =>
    assert.rejects(fetchWithRetry(`${server.url}/slow`, { attempts: 2, delayMs: 1, timeoutMs: 100 }), { name: 'TimeoutError' }),
  );
  // The retry's log line, not the server's count: under load an attempt
  // can time out before its request reaches the server at all.
  assert.match(stdout, /^GET .*\/slow: attempt 1 of 2 failed \(The operation was aborted due to timeout\); retrying in 1 ms\n$/);
});

test('fetchWithRetry times out a body that stops arriving', async (t) => {
  const server = await serve((req, res) => {
    res.writeHead(200, { 'content-length': '10' });
    res.write('abc');
  });
  t.after(server.close);
  const response = await fetchWithRetry(`${server.url}/stalls`, { attempts: 1, timeoutMs: 200 });
  await assert.rejects(response.text(), { name: 'TimeoutError' });
});

test('fetchWithRetry keeps the query string, where a pre-signed URL keeps its signature, out of errors', async (t) => {
  const server = await serve((req, res) => {
    res.statusCode = 403;
    res.end();
  });
  t.after(server.close);
  await assert.rejects(fetchWithRetry(`${server.url}/bucket/key?X-Amz-Signature=s3cr3t`, { method: 'put', body: 'x' }), (err) => {
    assert.equal(err.message, `PUT ${server.url}/bucket/key: 403 Forbidden`);
    return true;
  });
});

test('fetchWithRetry refuses an attempts that is not a count, before any request', async (t) => {
  const server = await serve((req, res) => res.end());
  t.after(server.close);
  await assert.rejects(fetchWithRetry(`${server.url}/x`, { attempts: Number('three') }), TypeError);
  assert.equal(server.requests.length, 0);
});

test('fetchWithRetry refuses a stream body it could not send twice, before any request', async (t) => {
  const server = await serve((req, res) => res.end());
  t.after(server.close);
  await assert.rejects(
    fetchWithRetry(`${server.url}/upload`, { method: 'PUT', body: Readable.from(['x']), duplex: 'half' }),
    /^TypeError: PUT .*: a stream body is spent by the first attempt; pass attempts: 1$/,
  );
  assert.equal(server.requests.length, 0);
});

test("fetchWithRetry honours the caller's own abort signal, without retrying it", async (t) => {
  const server = await serve(() => {});
  t.after(server.close);
  const controller = new AbortController();
  const { stdout } = await capture(() => {
    const pending = fetchWithRetry(`${server.url}/x`, { signal: controller.signal, delayMs: 1 });
    controller.abort();
    return assert.rejects(pending, { name: 'AbortError' });
  });
  assert.equal(stdout, '', 'a retry was attempted');
  assert.ok(server.requests.length <= 1);
});

test('fetchWithRetry keeps a URL with credentials out of its error, logged stack included', async (t) => {
  const server = await serve((req, res) => res.end());
  t.after(server.close);
  const port = new URL(server.url).port;
  for (const url of [`http://user:pa55@127.0.0.1:${port}/x?X-Amz-Signature=s3cr3t`, 'https://exa mple.invalid/x?X-Amz-Signature=s3cr3t']) {
    await assert.rejects(fetchWithRetry(url), (err) => {
      assert.ok(err instanceof TypeError, err.name);
      assert.doesNotMatch(inspect(err), /s3cr3t|pa55/);
      return true;
    });
  }
  assert.equal(server.requests.length, 0);
});

test("fetchWithRetry does not retry the caller's own timeout", async (t) => {
  const server = await serve(() => {});
  t.after(server.close);
  const { stdout } = await capture(() =>
    assert.rejects(fetchWithRetry(`${server.url}/x`, { signal: AbortSignal.timeout(100), delayMs: 1 }), { name: 'TimeoutError' }),
  );
  assert.equal(stdout, '', 'a retry was attempted');
});

// --- A LAN address --------------------------------------------------------------

test('maskUrl masks the URL, its host and its hostname before returning it without trailing slashes', async () => {
  const { value, stdout } = await capture(() => maskUrl('http://cache.lan:8080//', 'RP_LAN_CACHE_URL'));
  assert.equal(value, 'http://cache.lan:8080');
  assert.deepEqual(stdout.split('\n').slice(0, -1), [
    '::add-mask::http://cache.lan:8080',
    '::add-mask::cache.lan:8080',
    '::add-mask::cache.lan',
  ]);
});

test('maskUrl masks an IPv6 address with and without its brackets, as a connect error prints it', async () => {
  const { stdout } = await capture(() => maskUrl('http://[fd00::5]:8080', 'RP_LAN_CACHE_URL'));
  assert.deepEqual(stdout.split('\n').slice(0, -1), [
    '::add-mask::http://[fd00::5]:8080',
    '::add-mask::[fd00::5]:8080',
    '::add-mask::[fd00::5]',
    '::add-mask::fd00::5',
  ]);
});

test('maskUrl refuses a URL that does not parse, naming it without showing it', async () => {
  await assert.rejects(
    capture(() => maskUrl('not a url', 'RP_LAN_CACHE_URL')),
    (err) => /^RP_LAN_CACHE_URL does not parse as a URL/.test(err.message) && !err.message.includes('not a url'),
  );
});

test('requestFailure gives a status or error codes, never a message that may hold the address', () => {
  const refused = Object.assign(new AggregateError([new Error('connect ECONNREFUSED 10.9.8.7:80')], ''), { code: 'ECONNREFUSED' });
  assert.equal(requestFailure(new TypeError('fetch failed', { cause: refused })), 'ECONNREFUSED');
  const reset = Object.assign(new Error('read ECONNRESET 10.9.8.7:80'), { code: 'UND_ERR_SOCKET' });
  assert.equal(requestFailure(new TypeError('terminated', { cause: reset })), 'UND_ERR_SOCKET');
  assert.equal(requestFailure(new DOMException('The operation was aborted due to timeout', 'TimeoutError')), 'no answer in time');
  assert.equal(requestFailure(new RangeError('something at 10.9.8.7')), 'RangeError');
  assert.equal(requestFailure(new HttpError('GET', 'http://10.9.8.7/cas/x', 503, 'Service Unavailable')), 'HTTP 503');
});

// --- Processes ----------------------------------------------------------------

// The one program every OS and Bazel's sandbox are sure to have: this Node.
const node = (script) => command(process.execPath, ['-e', script]);

test('withoutRunnerVariables drops ACTIONS_* and INPUT_*, in any case, and keeps the rest', () => {
  const env = { ACTIONS_RUNTIME_TOKEN: 't', actions_cache_url: 'u', INPUT_PIN: 'p', CACHE_WRITE_TOKEN: 'kept', PATH: '/bin', MY_ACTIONS_X: 'kept' };
  assert.deepEqual(withoutRunnerVariables(env), { CACHE_WRITE_TOKEN: 'kept', PATH: '/bin', MY_ACTIONS_X: 'kept' });
  assert.equal(env.ACTIONS_RUNTIME_TOKEN, 't', 'the caller\'s object is left alone');
});

test('command resolves to what the program prints', async () => {
  assert.equal(await node("process.stdout.write('out'); process.stderr.write('err')"), 'out');
});

test('command runs in `cwd`, with `env` added to the environment', async () => {
  const dir = mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'command-'));
  const out = await command(process.execPath, ['-e', 'process.stdout.write(process.cwd() + "|" + process.env.RP_TEST + "|" + Boolean(process.env.PATH))'], {
    cwd: dir,
    env: { RP_TEST: 'set' },
  });
  const [cwd, ...rest] = out.split('|');
  assert.equal(realpathSync(cwd), realpathSync(dir));
  assert.deepEqual(rest, ['set', 'true']);
});

test('command rejects with the exit code and the last line of stderr', async () => {
  await assert.rejects(node("console.error('first'); console.error('last'); process.exit(3)"), {
    message: `${process.execPath} exited with 3: last`,
  });
});

test('command rejects a program that cannot start', async () => {
  await assert.rejects(command('rp-no-such-program', []), { code: 'ENOENT' });
});

const NODE = basename(process.execPath);
const PRINTS = "process.stdout.write('out\\n'); process.stderr.write('err\\n')";

test("execute copies a program's output to the log and resolves to its stdout", async () => {
  const { value, stdout, stderr } = await capture(() => execute(process.execPath, ['-e', PRINTS]));
  assert.equal(value, 'out\n');
  assert.equal(stdout, 'out\n');
  assert.equal(stderr, 'err\n');
});

test('execute leaves stdout out of the log when echo is false, and still resolves to it', async () => {
  const { value, stdout, stderr } = await capture(() => execute(process.execPath, ['-e', PRINTS], { echo: false }));
  assert.equal(value, 'out\n');
  assert.equal(stdout, '');
  assert.equal(stderr, 'err\n');
});

test("execute gives a program the environment without the runner's variables, unless given one", async () => {
  const script = 'process.stdout.write(JSON.stringify([process.env.ACTIONS_RUNTIME_TOKEN, process.env.INPUT_TOKEN, process.env.RP_KEPT, process.env.RP_ONLY]))';
  await withEnv({ ACTIONS_RUNTIME_TOKEN: 'runtime', INPUT_TOKEN: 'input', RP_KEPT: 'kept' }, async () => {
    assert.equal(await execute(process.execPath, ['-e', script], { echo: false }), '[null,null,"kept",null]');
    const env = { ...withoutRunnerVariables(process.env), RP_ONLY: 'only' };
    delete env.RP_KEPT;
    assert.equal(await execute(process.execPath, ['-e', script], { env, echo: false }), '[null,null,null,"only"]');
  });
});

test('execute rejects with the exit code and the last line of stderr, or the code alone', async () => {
  await capture(() =>
    assert.rejects(execute(process.execPath, ['-e', "console.error('first'); console.error('last'); process.exit(2)"]), {
      message: `${NODE} exited with 2: last`,
    }),
  );
  await assert.rejects(execute(process.execPath, ['-e', 'process.exit(3)']), { message: `${NODE} exited with 3` });
});

test('execute rejects a program that cannot start', async () => {
  await assert.rejects(execute('rp-no-such-program', []), { code: 'ENOENT' });
});

test('execute stops the program when its signal aborts, and rejects with an AbortError', async () => {
  const stop = new AbortController();
  let child;
  // Aborted once the child has started, not inside spawn, where Node would
  // emit the error before execute listens for it.
  const start = (...args) => {
    child = spawn(...args);
    child.once('spawn', () => stop.abort());
    return child;
  };
  await assert.rejects(execute(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { spawn: start, signal: stop.signal }), {
    name: 'AbortError',
  });
  await new Promise((resolve) => (child.exitCode !== null || child.signalCode !== null ? resolve() : child.once('exit', resolve)));
  assert.ok(child.killed);
});

// --- Hashing ------------------------------------------------------------------

// FIPS 180-2's "abc" vector, and the hash of nothing.
const SHA256_ABC = 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad';
const SHA256_EMPTY = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855';

test('sha256 hashes everything a stream yields, however it is chunked', async () => {
  assert.equal(await sha256(Readable.from([Buffer.from('abc')])), SHA256_ABC);
  assert.equal(await sha256(Readable.from(['a', 'b', 'c'])), SHA256_ABC);
  assert.equal(await sha256(Readable.from([])), SHA256_EMPTY);
});

test("sha256 hashes a fetch response's body", async (t) => {
  const server = await serve((req, res) => res.end('abc'));
  t.after(server.close);
  const response = await fetchWithRetry(`${server.url}/file`);
  assert.equal(await sha256(response.body), SHA256_ABC);
});
