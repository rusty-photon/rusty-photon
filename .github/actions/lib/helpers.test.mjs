import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { EOL, tmpdir } from 'node:os';
import { delimiter as PATH_DELIMITER, join } from 'node:path';
import { Readable } from 'node:stream';
import test from 'node:test';
import { inspect } from 'node:util';

import {
  addPath,
  exportVariable,
  fail,
  fetchWithRetry,
  getBooleanInput,
  getInput,
  group,
  HttpError,
  io,
  isTransient,
  retry,
  run,
  setOutput,
  setSecret,
  sha256,
  warning,
} from './helpers.mjs';

// --- Fixtures -----------------------------------------------------------------

// Sets `vars` (undefined deletes) for the length of `fn`, then puts back what was there.
async function withEnv(vars, fn) {
  const saved = Object.fromEntries(Object.keys(vars).map((k) => [k, process.env[k]]));
  const assign = (values) => {
    for (const [k, v] of Object.entries(values)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  };
  assign(vars);
  try {
    return await fn();
  } finally {
    assign(saved);
  }
}

function emptyFile() {
  const dir = mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'helpers-'));
  const file = join(dir, 'command-file');
  writeFileSync(file, '');
  return file;
}

// What `fn` writes through the helpers' `io`, then puts `io` back.
async function capture(fn) {
  const out = [];
  const err = [];
  const saved = { ...io };
  io.stdout = (text) => out.push(text);
  io.stderr = (text) => err.push(text);
  try {
    const value = await fn();
    return { value, stdout: out.join(''), stderr: err.join('') };
  } finally {
    Object.assign(io, saved);
  }
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

// Serves `handler` on loopback; `requests` lists every request's URL.
async function serve(handler) {
  const requests = [];
  const server = createServer((req, res) => {
    requests.push(req.url);
    handler(req, res, requests.length);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  return {
    url: `http://127.0.0.1:${port}`,
    requests,
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections();
        server.close(resolve);
      }),
  };
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

test('warning puts a multi-line message on one command line', async () => {
  const { stdout } = await capture(() => warning('first\nsecond'));
  assert.equal(stdout, '::warning::first%0Asecond\n');
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
    for (const name of ['', 'a=b', 'a<<b', 'a\nb', 'a\rb']) {
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
  assert.equal(isTransient(answer(429, 120_000)), true, 'Retry-After of two minutes');
  assert.equal(isTransient(answer(429, 120_001)), false, 'Retry-After beyond two minutes');
  assert.equal(isTransient(new DOMException('timed out', 'TimeoutError')), true);
  assert.equal(isTransient(networkFailure()), true);
  assert.equal(isTransient(new TypeError('fetch failed')), false, 'no cause: not from the network');
  assert.equal(isTransient(new TypeError('Request with GET/HEAD method cannot have body.')), false);
  assert.equal(isTransient(new TypeError("Cannot read properties of undefined (reading 'x')")), false);
  assert.equal(isTransient(new DOMException('aborted', 'AbortError')), false);
  assert.equal(isTransient(new Error('pin mismatch')), false);
});

test('retry repeats a transient failure, doubling the wait, until a call returns', async () => {
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
    '::warning::GET x: attempt 1 of 3 failed (fetch failed: read ECONNRESET); retrying in 1 ms\n' +
      '::warning::GET x: attempt 2 of 3 failed (fetch failed: read ECONNRESET); retrying in 2 ms\n',
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
          throw networkFailure(`E${calls}`);
        },
        { attempts: 2, delayMs: 1 },
      ),
      (err) => {
        assert.equal(err.cause.code, 'E2');
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

test('retry stops once its signal aborts, ending a wait under way', async () => {
  const controller = new AbortController();
  let calls = 0;
  const started = Date.now();
  await capture(() =>
    assert.rejects(
      retry(
        async () => {
          calls += 1;
          setTimeout(() => controller.abort(), 20);
          throw networkFailure();
        },
        { attempts: 3, delayMs: 5_000, signal: controller.signal },
      ),
      { name: 'AbortError' },
    ),
  );
  assert.equal(calls, 1);
  assert.ok(Date.now() - started < 2_500, 'the 5 s wait ran to its end');
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

test('fetchWithRetry retries a request whose connection drops before an answer', async (t) => {
  const server = await serve((req) => req.socket.destroy());
  t.after(server.close);
  const { stdout } = await capture(() => assert.rejects(fetchWithRetry(`${server.url}/x`, { attempts: 2, delayMs: 1 }), TypeError));
  assert.equal(server.requests.length, 2);
  assert.match(stdout, /^::warning::GET http:\/\/127\.0\.0\.1:\d+\/x: attempt 1 of 2 failed \(fetch failed: .+\)/);
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
  await capture(() =>
    assert.rejects(fetchWithRetry(`${server.url}/slow`, { attempts: 2, delayMs: 1, timeoutMs: 100 }), { name: 'TimeoutError' }),
  );
  assert.equal(server.requests.length, 2);
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
  const pending = fetchWithRetry(`${server.url}/x`, { signal: controller.signal, delayMs: 1 });
  controller.abort();
  await assert.rejects(pending, { name: 'AbortError' });
  assert.ok(server.requests.length <= 1);
});

test("fetchWithRetry does not retry the caller's own timeout", async (t) => {
  const server = await serve(() => {});
  t.after(server.close);
  const { stdout } = await capture(() =>
    assert.rejects(fetchWithRetry(`${server.url}/x`, { signal: AbortSignal.timeout(100), delayMs: 1 }), { name: 'TimeoutError' }),
  );
  assert.equal(stdout, '');
  assert.equal(server.requests.length, 1);
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
