import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { gzipSync } from 'node:zlib';

import { HttpError } from './helpers.mjs';
import {
  cacheKey,
  ChecksumError,
  download,
  forRunner,
  isoWeek,
  onlyDirectory,
  pinKey,
  runnerKey,
  tarProgram,
  unpack,
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

// --- Downloading ------------------------------------------------------------------

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

test('unpack fails on a damaged archive', async () => {
  const dir = tempDir();
  const file = join(dir, 'cut.zip');
  writeFileSync(file, Buffer.from(ARCHIVES['.zip'], 'base64').subarray(0, 200));
  await assert.rejects(unpack(file, join(dir, 'out')));
});

test('unpack refuses a file it has no tool for', async () => {
  await assert.rejects(unpack(join(tempDir(), 'tool.7z'), join(tempDir(), 'out')), {
    message: 'tool.7z: cannot unpack it; an archive is one of .tar.gz, .tgz, .tar.xz, .tar, .zip',
  });
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
