import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { DEBIAN, FEDORA_CLIENT, FEDORA_CLIENT_IMAGE, architectures, aptSource, dnfRepo, serveDirectory, verifyRepo } from './verify-repo.mjs';

function tempDir() {
  return mkdtempSync(path.join(process.env.TEST_TMPDIR ?? tmpdir(), 'verify-repo-'));
}

// A rendered site: the public key, both apt architectures' indices and both
// dnf repositories, or only those named.
function site({ deb = ['amd64', 'arm64'], rpm = ['x86_64', 'aarch64'] } = {}) {
  const dir = tempDir();
  const root = path.join(dir, 'site');
  mkdirSync(root);
  writeFileSync(path.join(root, 'pubkey.asc'), 'PUBLIC KEY');
  for (const arch of deb) {
    const binary = path.join(root, 'deb', 'dists', 'nightly', 'main', `binary-${arch}`);
    mkdirSync(binary, { recursive: true });
    writeFileSync(path.join(binary, 'Packages.gz'), 'index');
  }
  if (deb.length) writeFileSync(path.join(root, 'deb', 'dists', 'nightly', 'InRelease'), 'SIGNED RELEASE');
  for (const arch of rpm) {
    mkdirSync(path.join(root, 'rpm', arch, 'repodata'), { recursive: true });
  }
  return { root, temp: dir };
}

const PID = process.pid;

/**
 * podman, as the clients would answer: apt-get update reads InRelease from
 * the site through the source line it was given, dpkg-query says the
 * package is installed, a download leaves its file in /tmp. `answer` maps a
 * line's start to what it gives instead, or to an Error it rejects with.
 * Records each line, and what the mounted files held when the container
 * started.
 */
function fakePodman(answer = {}) {
  const lines = [];
  const mounted = {};
  let downloaded = '';
  const run = async (program, args) => {
    assert.equal(program, 'podman');
    const line = args.join(' ');
    lines.push(line);
    const override = Object.entries(answer).find(([start]) => line.startsWith(start));
    if (override) {
      if (override[1] instanceof Error) throw override[1];
      return override[1];
    }
    if (args[0] === 'run') {
      for (let i = args.indexOf('-v'); i !== -1; i = args.indexOf('-v', i + 1)) {
        // host:inside:ro,Z, where the host path may start with a drive letter.
        const spec = args[i + 1].replace(/:ro,Z$/, '');
        const at = spec.lastIndexOf(':/');
        const [host, inside] = [spec.slice(0, at), spec.slice(at + 1)];
        const file = inside.endsWith('keyrings') ? path.join(host, 'rusty-photon.asc') : host;
        mounted[inside] = readFileSync(file, 'utf8');
      }
      return 'container-id\n';
    }
    if (args.includes('update') && args.includes('apt-get')) {
      const base = mounted['/etc/apt/sources.list.d/rusty-photon-nightly.list'].split(' ')[2];
      const release = await fetch(`${base}/dists/nightly/InRelease`);
      assert.equal(await release.text(), 'SIGNED RELEASE');
      return '';
    }
    if (args.includes('dpkg-query')) return 'install ok installed 0.1.0+nightly.202610221006.g8c9e489';
    if (args.includes('rpm')) return '0.1.0^202610221006.g8c9e489';
    if (args.includes('download')) {
      const target = args.at(-1);
      downloaded = target.includes(':')
        ? `rusty-photon-sentinel_0.1.0+nightly.202610221006.g8c9e489_${target.split(':')[1]}.deb`
        : `rusty-photon-sentinel-0.1.0^202610221006.g8c9e489-1.${args[args.indexOf('--forcearch') + 1]}.rpm`;
      return '';
    }
    if (args.includes('ls')) return `${downloaded}\n`;
    return '';
  };
  return { run, lines, mounted };
}

// The lines, with `/` between folders, as on Windows too, and the work
// directory shortened to <work>.
function shorten(lines, temp) {
  const slashed = temp.replaceAll('\\', '/');
  const work = new RegExp(`${slashed.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}/repo-verify-[^/ :]+`, 'g');
  return lines.map((line) => line.replaceAll('\\', '/').replaceAll(work, '<work>'));
}

