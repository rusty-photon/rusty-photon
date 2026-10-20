import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { capture } from '../lib/testing.mjs';
import { answer, bazelrc, cacheHost, CLOUD_CACHE, configure, launcher, RC_FILE, venue } from './remote.mjs';

// --- Fixtures -----------------------------------------------------------------

const GRPC = 'grpc://10.1.2.3:9092';
const LAN = 'http://10.1.2.3:8080';
const AUTH = 'Basic Y2k6cHc=';
const TOKEN = 'tok';
const HELPER = fileURLToPath(new URL('./helper.mjs', import.meta.url));

// What Bazel asks about: a gRPC call's service URI on the LAN host, and the
// HTTP caches' own URLs.
const GRPC_CALL = 'https://10.1.2.3:9092/build.bazel.remote.execution.v2.ContentAddressableStorage';

// The pool's push-to-main job holds both credentials, as the env blocks set
// CACHE_WRITE_TOKEN on push for every venue.
const POOL_PUSH = { RP_LAN_CACHE_URL: LAN, RP_LAN_CACHE_GRPC_URL: GRPC, LAN_CACHE_WRITE_AUTH: AUTH, CACHE_WRITE_TOKEN: TOKEN };
const POOL_PR = { RP_LAN_CACHE_URL: LAN, RP_LAN_CACHE_GRPC_URL: GRPC, LAN_CACHE_WRITE_AUTH: '', CACHE_WRITE_TOKEN: '' };

function tempDir() {
  return mkdtempSync(join(process.env.TEST_TMPDIR ?? tmpdir(), 'remote-cache-'));
}

// Runs `cmd args` with `input` on stdin, resolving to its exit code and output.
function runWith(cmd, args, input, options = {}) {
  return new Promise((resolve, reject) => {
    const child = spawn(cmd, args, { stdio: ['pipe', 'pipe', 'pipe'], ...options });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (d) => (stdout += d));
    child.stderr.on('data', (d) => (stderr += d));
    child.on('error', reject);
    child.on('close', (code) => resolve({ code, stdout, stderr }));
    child.stdin.end(input);
  });
}

// --- venue --------------------------------------------------------------------

test('the pool with the Remote Asset API writes over gRPC with the LAN credential, not the cloud token', () => {
  assert.deepEqual(venue(POOL_PUSH), {
    flags: [
      '--config=remote-cache-base',
      `--remote_cache=${GRPC}`,
      `--experimental_remote_downloader=${GRPC}`,
      '--experimental_remote_downloader_local_fallback',
    ],
    host: '10.1.2.3',
    lan: true,
    authorization: AUTH,
    what: "the LAN bazel-remote over gRPC, with its Remote Asset API for Bazel's downloads",
  });
});

test("the pool's pull requests read over gRPC and upload nothing", () => {
  const { flags, authorization } = venue(POOL_PR);
  assert.deepEqual(flags, [
    '--config=remote-cache-base',
    `--remote_cache=${GRPC}`,
    `--experimental_remote_downloader=${GRPC}`,
    '--experimental_remote_downloader_local_fallback',
    '--remote_upload_local_results=false',
  ]);
  assert.equal(authorization, '');
});

test('the pool without the Remote Asset API uses the HTTP cache, without its trailing slashes', () => {
  const push = venue({ RP_LAN_CACHE_URL: `${LAN}//`, LAN_CACHE_WRITE_AUTH: AUTH, CACHE_WRITE_TOKEN: TOKEN });
  assert.deepEqual(push.flags, ['--config=remote-cache-base', `--remote_cache=${LAN}`]);
  assert.equal(push.host, '10.1.2.3');
  assert.equal(push.authorization, AUTH);
  assert.equal(push.what, 'the LAN bazel-remote over HTTP');

  const pr = venue({ RP_LAN_CACHE_URL: LAN });
  assert.deepEqual(pr.flags, ['--config=remote-cache-base', `--remote_cache=${LAN}`, '--remote_upload_local_results=false']);
  assert.equal(pr.authorization, '');
});

test('a hosted runner writes the cloud cache with the Bearer token, not the LAN credential', () => {
  const push = venue({ LAN_CACHE_WRITE_AUTH: AUTH, CACHE_WRITE_TOKEN: TOKEN });
  assert.deepEqual(push, {
    flags: ['--config=remote-cache'],
    host: 'cache.rustyphoton.space',
    lan: false,
    authorization: `Bearer ${TOKEN}`,
    what: `the cloud cache, ${CLOUD_CACHE}`,
  });
});

