import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { createServer } from 'node:net';
import { chmodSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, readlinkSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join } from 'node:path';
import test from 'node:test';
import { gzipSync } from 'node:zlib';

import { HttpError } from './helpers.mjs';
import {
  cacheKey,
  canWrite,
  checkReplaceable,
  ChecksumError,
  download,
  downloadAll,
  findRuntime,
  forRunner,
  get,
  hostKey,
  INSTALLED,
  isoWeek,
  linkRuntime,
  loaderFinds,
  makeRunnable,
  markInstalled,
  onlyDirectory,
  pinKey,
  powershellProgram,
  redirectTarget,
  restoreKey,
  runnerKey,
  tarProgram,
  unpack,
  unpackZips,
  withWorkDir,
} from './install.mjs';
import { capture, serve, tar, withEnv } from './testing.mjs';

// --- Fixtures -----------------------------------------------------------------

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'install-'));
}

const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');

const COMMIT = 'b0802f28055b67aa6a99580d260c3bb4c27eba4b';
const PAYLOAD = Buffer.from('the asset\n'.repeat(1000));

// Each archive holds tool-1.0/bin/tool ("hello\n", mode 755) and
// tool-1.0/README ("notes\n"), made by the tools that publish such assets:
// GNU tar with gzip and xz, and Info-ZIP's zip.
const ARCHIVES = {
  '.tar.gz':
    'H4sIAAAAAAACA+3VsQrCMBSF4cw+RV9AzU2TdBbs6OIbKBQUSgM2vr814KCC4JCI9P+Wm2TJcMhJDKFfykqvVT560jiX5uR1prU4' +
    'Ma6urU3njXFGVU4VcB3j4TJdqeYpPvLft5vtrs2Wv7f2Q/7+OX/RYryqNPlnN4TYjQuFub//43nI9Qd83f+itaH/y+d/3/wk/7f+' +
    'b4zQ/yWcur4P9D8AAAAAAAAAAAAA/L0bLIMJWwAoAAA=',
  '.tar.xz':
    '/Td6WFoAAATm1rRGBMCpAYBQIQEcAAAAAAAAAObXqAXgJ/8AoV0AOhvs2C0NRXoZe5r8vhqcPePZQBxbvOCtc/GyYOT6QNAsP3P1' +
    '6s9Txwd975l2P9drKQ4r914IXl9v/s5QW1Lx2V2W5CORj8WUOVTYYYK4ezWbcPnR1bDJtlvMqFP9o1bMxPVPIOl4VkOvlwW0VHVi' +
    'Q1C684foAqjWmWYAtUmVLMEuKM/8WaQg0C5GhdaftmWqjB5eCG4Ax2BauemwZpkaGAAAAAAAdEbZxB3YyXMAAcUBgFAAAOHq9T6x' +
    'xGf7AgAAAAAEWVo=',
  '.zip':
    'UEsDBAoAAAAAAAAAIVwAAAAAAAAAAAAAAAAJAAAAdG9vbC0xLjAvUEsDBAoAAAAAAAAAIVw+0tjWBgAAAAYAAAAPAAAAdG9vbC0x' +
    'LjAvUkVBRE1Fbm90ZXMKUEsDBAoAAAAAAAAAIVwAAAAAAAAAAAAAAAANAAAAdG9vbC0xLjAvYmluL1BLAwQKAAAAAAAAACFcIDA6' +
    'NgYAAAAGAAAAEQAAAHRvb2wtMS4wL2Jpbi90b29saGVsbG8KUEsBAh4DCgAAAAAAAAAhXAAAAAAAAAAAAAAAAAkAAAAAAAAAAAAQ' +
    'AO1BAAAAAHRvb2wtMS4wL1BLAQIeAwoAAAAAAAAAIVw+0tjWBgAAAAYAAAAPAAAAAAAAAAEAAACkgScAAAB0b29sLTEuMC9SRUFE' +
    'TUVQSwECHgMKAAAAAAAAACFcAAAAAAAAAAAAAAAADQAAAAAAAAAAABAA7UFaAAAAdG9vbC0xLjAvYmluL1BLAQIeAwoAAAAAAAAA' +
    'IVwgMDo2BgAAAAYAAAARAAAAAAAAAAEAAADtgYUAAAB0b29sLTEuMC9iaW4vdG9vbFBLBQYAAAAABAAEAO4AAAC6AAAAAAA=',
};

// The formats each OS's assets come in, so each leg unpacks what it meets:
// Linux, OmniSim's .tar.xz, Pebble's .tar.gz and ASTAP's .zip; macOS, zips
// and .tar.gz; Windows, zips and .tar.gz.
const FORMATS = {
  linux: ['.tar.gz', '.tar.xz', '.zip'],
  darwin: ['.tar.gz', '.tar.xz', '.zip'],
  win32: ['.tar.gz', '.zip'],
}[process.platform];

// A `run` that records each command it is given and answers it from
// `answers` (a program to its output), running none of them.
function recorder(answers = {}) {
  const calls = [];
  const run = async (program, args) => {
    calls.push([program, ...args]);
    return answers[program] ?? '';
  };
  return { run, calls };
}

function asset(server, pin, name = 'asset.bin') {
  return { name, url: `${server.url}/${name}`, pin };
}

const fast = { delayMs: 1 };

// --- Choosing the asset ---------------------------------------------------------

test('runnerKey joins RUNNER_OS and RUNNER_ARCH', async () => {
  await withEnv({ RUNNER_OS: 'Linux', RUNNER_ARCH: 'ARM64' }, async () => {
    assert.equal(runnerKey(), 'Linux-ARM64');
  });
  assert.equal(runnerKey({ os: 'macOS', arch: 'X64' }), 'macOS-X64');
});

test('runnerKey refuses to guess outside a runner', async () => {
  await withEnv({ RUNNER_OS: undefined, RUNNER_ARCH: 'X64' }, async () => {
    assert.throws(() => runnerKey(), /RUNNER_OS and RUNNER_ARCH are not set/);
  });
});