test('installs from both repositories as a client with only the public key, and downloads the other architecture', async () => {
  const { root, temp } = site();
  const { run, lines, mounted } = fakePodman();
  const { stdout } = await capture(() => verifyRepo({ site: root, arch: 'x64', temp, run }));
  const apt = `rusty-photon-repo-verify-${PID}-apt`;
  const dnf = `rusty-photon-repo-verify-${PID}-dnf`;
  assert.deepEqual(shorten(lines, temp), [
    `run -d --rm --network host --name ${apt} -v <work>/apt-keyrings:/etc/apt/keyrings:ro,Z ` +
      `-v <work>/rusty-photon-nightly.list:/etc/apt/sources.list.d/rusty-photon-nightly.list:ro,Z ${DEBIAN} sleep infinity`,
    `exec -e DEBIAN_FRONTEND=noninteractive ${apt} apt-get update`,
    `exec -e DEBIAN_FRONTEND=noninteractive ${apt} apt-get install -y rusty-photon-sentinel`,
    `exec ${apt} dpkg-query -W -f \${Status} \${Version} rusty-photon-sentinel`,
    `exec ${apt} dpkg --add-architecture arm64`,
    `exec -e DEBIAN_FRONTEND=noninteractive ${apt} apt-get update`,
    `exec -w /tmp ${apt} apt-get download rusty-photon-sentinel:arm64`,
    `exec ${apt} ls /tmp`,
    `rm -f ${apt}`,
    `build -q -t ${FEDORA_CLIENT_IMAGE} -f <work>/fedora-client/Containerfile <work>/fedora-client`,
    `run -d --rm --network host --name ${dnf} -v <work>/rusty-photon-nightly.repo:/etc/yum.repos.d/rusty-photon-nightly.repo:ro,Z ` +
      `${FEDORA_CLIENT_IMAGE} sleep infinity`,
    `exec ${dnf} dnf -y install rusty-photon-sentinel`,
    `exec ${dnf} rpm -q --qf %{VERSION} rusty-photon-sentinel`,
    `exec -w /tmp ${dnf} dnf -y download --forcearch aarch64 rusty-photon-sentinel`,
    `exec ${dnf} ls /tmp`,
    `rm -f ${dnf}`,
  ]);
  const base = mounted['/etc/apt/sources.list.d/rusty-photon-nightly.list'].split(' ')[2].replace(/\/deb$/, '');
  assert.match(base, /^http:\/\/127\.0\.0\.1:\d+$/);
  assert.equal(mounted['/etc/apt/keyrings'], 'PUBLIC KEY');
  assert.equal(mounted['/etc/yum.repos.d/rusty-photon-nightly.repo'], dnfRepo(base));
  assert.match(stdout, /apt: installed 0\.1\.0\+nightly\.202610221006\.g8c9e489 \(amd64\)/);
  assert.match(stdout, /dnf: aarch64 resolved, and its download checksum-verified/);
  // The server and the work directory are gone.
  await assert.rejects(fetch(`${base}/pubkey.asc`));
});

test('verifies from an arm64 machine the other way round, and only the trees and architectures the site has', async () => {
  const { root, temp } = site({ deb: ['arm64'], rpm: [] });
  const { run, lines } = fakePodman();
  const { stdout } = await capture(() => verifyRepo({ site: root, service: 'rp', arch: 'arm64', temp, run }));
  const apt = `rusty-photon-repo-verify-${PID}-apt`;
  assert.deepEqual(shorten(lines, temp).slice(1), [
    `exec -e DEBIAN_FRONTEND=noninteractive ${apt} apt-get update`,
    `exec -e DEBIAN_FRONTEND=noninteractive ${apt} apt-get install -y rusty-photon-rp`,
    `exec ${apt} dpkg-query -W -f \${Status} \${Version} rusty-photon-rp`,
    `rm -f ${apt}`,
  ]);
  assert.match(stdout, /== no .*\/rpm tree: skipped/);
  assert.throws(() => architectures('ia32'), { message: 'unsupported architecture ia32 (need x64 or arm64)' });
});