test('a hosted pull request reads the cloud cache and uploads nothing', () => {
  for (const env of [{}, { CACHE_WRITE_TOKEN: '', LAN_CACHE_WRITE_AUTH: '', RP_LAN_CACHE_URL: '' }]) {
    const { flags, authorization } = venue(env);
    assert.deepEqual(flags, ['--config=remote-cache', '--remote_upload_local_results=false']);
    assert.equal(authorization, '');
  }
});

test("the cloud cache is the one .bazelrc's remote-cache config names, beside remote-cache-base's settings", () => {
  const rc = readFileSync(fileURLToPath(new URL('../../../.bazelrc', import.meta.url)), 'utf8');
  assert.match(rc, new RegExp(`^build:remote-cache --remote_cache=${CLOUD_CACHE.replaceAll('.', '\\.')}$`, 'm'));
  assert.match(rc, /^build:remote-cache --config=remote-cache-base$/m);
  assert.doesNotMatch(rc, /^build:remote-cache-base --remote_cache=/m);
});

test('.bazelrc imports the file configure writes', () => {
  const rc = readFileSync(fileURLToPath(new URL('../../../.bazelrc', import.meta.url)), 'utf8');
  assert.match(rc, new RegExp(`^try-import %workspace%/${RC_FILE.replaceAll('.', '\\.')}$`, 'm'));
});

// --- cacheHost ----------------------------------------------------------------

test('cacheHost reads the host as java.net.URI does, in its own case, without the port', () => {
  assert.equal(cacheHost('http://NAS.Lan:8080/', 'X'), 'NAS.Lan');
  assert.equal(cacheHost('grpc://10.1.2.3:9092', 'X'), '10.1.2.3');
  assert.equal(cacheHost('https://cache.rustyphoton.space', 'X'), 'cache.rustyphoton.space');
  assert.equal(cacheHost('https://h.example/cas/abc?x=1', 'X'), 'h.example');
  assert.equal(cacheHost('https://h.example?x', 'X'), 'h.example');
});

test('cacheHost refuses a host a scope cannot name, without showing the URL', () => {
  for (const url of ['http://ci:pw@10.1.2.3:8080', 'grpc://[fd00::1]:9092', '10.1.2.3:9092', 'http://', 'http://h_x.lan']) {
    assert.throws(
      () => cacheHost(url, 'RP_LAN_CACHE_URL'),
      (err) => err.message.startsWith('RP_LAN_CACHE_URL needs a host name') && !err.message.includes('10.1.2.3'),
      url,
    );
  }
});

test('venue refuses a LAN URL with a user, naming the variable', () => {
  assert.throws(() => venue({ RP_LAN_CACHE_URL: 'http://ci:pw@10.1.2.3:8080' }), /^Error: RP_LAN_CACHE_URL needs/);
  assert.throws(() => venue({ RP_LAN_CACHE_GRPC_URL: 'grpc://[fd00::1]:9092' }), /^Error: RP_LAN_CACHE_GRPC_URL needs/);
});

// --- bazelrc and launcher -----------------------------------------------------

test('bazelrc puts each flag on a ci-remote line of its own', () => {
  assert.equal(
    bazelrc(['--config=remote-cache', '--remote_upload_local_results=false']),
    "# This job's remote cache, written by .github/actions/remote-cache. Not checked in.\n" +
      'build:ci-remote --config=remote-cache\n' +
      'build:ci-remote --remote_upload_local_results=false\n',
  );
});

test('bazelrc refuses a flag a bazelrc line would split or cut, without showing it', () => {
  for (const flag of ['--a=b c', '--a=b\tc', '--a=#b', "--a='b'", '--a="b"', '--a=C:\\b']) {
    assert.throws(
      () => bazelrc(['--config=remote-cache', flag]),
      (err) => err.message.includes('cannot carry') && !err.message.includes('--a='),
      flag,
    );
  }
});

test('launcher runs the helper on the Node it names, passing its arguments on', () => {
  assert.deepEqual(launcher('/opt/node/bin/node', '/w/helper.mjs', 'linux'), {
    name: 'bazel-credential-helper',
    text: "#!/bin/sh\nexec '/opt/node/bin/node' '/w/helper.mjs' \"$@\"\n",
  });
  assert.deepEqual(launcher('C:\\r\\node.exe', 'D:\\w\\helper.mjs', 'win32'), {
    name: 'bazel-credential-helper.cmd',
    text: '@"C:\\r\\node.exe" "D:\\w\\helper.mjs" %*\r\n',
  });
});