test('hostKey gives this machine the key a runner like it has', () => {
  assert.equal(hostKey({ platform: 'linux', arch: 'x64' }), 'Linux-X64');
  assert.equal(hostKey({ platform: 'linux', arch: 'arm64' }), 'Linux-ARM64');
  assert.equal(hostKey({ platform: 'darwin', arch: 'arm64' }), 'macOS-ARM64');
  assert.equal(hostKey({ platform: 'darwin', arch: 'x64' }), 'macOS-X64');
  assert.equal(hostKey({ platform: 'win32', arch: 'x64' }), 'Windows-X64');
  assert.equal(hostKey({ platform: 'win32', arch: 'ia32' }), 'Windows-X86');
  assert.match(hostKey(), /^(Linux|macOS|Windows)-/);
});

test('hostKey refuses a machine no runner is like', () => {
  assert.throws(() => hostKey({ platform: 'freebsd', arch: 'x64' }), {
    message: "no GitHub runner has this machine's OS and architecture, freebsd x64",
  });
  assert.throws(() => hostKey({ platform: 'linux', arch: 'riscv64' }), /linux riscv64/);
  assert.throws(() => hostKey({ platform: 'toString', arch: 'x64' }), /toString x64/);
});

test('forRunner picks the runner its table names, and names the others when it has none', () => {
  const table = { 'Linux-X64': 'a', 'Windows-X64': 'b' };
  assert.equal(forRunner(table, 'Windows-X64', 'Tool'), 'b');
  assert.throws(() => forRunner(table, 'Linux-ARM64', 'Tool'), {
    message: 'Tool has no asset for Linux-ARM64; it has one for Linux-X64, Windows-X64',
  });
  assert.throws(() => forRunner(table, 'toString', 'Tool'), /no asset for toString/);
});

// --- Pins and the cache key -----------------------------------------------------

test('isoWeek agrees with date -u +%G-%V, across year ends', () => {
  const cases = {
    '2026-10-18T23:59:59Z': '2026-42', // a Sunday ends its week
    '2026-10-19T00:00:00Z': '2026-43',
    '2021-01-03T12:00:00Z': '2020-53', // early January in the last year's week
    '2021-01-04T00:00:00Z': '2021-01',
    '2024-12-30T00:00:00Z': '2025-01', // late December in the next year's week
    '2020-12-31T00:00:00Z': '2020-53',
    '2027-01-01T00:00:00Z': '2026-53',
  };
  for (const [date, week] of Object.entries(cases)) {
    assert.equal(isoWeek(new Date(date)), week, date);
  }
});

test('pinKey gives each kind of pin its part of the key', () => {
  const hash = 'a'.repeat(64);
  assert.equal(pinKey({ sha256: hash }), hash);
  assert.equal(pinKey({ ref: COMMIT }), COMMIT);
  assert.equal(pinKey({ rolling: true }, new Date('2026-10-18T00:00:00Z')), 'week-2026-42');
});