test("downloads no other architecture's package when the site has only this machine's", async () => {
  const { root, temp } = site({ deb: ['amd64'], rpm: ['x86_64'] });
  const { run, lines } = fakePodman();
  await capture(() => verifyRepo({ site: root, arch: 'x64', temp, run }));
  assert.ok(lines.some((line) => line.includes('dnf -y install')), lines.join('\n'));
  assert.ok(!lines.some((line) => line.includes('download') || line.includes('--add-architecture')), lines.join('\n'));
});

test('fails on a package that did not install or download, and removes the container all the same', async () => {
  const cases = [
    [{ [`exec rusty-photon-repo-verify-${PID}-apt dpkg-query`]: 'deinstall ok config-files 0.1.0' }, 'rusty-photon-sentinel not installed (dpkg says: deinstall ok config-files 0.1.0)'],
    [{ [`exec rusty-photon-repo-verify-${PID}-apt ls`]: 'other.deb\n' }, 'apt download of rusty-photon-sentinel:arm64 left no rusty-photon-sentinel_*_arm64.deb'],
    [{ [`exec rusty-photon-repo-verify-${PID}-dnf ls`]: '\n' }, 'dnf download of rusty-photon-sentinel for aarch64 left no rusty-photon-sentinel-*.aarch64.rpm'],
    [{ [`exec rusty-photon-repo-verify-${PID}-dnf dnf -y install`]: new Error('podman exited with 1: No match for argument') }, 'podman exited with 1: No match for argument'],
  ];
  for (const [answer, message] of cases) {
    const { root, temp } = site();
    const { run, lines } = fakePodman(answer);
    await assert.rejects(capture(() => verifyRepo({ site: root, arch: 'x64', temp, run })), { message });
    assert.match(lines.at(-1), /^rm -f rusty-photon-repo-verify-\d+-(apt|dnf)$/);
  }
  const { root, temp } = site();
  await assert.rejects(verifyRepo({ site: path.join(root, 'none'), temp, run: fakePodman().run }), {
    message: `${path.join(root, 'none')}/pubkey.asc missing: render the site first`,
  });
});

test('serves the files under its root, and nothing else', async () => {
  const { root } = site();
  mkdirSync(path.join(root, 'deb', 'pool'), { recursive: true });
  writeFileSync(path.join(path.dirname(root), 'secret'), 'not served');
  const server = await serveDirectory(root);
  try {
    const get = (url, init) => fetch(`${server.url}${url}`, init);
    assert.equal(await (await get('/pubkey.asc')).text(), 'PUBLIC KEY');
    const head = await get('/deb/dists/nightly/InRelease', { method: 'HEAD' });
    assert.equal(head.status, 200);
    assert.equal(head.headers.get('content-length'), '14');
    for (const url of ['/deb/pool', '/missing', '/..%2Fsecret', '/%2e%2e/secret', '/%E0%A4%A']) {
      assert.equal((await get(url)).status, 404, url);
    }
    assert.equal((await get('/pubkey.asc', { method: 'DELETE' })).status, 404);
  } finally {
    await server.close();
  }
});

test("gives the client the source and .repo docs/packaging.md gives a machine", () => {
  assert.equal(aptSource('http://h'), 'deb [signed-by=/etc/apt/keyrings/rusty-photon.asc] http://h/deb nightly main\n');
  assert.equal(
    dnfRepo('http://h'),
    '[rusty-photon-nightly]\nname=Rusty Photon nightly\nbaseurl=http://h/rpm/$basearch/\nenabled=1\nrepo_gpgcheck=1\ngpgcheck=0\ngpgkey=http://h/pubkey.asc\n',
  );
  assert.match(FEDORA_CLIENT, /^FROM registry\.fedoraproject\.org\/fedora:44\nRUN dnf -y install systemd dnf5-plugins/);
});