test('launcher refuses a path its script cannot quote', () => {
  assert.throws(() => launcher("/o'p/node", '/w/helper.mjs', 'linux'), /cannot quote/);
  assert.throws(() => launcher('/opt/node', "/w'/helper.mjs", 'darwin'), /cannot quote/);
  assert.throws(() => launcher('C:\\r"\\node.exe', 'D:\\w\\helper.mjs', 'win32'), /cannot quote/);
  assert.throws(() => launcher('C:\\r\\node.exe', 'D:\\%w%\\helper.mjs', 'win32'), /cannot quote/);
});

// --- answer -------------------------------------------------------------------

test("answer gives the LAN credential for the LAN cache's host alone", () => {
  const request = (uri) => JSON.stringify({ uri });
  assert.deepEqual(answer(['get'], request(GRPC_CALL), POOL_PUSH), { headers: { Authorization: [AUTH] } });
  assert.deepEqual(answer(['get'], request(`${GRPC}`), POOL_PUSH), { headers: { Authorization: [AUTH] } });
  for (const uri of [`${CLOUD_CACHE}/cas/abc`, 'https://github.com/o/r/archive/x.tar.gz', 'https://10.1.2.30:9092/x']) {
    assert.deepEqual(answer(['get'], request(uri), POOL_PUSH), {}, uri);
  }
});

test("answer gives the Bearer token for the cloud cache's host alone", () => {
  const env = { CACHE_WRITE_TOKEN: TOKEN, LAN_CACHE_WRITE_AUTH: AUTH };
  assert.deepEqual(answer(['get'], JSON.stringify({ uri: CLOUD_CACHE }), env), {
    headers: { Authorization: [`Bearer ${TOKEN}`] },
  });
  assert.deepEqual(answer(['get'], JSON.stringify({ uri: GRPC_CALL }), env), {});
});

test('answer gives no headers to a job with no credential, or for a request with no URI it can read', () => {
  assert.deepEqual(answer(['get'], JSON.stringify({ uri: GRPC_CALL }), POOL_PR), {});
  assert.deepEqual(answer(['get'], JSON.stringify({ uri: CLOUD_CACHE }), {}), {});
  for (const input of ['{}', '{"uri":7}', '{"uri":"not a url"}', 'null']) {
    assert.deepEqual(answer(['get'], input, POOL_PUSH), {}, input);
  }
});

test('answer refuses another command, and a request that is not JSON, without showing it', () => {
  assert.throws(() => answer(['store'], '{}', POOL_PUSH), /takes the command get, not "store"/);
  assert.throws(() => answer([], '{}', POOL_PUSH), /not ""/);
  assert.throws(
    () => answer(['get'], `{"uri":"${GRPC_CALL}"`, POOL_PUSH),
    (err) => err.message === 'The request on stdin is not JSON',
  );
});

// --- configure ----------------------------------------------------------------

async function configured(env) {
  const workspace = tempDir();
  const runnerTemp = tempDir();
  const { stdout } = await capture(() =>
    configure({ env, workspace, runnerTemp, node: process.execPath, helper: HELPER }),
  );
  const name = process.platform === 'win32' ? 'bazel-credential-helper.cmd' : 'bazel-credential-helper';
  const launcherPath = join(runnerTemp, name);
  return { stdout, launcherPath, rc: readFileSync(join(workspace, RC_FILE), 'utf8') };
}

test('configure masks the LAN address first, then writes the config and the launcher', async () => {
  const { stdout, launcherPath, rc } = await configured(POOL_PUSH);
  const lines = stdout.split('\n');
  const summary = lines.findIndex((l) => l.startsWith('Remote cache:'));
  for (const mask of [LAN, '10.1.2.3:8080', '10.1.2.3', GRPC, '10.1.2.3:9092']) {
    const at = lines.indexOf(`::add-mask::${mask}`);
    assert.ok(at >= 0 && at < summary, `${mask} is not masked before the summary`);
  }
  assert.equal(
    lines[summary],
    "Remote cache: the LAN bazel-remote over gRPC, with its Remote Asset API for Bazel's downloads. This job reads and writes it.",
  );
  assert.ok(!stdout.includes(AUTH), 'the credential is printed');

  assert.equal(
    rc,
    bazelrc([
      '--config=remote-cache-base',
      `--remote_cache=${GRPC}`,
      `--experimental_remote_downloader=${GRPC}`,
      '--experimental_remote_downloader_local_fallback',
      `--credential_helper=10.1.2.3=${launcherPath.replaceAll('\\', '/')}`,
    ]),
  );
  assert.ok(!rc.includes(AUTH), 'the config holds the credential');
  assert.equal(readFileSync(launcherPath, 'utf8'), launcher(process.execPath, HELPER, process.platform).text);
  if (process.platform !== 'win32') assert.equal(statSync(launcherPath).mode & 0o777, 0o755);
});