test('pinKey refuses a pin that does not pin', () => {
  const refused = [
    [undefined, /a pin is one of/],
    [{}, /a pin is one of/],
    [{ sha256: 'a'.repeat(64), ref: COMMIT }, /a pin is one of/],
    [{ sha256: 'A'.repeat(64) }, /not a SHA-256 in lowercase hex/],
    [{ sha256: 'a'.repeat(63) }, /not a SHA-256 in lowercase hex/],
    [{ ref: 'master' }, /not a commit's full SHA, so what it names could move/],
    [{ ref: COMMIT.slice(0, 12) }, /not a commit's full SHA/],
    [{ rolling: 'yes' }, /pin rolling must be true/],
  ];
  for (const [pin, message] of refused) {
    assert.throws(() => pinKey(pin), message, JSON.stringify(pin));
  }
});

test('cacheKey carries each asset and its pin, so moving a pin misses the old cache', () => {
  const a = { name: 'a.zip', pin: { sha256: '1'.repeat(64) } };
  const b = { name: 'b.so', pin: { ref: COMMIT } };
  assert.equal(cacheKey('tool-v1', [a, b]), `install-tool-v1-a.zip-${'1'.repeat(64)}-b.so-${COMMIT}`);
  assert.notEqual(cacheKey('tool-v1', [{ ...a, pin: { sha256: '2'.repeat(64) } }]), cacheKey('tool-v1', [a]));
});

test('cacheKey of a rolling download moves with the ISO week', () => {
  const sdk = { name: 'sdk.zip', pin: { rolling: true } };
  const sunday = cacheKey('sdk', [sdk], { now: new Date('2026-10-18T23:00:00Z') });
  const monday = cacheKey('sdk', [sdk], { now: new Date('2026-10-19T01:00:00Z') });
  assert.equal(sunday, 'install-sdk-sdk.zip-week-2026-42');
  assert.equal(monday, 'install-sdk-sdk.zip-week-2026-43');
});

test('cacheKey carries a pin once after a run of assets that share it, and each pin of another', () => {
  const ref = { ref: COMMIT };
  const files = [
    { name: 'a.h', pin: ref },
    { name: 'liba.so', pin: ref },
    { name: 'b.zip', pin: { sha256: '1'.repeat(64) } },
    { name: 'libc.so', pin: ref },
  ];
  assert.equal(cacheKey('sdk', files), `install-sdk-a.h-liba.so-${COMMIT}-b.zip-${'1'.repeat(64)}-libc.so-${COMMIT}`);
  const weekly = [
    { name: 'a.zip', pin: { rolling: true } },
    { name: 'b.zip', pin: { rolling: true } },
  ];
  assert.equal(cacheKey('sdk', weekly, { now: new Date('2026-10-19T01:00:00Z') }), 'install-sdk-a.zip-b.zip-week-2026-43');
});

test('restoreKey is the start every earlier week\'s key of the same rolling downloads has, and nothing for pinned ones', () => {
  const weekly = [
    { name: 'a.zip', pin: { rolling: true } },
    { name: 'b.zip', pin: { rolling: true } },
  ];
  const prefix = restoreKey('sdk', weekly, { now: new Date('2026-10-19T01:00:00Z') });
  assert.equal(prefix, 'install-sdk-a.zip-b.zip-week-');
  assert.ok(cacheKey('sdk', weekly, { now: new Date('2025-12-31T01:00:00Z') }).startsWith(prefix));
  assert.ok(!cacheKey('sdk', weekly.slice(0, 1), { now: new Date('2025-12-31T01:00:00Z') }).startsWith(prefix));
  assert.equal(restoreKey('sdk', [{ name: 'a.so', pin: { ref: COMMIT } }]), '');
  assert.equal(restoreKey('sdk', [weekly[0], { name: 'c.zip', pin: { sha256: '1'.repeat(64) } }]), '');
});

// --- Downloading ------------------------------------------------------------------

// --- Fetching ----------------------------------------------------------------------

// A server speaking raw HTTP/1.1, which answers every request with `answer`
// and closes its side, as a SourceForge mirror's nginx does after
// `Connection: close`.
async function rawServer(answer) {
  const server = createServer((socket) => {
    socket.once('data', () => {
      socket.write(answer);
      socket.end();
    });
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  return { url: `http://127.0.0.1:${server.address().port}`, close: () => new Promise((done) => server.close(done)) };
}

test('get hands over a body whole when the server closed before it was read', async () => {
  // The case that crashes Node 24's fetch (nodejs/undici#5360): a body of a
  // full stream buffer, 64 KiB, paused unread when the server's FIN comes.
  const body = Buffer.alloc(64 * 1024, 0x61);
  const server = await rawServer(
    Buffer.concat([Buffer.from(`HTTP/1.1 200 OK\r\nContent-Length: ${body.length}\r\nConnection: close\r\n\r\n`), body]),
  );
  try {
    const response = await get(`${server.url}/asset.bin`);
    await new Promise((resolve) => setTimeout(resolve, 200));
    const chunks = [];
    for await (const chunk of response) chunks.push(chunk);
    assert.deepEqual(Buffer.concat(chunks), body);
  } finally {
    await server.close();
  }
});

test('get follows redirects, relative and absolute, to the answer', async () => {
  const server = await serve((req, res) => {
    if (req.url === '/download') {
      res.writeHead(302, { location: '/mirror/asset.bin' });
      res.end('moved');
    } else if (req.url === '/mirror/asset.bin') {
      res.writeHead(307, { location: `http://${req.headers.host}/final` });
      res.end();
    } else {
      res.end(PAYLOAD);
    }
  });
  try {
    const response = await get(`${server.url}/download`, { headers: { 'user-agent': 'test' } });
    const chunks = [];
    for await (const chunk of response) chunks.push(chunk);
    assert.deepEqual(Buffer.concat(chunks), PAYLOAD);
    assert.deepEqual(server.requests.map(({ url }) => url), ['/download', '/mirror/asset.bin', '/final']);
    // Each hop carries the request's headers.
    assert.ok(server.requests.every(({ headers }) => headers['user-agent'] === 'test'));
  } finally {
    await server.close();
  }
});

test('get sends an Authorization header on to its own origin only, as fetch does', async () => {
  const elsewhere = await serve((req, res) => res.end(PAYLOAD));
  const server = await serve((req, res) => {
    if (req.url === '/download') {
      res.writeHead(302, { location: '/moved' });
    } else {
      res.writeHead(302, { location: `${elsewhere.url}/final` });
    }
    res.end();
  });
  try {
    const response = await get(`${server.url}/download`, { headers: { Authorization: 'Bearer token', 'user-agent': 'test' } });
    response.resume();
    assert.deepEqual(server.requests.map(({ headers }) => headers.authorization), ['Bearer token', 'Bearer token']);
    assert.equal(elsewhere.requests.length, 1);
    assert.equal(elsewhere.requests[0].headers.authorization, undefined);
    assert.equal(elsewhere.requests[0].headers['user-agent'], 'test');
  } finally {
    await server.close();
    await elsewhere.close();
  }
});

test('get gives up after 20 redirects', async () => {
  const server = await serve((req, res) => {
    res.writeHead(302, { location: '/again' });
    res.end();
  });
  try {
    await assert.rejects(get(`${server.url}/start`), /more than 20 redirects/);
    assert.equal(server.requests.length, 21);
  } finally {
    await server.close();
  }
});

test('get throws an HttpError for an answer outside 2xx, with the wait it asks for', async () => {
  const server = await serve((req, res) => {
    res.writeHead(503, { 'retry-after': '7' });
    res.end('busy');
  });
  try {
    await assert.rejects(get(`${server.url}/asset.bin?token=secret`), (err) => {
      assert.ok(err instanceof HttpError);
      assert.equal(err.status, 503);
      assert.equal(err.retryAfterMs, 7_000);
      assert.doesNotMatch(err.message, /secret/);
      return true;
    });
  } finally {
    await server.close();
  }
});

test('redirectTarget reads a Location against the URL it came from, and refuses https to http', () => {
  const from = new URL('https://sourceforge.net/projects/x/files/a.zip/download');
  assert.equal(redirectTarget(from, '/project/x/a.zip').href, 'https://sourceforge.net/project/x/a.zip');
  assert.equal(redirectTarget(from, 'https://mirror.example/a.zip').href, 'https://mirror.example/a.zip');
  assert.throws(() => redirectTarget(from, 'http://mirror.example/a.zip'), /refused a redirect from https to http:/);
  assert.equal(redirectTarget(new URL('http://127.0.0.1:1/a'), 'http://127.0.0.1:1/b').href, 'http://127.0.0.1:1/b');
});

test('download writes an asset that matches its SHA-256, and says so', async () => {
  const server = await serve((req, res) => res.end(PAYLOAD));
  const file = join(tempDir(), 'sub', 'asset.bin');
  try {
    const { stdout } = await capture(() => download(asset(server, { sha256: sha(PAYLOAD) }), file, fast));
    assert.deepEqual(readFileSync(file), PAYLOAD);
    assert.match(stdout, new RegExp(`asset.bin: SHA-256 ${sha(PAYLOAD)} matches its pin`));
    assert.deepEqual(readdirSync(join(file, '..')), ['asset.bin']);
    // The bytes as the server keeps them, never re-encoded on the way.
    assert.equal(server.requests[0].headers['accept-encoding'], 'identity');
  } finally {
    await server.close();
  }
});

test('download sends the headers it is given, beside its own', async () => {
  const server = await serve((req, res) => res.end(PAYLOAD));
  const file = join(tempDir(), 'asset.bin');
  try {
    await capture(() => download(asset(server, { sha256: sha(PAYLOAD) }), file, { ...fast, headers: { authorization: 'Bearer token' } }));
    assert.equal(server.requests[0].headers.authorization, 'Bearer token');
    assert.equal(server.requests[0].headers['accept-encoding'], 'identity');
  } finally {
    await server.close();
  }
});

test('download refuses a mismatch after trying again, and leaves nothing behind', async () => {
  const server = await serve((req, res) => res.end('something else'));
  const file = join(tempDir(), 'asset.bin');
  try {
    await capture(() =>
      assert.rejects(download(asset(server, { sha256: sha(PAYLOAD) }), file, fast), (err) => {
        assert.ok(err instanceof ChecksumError);
        assert.match(err.message, new RegExp(`expected ${sha(PAYLOAD)}, got ${sha(Buffer.from('something else'))}`));
        return true;
      }),
    );
    assert.equal(server.requests.length, 3);
    assert.deepEqual(readdirSync(join(file, '..')), []);
  } finally {
    await server.close();
  }
});

test('download tries again after a body cut short', async () => {
  const server = await serve((req, res, n) => {
    res.writeHead(200, { 'content-length': PAYLOAD.length });
    if (n === 1) {
      res.write(PAYLOAD.subarray(0, 100), () => res.destroy());
      return;
    }
    res.end(PAYLOAD);
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    const { stdout } = await capture(() => download(asset(server, { sha256: sha(PAYLOAD) }), file, fast));
    assert.equal(server.requests.length, 2);
    assert.match(stdout, /download asset.bin: attempt 1 of 3 failed/);
    assert.deepEqual(readFileSync(file), PAYLOAD);
  } finally {
    await server.close();
  }
});

test('download gives up on a body cut short every time', async () => {
  const server = await serve((req, res) => {
    res.writeHead(200, { 'content-length': PAYLOAD.length });
    res.write(PAYLOAD.subarray(0, 100), () => res.destroy());
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    await capture(() => assert.rejects(download(asset(server, { sha256: sha(PAYLOAD) }), file, fast)));
    assert.equal(server.requests.length, 3);
    assert.deepEqual(readdirSync(join(file, '..')), []);
  } finally {
    await server.close();
  }
});

test('download abandons a transfer that stalls, and tries again', async () => {
  const server = await serve((req, res, n) => {
    res.writeHead(200, { 'content-length': PAYLOAD.length });
    if (n === 1) {
      res.write(PAYLOAD.subarray(0, 100)); // and nothing more
      return;
    }
    res.end(PAYLOAD);
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    const { stdout } = await capture(() =>
      download(asset(server, { sha256: sha(PAYLOAD) }), file, { ...fast, stallMs: 500 }),
    );
    assert.equal(server.requests.length, 2);
    assert.match(stdout, /attempt 1 of 3 failed \(no data for 500 ms\)/);
    assert.deepEqual(readFileSync(file), PAYLOAD);
  } finally {
    await server.close();
  }
});

test('download lets a slow transfer run on while data keeps coming', async () => {
  const chunks = 15;
  const size = Math.ceil(PAYLOAD.length / chunks);
  const server = await serve((req, res) => {
    res.writeHead(200, { 'content-length': PAYLOAD.length });
    let sent = 0;
    const timer = setInterval(() => {
      res.write(PAYLOAD.subarray(sent * size, (sent + 1) * size));
      sent += 1;
      if (sent === chunks) {
        clearInterval(timer);
        res.end();
      }
    }, 100);
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    // 15 chunks 100 ms apart take longer than the stall timeout in all, never
    // between two of them.
    await capture(() => download(asset(server, { sha256: sha(PAYLOAD) }), file, { ...fast, stallMs: 1_000 }));
    assert.equal(server.requests.length, 1);
    assert.deepEqual(readFileSync(file), PAYLOAD);
  } finally {
    await server.close();
  }
});

// A server whose first answer dribbles `PAYLOAD`, 10 bytes every 50 ms, a
// rate of 200 bytes/s, and whose later answers send it whole.
async function dribbleFirst() {
  return serve((req, res, n) => {
    res.writeHead(200, { 'content-length': PAYLOAD.length });
    if (n > 1) {
      res.end(PAYLOAD);
      return;
    }
    let sent = 0;
    const timer = setInterval(() => {
      res.write(PAYLOAD.subarray(sent, sent + 10));
      sent += 10;
    }, 50);
    res.on('close', () => clearInterval(timer));
  });
}

test('download abandons a transfer slower than its floor, and tries again', async () => {
  const server = await dribbleFirst();
  const file = join(tempDir(), 'asset.bin');
  try {
    const { stdout } = await capture(() =>
      download(asset(server, { sha256: sha(PAYLOAD) }), file, { ...fast, stallMs: 500, minBytesPerSecond: 1_000 }),
    );
    assert.equal(server.requests.length, 2);
    assert.match(stdout, /attempt 1 of 3 failed \(slower than 1000 bytes\/s for 500 ms\)/);
    assert.deepEqual(readFileSync(file), PAYLOAD);
  } finally {
    await server.close();
  }
});

test('download catches a transfer that slows part way through', async () => {
  // Half the payload at once, then 200 bytes/s for as long as the client
  // listens.
  const server = await serve((req, res, n) => {
    res.writeHead(200, { 'content-length': PAYLOAD.length });
    if (n > 1) {
      res.end(PAYLOAD);
      return;
    }
    let sent = PAYLOAD.length / 2;
    res.write(PAYLOAD.subarray(0, sent));
    const timer = setInterval(() => {
      res.write(PAYLOAD.subarray(sent, sent + 10));
      sent += 10;
    }, 50);
    res.on('close', () => clearInterval(timer));
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    // The fast start counts for its own stretch of 500 ms only; the time
    // limit ends the attempt if nothing else does.
    const { stdout } = await capture(() =>
      download(asset(server, { sha256: sha(PAYLOAD) }), file, {
        ...fast,
        stallMs: 500,
        minBytesPerSecond: 1_000,
        timeoutMs: 3_000,
      }),
    );
    assert.equal(server.requests.length, 2);
    assert.match(stdout, /attempt 1 of 3 failed \(slower than 1000 bytes\/s for 500 ms\)/);
  } finally {
    await server.close();
  }
});

test('download sets no floor unless asked, so a dribble that keeps coming finishes', async () => {
  // 300 bytes at 200 bytes/s take 1.5 s, three times the stall timeout,
  // with data never more than 50 ms apart.
  const body = PAYLOAD.subarray(0, 300);
  const server = await serve((req, res) => {
    res.writeHead(200, { 'content-length': body.length });
    let sent = 0;
    const timer = setInterval(() => {
      res.write(body.subarray(sent, sent + 10));
      sent += 10;
      if (sent >= body.length) {
        clearInterval(timer);
        res.end();
      }
    }, 50);
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    await capture(() => download(asset(server, { sha256: sha(body) }), file, { ...fast, stallMs: 500 }));
    assert.equal(server.requests.length, 1);
    assert.deepEqual(readFileSync(file), body);
  } finally {
    await server.close();
  }
});

test('download lets a transfer above its floor run on', async () => {
  const chunks = 15;
  const size = Math.ceil(PAYLOAD.length / chunks);
  const server = await serve((req, res) => {
    res.writeHead(200, { 'content-length': PAYLOAD.length });
    let sent = 0;
    const timer = setInterval(() => {
      res.write(PAYLOAD.subarray(sent * size, (sent + 1) * size));
      sent += 1;
      if (sent === chunks) {
        clearInterval(timer);
        res.end();
      }
    }, 100);
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    // About 6,700 bytes/s for 1.5 s, three stretches of 500 ms, against a
    // floor of 1,000.
    await capture(() =>
      download(asset(server, { sha256: sha(PAYLOAD) }), file, { ...fast, stallMs: 500, minBytesPerSecond: 1_000 }),
    );
    assert.equal(server.requests.length, 1);
    assert.deepEqual(readFileSync(file), PAYLOAD);
  } finally {
    await server.close();
  }
});

test('download starts no attempt once retryForMs has passed', async () => {
  // Each answer is a 503 that takes 100 ms to come.
  const server = await serve((req, res) => {
    setTimeout(() => {
      res.statusCode = 503;
      res.end('busy');
    }, 100);
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    await capture(() =>
      assert.rejects(download(asset(server, { sha256: sha(PAYLOAD) }), file, { attempts: 8, delayMs: 1, retryForMs: 250 })),
    );
    // At most three attempts start within 250 ms, however busy the host, since
    // each answer takes 100 ms; all eight take about a second.
    assert.ok(server.requests.length < 8, `${server.requests.length} requests`);
    assert.deepEqual(readdirSync(join(file, '..')), []);
  } finally {
    await server.close();
  }
});

test('download with retryAll tries again what it would otherwise not', async () => {
  // A mirror that has not synced the file yet, then one that has.
  const server = await serve((req, res, n) => {
    if (n === 1) {
      res.statusCode = 404;
      res.end('Not Found');
      return;
    }
    res.end(PAYLOAD);
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    const { stdout } = await capture(() => download(asset(server, { sha256: sha(PAYLOAD) }), file, { ...fast, retryAll: true }));
    assert.equal(server.requests.length, 2);
    assert.match(stdout, /attempt 1 of 3 failed \(GET .*: 404 Not Found\)/);
    assert.deepEqual(readFileSync(file), PAYLOAD);
  } finally {
    await server.close();
  }
});

test('download does not try a 404 again', async () => {
  const server = await serve((req, res) => {
    res.statusCode = 404;
    res.end('Not Found');
  });
  const file = join(tempDir(), 'asset.bin');
  try {
    await capture(() =>
      assert.rejects(download(asset(server, { sha256: sha(PAYLOAD) }), file, fast), (err) => {
        assert.ok(err instanceof HttpError);
        assert.equal(err.status, 404);
        return true;
      }),
    );
    assert.equal(server.requests.length, 1);
    assert.equal(existsSync(file), false);
  } finally {
    await server.close();
  }
});

test('download takes what a git ref or a rolling link serves, and says it did not check it', async () => {
  const server = await serve((req, res) => res.end(PAYLOAD));
  const dir = tempDir();
  try {
    const byRef = await capture(() => download(asset(server, { ref: COMMIT }), join(dir, 'a'), fast));
    assert.match(byRef.stdout, new RegExp(`not checked: pinned by the commit in its URL, ${COMMIT}`));
    const rolling = await capture(() => download(asset(server, { rolling: true }), join(dir, 'b'), fast));
    assert.match(rolling.stdout, /not checked: a rolling download with no pin/);
    assert.deepEqual(readFileSync(join(dir, 'a')), PAYLOAD);
    assert.deepEqual(readFileSync(join(dir, 'b')), PAYLOAD);
  } finally {
    await server.close();
  }
});

test('download refuses a malformed pin before it fetches anything', async () => {
  const server = await serve((req, res) => res.end(PAYLOAD));
  try {
    await assert.rejects(download(asset(server, { ref: 'main' }), join(tempDir(), 'a'), fast), /not a commit's full SHA/);
    assert.equal(server.requests.length, 0);
  } finally {
    await server.close();
  }
});

// Assets `names` on `server`, each with `pin`.
function assets(server, names, pin) {
  return names.map((name) => asset(server, pin, name));
}

test('downloadAll downloads every asset into the directory, and says it did', async () => {
  const server = await serve((req, res) => res.end(`the ${basename(req.url)}`));
  const dir = join(tempDir(), 'sdk');
  try {
    const { value } = await capture(() => downloadAll(assets(server, ['a.zip', 'b.zip'], { rolling: true }), dir, { download: fast }));
    assert.equal(value, true);
    assert.deepEqual(readdirSync(dir).sort(), ['a.zip', 'b.zip']);
    assert.equal(readFileSync(join(dir, 'b.zip'), 'utf8'), 'the b.zip');
    assert.deepEqual(readdirSync(dirname(dir)), ['sdk']);
  } finally {
    await server.close();
  }
});

test('downloadAll takes what the cache restored when it holds every asset, and downloads again when not', async () => {
  const server = await serve((req, res) => res.end(`the ${basename(req.url)}`));
  const dir = tempDir();
  writeFileSync(join(dir, 'a.zip'), 'cached');
  try {
    const partial = await capture(() => downloadAll(assets(server, ['a.zip', 'b.zip'], { rolling: true }), dir, { cached: true, download: fast }));
    assert.equal(partial.value, true);
    assert.match(partial.stdout, /::warning::The cache restored .* without b\.zip; downloading again/);
    assert.equal(server.requests.length, 2);
    writeFileSync(join(dir, 'a.zip'), 'cached');
    const whole = await capture(() => downloadAll(assets(server, ['a.zip', 'b.zip'], { rolling: true }), dir, { cached: true, download: fast }));
    assert.equal(whole.value, false);
    assert.equal(server.requests.length, 2);
    assert.equal(readFileSync(join(dir, 'a.zip'), 'utf8'), 'cached');
  } finally {
    await server.close();
  }
});

test('downloadAll changes nothing in the directory unless every download passes', async () => {
  const server = await serve((req, res) => {
    res.statusCode = req.url === '/b.so' ? 404 : 200;
    res.end('new');
  });
  const dir = tempDir();
  writeFileSync(join(dir, 'a.so'), 'older');
  try {
    await assert.rejects(downloadAll(assets(server, ['a.so', 'b.so'], { ref: COMMIT }), dir, { download: fast }), HttpError);
    assert.deepEqual(readdirSync(dir), ['a.so']);
    assert.equal(readFileSync(join(dir, 'a.so'), 'utf8'), 'older');
  } finally {
    await server.close();
  }
});

test('downloadAll falls back on every rolling asset an earlier week left, when the download fails', async () => {
  const server = await serve((req, res) => {
    res.statusCode = 503;
    res.end();
  });
  const dir = tempDir();
  writeFileSync(join(dir, 'a.zip'), 'last week');
  writeFileSync(join(dir, 'b.zip'), 'last week');
  try {
    const { value, stdout } = await capture(() =>
      downloadAll(assets(server, ['a.zip', 'b.zip'], { rolling: true }), dir, { restored: 'install-sdk-a.zip-b.zip-week-2026-41', download: fast }),
    );
    assert.equal(value, false);
    assert.match(stdout, /::warning::.*503.*\. Installing the earlier download the cache restored instead, install-sdk-a\.zip-b\.zip-week-2026-41/);
    assert.equal(readFileSync(join(dir, 'a.zip'), 'utf8'), 'last week');
  } finally {
    await server.close();
  }
});

test('downloadAll falls back on nothing short of every asset, and never for a pinned one', async () => {
  const server = await serve((req, res) => {
    res.statusCode = 503;
    res.end();
  });
  const dir = tempDir();
  writeFileSync(join(dir, 'a.zip'), 'last week');
  try {
    await assert.rejects(downloadAll(assets(server, ['a.zip', 'b.zip'], { rolling: true }), dir, { download: fast }), HttpError);
    writeFileSync(join(dir, 'b.zip'), 'last week');
    const pinned = [asset(server, { rolling: true }, 'a.zip'), asset(server, { sha256: '1'.repeat(64) }, 'b.zip')];
    await assert.rejects(downloadAll(pinned, dir, { download: fast }), HttpError);
  } finally {
    await server.close();
  }
});

// --- Unpacking --------------------------------------------------------------------

test('tarProgram runs Windows its own bsdtar by its full path', async () => {
  await withEnv({ SystemRoot: 'C:\\Windows' }, async () => {
    assert.equal(tarProgram('win32'), 'C:\\Windows\\System32\\tar.exe');
  });
  assert.equal(tarProgram('linux'), 'tar');
  assert.equal(tarProgram('darwin'), 'tar');
});

for (const format of FORMATS) {
  test(`unpack reads a ${format} on ${process.platform}`, async () => {
    const dir = tempDir();
    const file = join(dir, `tool${format}`);
    writeFileSync(file, Buffer.from(ARCHIVES[format], 'base64'));
    await unpack(file, join(dir, 'out'));
    assert.equal(readFileSync(join(dir, 'out', 'tool-1.0', 'bin', 'tool'), 'utf8'), 'hello\n');
    assert.equal(readFileSync(join(dir, 'out', 'tool-1.0', 'README'), 'utf8'), 'notes\n');
  });
}

test('unpack reads a .tar.gz built in memory, as the action tests build theirs', async () => {
  const dir = tempDir();
  const file = join(dir, 'built.tar.gz');
  writeFileSync(file, gzipSync(tar([{ path: 'top/', type: '5', mode: 0o755 }, { path: 'top/file', data: 'x' }])));
  await unpack(file, join(dir, 'out'));
  assert.equal(readFileSync(join(dir, 'out', 'top', 'file'), 'utf8'), 'x');
});

// Each archive is cut inside its last file. bsdtar reads a zip as a stream
// of entries, so one that loses only its central directory, after the last
// entry's data (byte 195 here), still unpacks whole and passes; unzip
// refuses even that.
for (const [format, cut] of [
  ['.zip', 160],
  ['.tar.gz', 100],
]) {
  test(`unpack fails on a ${format} cut short`, async () => {
    const dir = tempDir();
    const file = join(dir, `cut${format}`);
    writeFileSync(file, Buffer.from(ARCHIVES[format], 'base64').subarray(0, cut));
    await assert.rejects(unpack(file, join(dir, 'out')));
  });
}

test('unpack refuses a file it has no tool for', async () => {
  await assert.rejects(unpack(join(tempDir(), 'tool.7z'), join(tempDir(), 'out')), {
    message: 'tool.7z: cannot unpack it; an archive is one of .tar.gz, .tgz, .tar.xz, .tar, .zip',
  });
});

test('unpackZips on Windows runs one Expand-Archive per zip in one PowerShell, by its full path, each path quoted whole', async () => {
  const { run, calls } = recorder();
  const program = await withEnv({ SystemRoot: 'C:\\Windows' }, async () => {
    const zips = [
      ["C:\\Users\\o'brien\\a\u2019b.zip", 'C:\\Temp\\a'],
      ['C:\\Temp\\b.zip', 'C:\\Temp\\b'],
    ];
    await unpackZips(zips, { platform: 'win32', run });
    await unpackZips([], { platform: 'win32', run });
    return powershellProgram();
  });
  assert.equal(program, 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe');
  assert.equal(calls.length, 1);
  const [called, ...args] = calls[0];
  assert.equal(called, program);
  assert.deepEqual(args.slice(0, 3), ['-NoProfile', '-NonInteractive', '-EncodedCommand']);
  assert.equal(
    Buffer.from(args[3], 'base64').toString('utf16le'),
    "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; try { " +
      "Expand-Archive -LiteralPath 'C:\\Users\\o''brien\\a\u2019\u2019b.zip' -DestinationPath 'C:\\Temp\\a' -Force; " +
      "Expand-Archive -LiteralPath 'C:\\Temp\\b.zip' -DestinationPath 'C:\\Temp\\b' -Force" +
      ' } catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }',
  );
});

test('onlyDirectory finds the one folder an archive unpacked into, past a __MACOSX folder', async () => {
  const dir = tempDir();
  mkdirSync(join(dir, 'tool-1.0'));
  assert.equal(await onlyDirectory(dir), join(dir, 'tool-1.0'));
  mkdirSync(join(dir, '__MACOSX'));
  assert.equal(await onlyDirectory(dir), join(dir, 'tool-1.0'));
});

test('onlyDirectory refuses an archive without exactly one folder at its top', async () => {
  const two = tempDir();
  mkdirSync(join(two, 'a'));
  mkdirSync(join(two, 'b'));
  await assert.rejects(onlyDirectory(two), { message: 'expected the archive to hold one directory at its top; it holds a, b' });
  const file = tempDir();
  writeFileSync(join(file, 'tool'), '');
  await assert.rejects(onlyDirectory(file), /it holds tool$/);
  await assert.rejects(onlyDirectory(tempDir()), /it holds nothing$/);
});

// --- Placing the install ------------------------------------------------------------

test('withWorkDir gives fn an empty directory beside dir, and removes it after', async () => {
  const parent = tempDir();
  const dir = join(parent, 'nested', 'tool');
  const listening = () => ['SIGINT', 'SIGTERM'].map((signal) => process.listenerCount(signal));
  const before = listening();
  let seen;
  const value = await withWorkDir(dir, async (work) => {
    seen = work;
    assert.equal(dirname(work), join(parent, 'nested'));
    assert.match(basename(work), /^\.tool-download-/);
    assert.deepEqual(readdirSync(work), []);
    writeFileSync(join(work, 'file'), '');
    return 'done';
  });
  assert.equal(value, 'done');
  assert.equal(existsSync(seen), false);
  assert.deepEqual(readdirSync(join(parent, 'nested')), []);
  assert.deepEqual(listening(), before, 'withWorkDir leaves its signal listeners behind');
});

test('withWorkDir removes its directory when fn fails', async () => {
  const parent = tempDir();
  await assert.rejects(
    withWorkDir(join(parent, 'tool'), async (work) => {
      writeFileSync(join(work, 'file'), '');
      throw new Error('boom');
    }),
    { message: 'boom' },
  );
  assert.deepEqual(readdirSync(parent), []);
});

// Runs withWorkDir in a process of its own, which prints the work directory
// and waits; ends it with `signal`, and resolves to the directory and how the
// process ended.
async function interruptWorkDir(dir, signal) {
  const script = [
    `import { withWorkDir } from ${JSON.stringify(new URL('./install.mjs', import.meta.url).href)};`,
    `await withWorkDir(${JSON.stringify(dir)}, async (work) => {`,
    "  process.stdout.write(`${work}\\n`);",
    '  await new Promise(() => setInterval(() => {}, 1000));',
    '});',
  ].join('\n');
  const child = spawn(process.execPath, ['--input-type=module', '--eval', script], { stdio: ['ignore', 'pipe', 'inherit'] });
  let out = '';
  for await (const chunk of child.stdout) {
    out += chunk;
    if (out.endsWith('\n')) break;
  }
  const work = out.trim();
  assert.equal(existsSync(work), true, work);
  child.kill(signal);
  const [code, ended] = await once(child, 'exit');
  return { work, code, ended };
}

for (const signal of ['SIGINT', 'SIGTERM']) {
  test(`withWorkDir removes its directory when a ${signal} ends the process`, {
    skip: process.platform === 'win32' && 'Windows has no signal one process can deliver to another',
  }, async () => {
    const parent = tempDir();
    const { work, code, ended } = await interruptWorkDir(join(parent, 'tool'), signal);
    assert.equal(ended, signal, `exit code ${code}`);
    assert.equal(existsSync(work), false);
    assert.deepEqual(readdirSync(parent), []);
  });
}

test('checkReplaceable takes a missing or empty directory, or one an earlier install marked', async () => {
  const dir = tempDir();
  await checkReplaceable(join(dir, 'missing'));
  await checkReplaceable(dir);
  writeFileSync(join(dir, 'tool'), '');
  await markInstalled(dir, 'Tool 1.0');
  assert.equal(readFileSync(join(dir, INSTALLED), 'utf8'), 'Tool 1.0\n');
  await checkReplaceable(dir);
});

test('checkReplaceable refuses a directory no install of ours marked, even one holding the tool', async () => {
  const dir = tempDir();
  writeFileSync(join(dir, 'tool'), '');
  writeFileSync(join(dir, 'notes.txt'), '');
  await assert.rejects(checkReplaceable(dir), {
    message:
      `${dir} holds files and no .installed-by-rusty-photon, so no earlier install of ours made it, ` +
      'and it is not ours to replace; choose another directory',
  });
  await assert.rejects(checkReplaceable(join(dir, 'notes.txt')), /notes\.txt/);
});

test('makeRunnable lets a binary run, and leaves a Windows .exe as it is', { skip: process.platform === 'win32' && 'Windows has no mode bits' }, async () => {
  const dir = tempDir();
  writeFileSync(join(dir, 'tool'), 'binary', { mode: 0o644 });
  writeFileSync(join(dir, 'tool.exe'), 'binary', { mode: 0o644 });
  await makeRunnable(dir, 'tool');
  await makeRunnable(dir, 'tool.exe');
  assert.equal(statSync(join(dir, 'tool')).mode & 0o777, 0o755);
  assert.equal(statSync(join(dir, 'tool.exe')).mode & 0o777, 0o644);
});

// --- Libraries a build links ---------------------------------------------------

test('findRuntime reads the loader\'s cache, and the usual directories when that fails', async () => {
  const listing =
    '1234 libs found in cache `/etc/ld.so.cache\'\n' +
    '\tlibusb-1.0.so.0 (libc6,AArch64) => /lib/aarch64-linux-gnu/libusb-1.0.so.0\n' +
    '\tlibudev.so.1 (libc6,AArch64) => /lib/aarch64-linux-gnu/libudev.so.1\n';
  const cache = recorder({ ldconfig: listing });
  assert.equal(await findRuntime('libudev.so.1', { run: cache.run, dirs: [] }), '/lib/aarch64-linux-gnu/libudev.so.1');
  assert.deepEqual(cache.calls, [['ldconfig', '-p']]);

  const dirs = [tempDir(), tempDir()];
  writeFileSync(join(dirs[1], 'libudev.so.1'), 'libudev');
  const none = async () => {
    throw new Error('spawn ldconfig ENOENT');
  };
  assert.equal(await findRuntime('libudev.so.1', { run: none, dirs }), join(dirs[1], 'libudev.so.1'));
  assert.equal(await findRuntime('libudev.so.1', { run: recorder({ ldconfig: listing.slice(0, 40) }).run, dirs }), join(dirs[1], 'libudev.so.1'));
  assert.equal(await findRuntime('libusb-1.0.so.0', { run: none, dirs }), undefined);
});

test('loaderFinds says whether the loader\'s cache lists every file in the directory a build linked', async () => {
  const listing = (dir) =>
    `2 libs found in cache\n\tlibone.so (libc6,x86-64) => ${join(dir, 'libone.so')}\n\tlibtwo.so (libc6,x86-64) => ${join(dir, 'libtwo.so')}\n`;
  const lib = join('/', 'usr', 'local', 'lib');
  assert.equal(await loaderFinds(lib, ['libone.so', 'libtwo.so'], { run: recorder({ ldconfig: listing(lib) }).run }), true);
  assert.equal(await loaderFinds(lib, ['libone.so', 'libthree.so'], { run: recorder({ ldconfig: listing(lib) }).run }), false);
  assert.equal(await loaderFinds(lib, ['libone.so'], { run: recorder({ ldconfig: listing(join('/', 'opt', 'lib')) }).run }), false);
});

test('linkRuntime links each linker\'s name to the runtime library, replacing a link there', {
  skip: process.platform === 'win32' && 'the runners without sudo are Linux',
}, async () => {
  const lib = tempDir();
  const system = tempDir();
  writeFileSync(join(system, 'libusb-1.0.so.0'), 'libusb');
  writeFileSync(join(lib, 'libusb-1.0.so'), 'an older link');
  const find = async (soname) => join(system, soname);
  const { stdout } = await capture(() => linkRuntime(lib, [['libusb-1.0.so.0', 'libusb-1.0.so', 'libusb-1.0-0']], { find }));
  assert.ok(lstatSync(join(lib, 'libusb-1.0.so')).isSymbolicLink());
  assert.equal(readlinkSync(join(lib, 'libusb-1.0.so')), join(system, 'libusb-1.0.so.0'));
  assert.equal(stdout, `libusb-1.0.so -> ${join(system, 'libusb-1.0.so.0')}\n`);
});

test('linkRuntime names the package to install when a runtime library is missing', async () => {
  const lib = tempDir();
  await assert.rejects(
    linkRuntime(lib, [['libudev.so.1', 'libudev.so', 'libudev1']], { find: async () => undefined }),
    /the system library libudev\.so\.1 is not on this machine\. .*sudo apt-get install -y libudev1$/,
  );
  assert.deepEqual(readdirSync(lib), []);
});

test('canWrite asks of the nearest directory there is, and says no to one this user cannot write', {
  skip: (process.platform === 'win32' || process.getuid?.() === 0) && 'mode bits deny root and Windows nothing',
}, async () => {
  const dir = tempDir();
  assert.equal(await canWrite(join(dir, 'not', 'there', 'yet')), true);
  const locked = join(dir, 'locked');
  mkdirSync(locked);
  chmodSync(locked, 0o555);
  try {
    assert.equal(await canWrite(locked), false);
    assert.equal(await canWrite(join(locked, 'lib')), false);
  } finally {
    chmodSync(locked, 0o755);
  }
});