test("configure on a hosted runner masks nothing, and still configures the helper for a job that only reads", async () => {
  const { stdout, launcherPath, rc } = await configured({});
  assert.ok(!stdout.includes('::add-mask::'), stdout);
  assert.equal(stdout, 'Remote cache: the cloud cache, https://cache.rustyphoton.space. This job reads it only, with no write credential.\n');
  assert.equal(
    rc,
    bazelrc([
      '--config=remote-cache',
      '--remote_upload_local_results=false',
      `--credential_helper=cache.rustyphoton.space=${launcherPath.replaceAll('\\', '/')}`,
    ]),
  );
});

test('configure masks the LAN host in its own case too', async () => {
  const { stdout } = await configured({ RP_LAN_CACHE_URL: 'http://NAS.Lan:8080', LAN_CACHE_WRITE_AUTH: AUTH });
  assert.ok(stdout.split('\n').includes('::add-mask::NAS.Lan'), stdout);
});

test('configure refuses a LAN URL that does not parse, before writing anything, without showing it', async () => {
  const workspace = tempDir();
  await assert.rejects(
    capture(() =>
      configure({ env: { RP_LAN_CACHE_URL: 'not a url' }, workspace, runnerTemp: tempDir(), node: 'n', helper: HELPER }),
    ),
    (err) => err.message.includes('does not parse') && !err.message.includes('not a url'),
  );
  assert.throws(() => statSync(join(workspace, RC_FILE)), { code: 'ENOENT' });
});

// --- The helper, run as Bazel runs it ------------------------------------------

// Starts the launcher with `get` and the request on stdin, as Bazel does:
// directly where it is a script, and on Windows through cmd.exe, as
// CreateProcess runs a .cmd file.
function runLauncher(launcherPath, input, env) {
  if (process.platform === 'win32') {
    return runWith(process.env.ComSpec ?? 'cmd.exe', ['/d', '/s', '/c', `""${launcherPath}" get"`], input, {
      env,
      windowsVerbatimArguments: true,
    });
  }
  return runWith(launcherPath, ['get'], input, { env });
}

test('the launcher answers Bazel with the credential for the cache, and no headers for another host', async () => {
  const { launcherPath } = await configured(POOL_PUSH);
  const env = { ...process.env, ...POOL_PUSH };
  const cache = await runLauncher(launcherPath, JSON.stringify({ uri: GRPC_CALL }), env);
  assert.deepEqual(cache, { code: 0, stdout: JSON.stringify({ headers: { Authorization: [AUTH] } }), stderr: '' });
  const other = await runLauncher(launcherPath, JSON.stringify({ uri: 'https://github.com/x' }), env);
  assert.deepEqual(other, { code: 0, stdout: '{}', stderr: '' });
});

test('the launcher answers a job with no credential with no headers', async () => {
  const { launcherPath } = await configured(POOL_PR);
  const env = { ...process.env, ...POOL_PR };
  assert.deepEqual(await runLauncher(launcherPath, JSON.stringify({ uri: GRPC_CALL }), env), {
    code: 0,
    stdout: '{}',
    stderr: '',
  });
});

test('the helper fails with a message and no answer on a request it cannot read', async () => {
  const env = { ...process.env, ...POOL_PUSH };
  const bad = await runWith(process.execPath, [HELPER, 'get'], '{"uri":', { env });
  assert.deepEqual(bad, { code: 1, stdout: '', stderr: 'remote-cache credential helper: The request on stdin is not JSON\n' });
  const store = await runWith(process.execPath, [HELPER, 'store'], '{}', { env });
  assert.equal(store.code, 1);
  assert.equal(store.stdout, '');
  assert.match(store.stderr, /takes the command get, not "store"/);
});
