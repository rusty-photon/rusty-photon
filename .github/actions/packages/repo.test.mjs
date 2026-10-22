import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { gunzipSync } from 'node:zlib';

import { capture } from '../lib/testing.mjs';
import { APT_PACKAGES, debArch, filesUnder, fingerprint, prerequisites, renderSite, rpmArch, withSigningKey } from './repo.mjs';

function tempDir() {
  return mkdtempSync(path.join(process.env.TEST_TMPDIR ?? tmpdir(), 'repo-'));
}

const FPR = 'A'.repeat(40);
const NIGHTLY = '0.1.0+nightly.202610221006.g8c9e489';

// The Linux legs' packages, both architectures, and the public key.
function fixture({ debs = ['amd64', 'arm64'], rpms = ['x86_64', 'aarch64'] } = {}) {
  const dir = tempDir();
  const dist = path.join(dir, 'pkgs');
  mkdirSync(dist);
  for (const service of ['rp', 'sentinel']) {
    for (const arch of debs) writeFileSync(path.join(dist, `rusty-photon-${service}_${NIGHTLY}_${arch}.deb`), `deb ${service} ${arch}`);
    for (const arch of rpms) {
      writeFileSync(path.join(dist, `rusty-photon-${service}-${NIGHTLY.replace('+nightly.', '^')}-1.${arch}.rpm`), `rpm ${service} ${arch}`);
    }
  }
  writeFileSync(path.join(dist, 'SHA256SUMS.txt'), 'sums');
  const pubkey = path.join(dir, 'pubkey.asc');
  writeFileSync(pubkey, '-----BEGIN PGP PUBLIC KEY BLOCK-----\n');
  return { dir, dist, pubkey, site: path.join(dir, 'site'), temp: dir };
}

/**
 * A runner for the rendering's programs, which writes what each would and
 * records each call as `[program, ...args]` with its directory and GnuPG home.
 * `installed` lists the apt packages dpkg knows; `publicFpr` is the key the
 * public key file holds.
 */
function fakeRun({ installed = APT_PACKAGES, publicFpr = FPR, importFails = false, noSecret = false } = {}) {
  const calls = [];
  const homes = new Set();
  const run = async (program, args, { cwd, env } = {}) => {
    calls.push({ line: [program, ...args].join(' '), cwd, home: env?.GNUPGHOME });
    if (env?.GNUPGHOME) homes.add(env.GNUPGHOME);
    const at = (file) => path.resolve(cwd ?? '.', file);
    if (program === 'dpkg') {
      if (!installed.includes(args[1])) throw new Error(`dpkg exited with 1: package '${args[1]}' is not installed`);
      return '';
    }
    if (program === 'gpg' && args.includes('--import')) {
      if (importFails) throw new Error('gpg exited with 2');
      assert.equal(readFileSync(args.at(-1), 'utf8'), 'PRIVATE KEY\n');
      return '';
    }
    if (program === 'gpg' && args.includes('--list-secret-keys')) {
      assert.ok(!existsSync(path.join(env.GNUPGHOME, 'key.asc')), 'the key file is gone once imported');
      return noSecret ? '' : `sec:u:4096:1:XXXX:::::::scESC:::+:::23::0:\nfpr:::::::::${FPR}:\n`;
    }
    if (program === 'gpg' && args.includes('--show-keys')) return `pub:-:4096:1:XXXX::::::::scESC::::::23::0:\nfpr:::::::::${publicFpr}:\n`;
    if (program === 'gpg') {
      writeFileSync(at(args[args.indexOf('-o') + 1]), `signed ${args.at(-1)}`);
      return '';
    }
    if (program === 'sudo' || program === 'apt-get') return '';
    if (program === 'dpkg-scanpackages') return `Package: rusty-photon\nArchitecture: ${args[1]}\nFilename: pool/main/x\n`;
    if (program === 'apt-ftparchive') return 'Origin: rusty-photon\n';
    if (program === 'createrepo_c') {
      mkdirSync(path.join(args.at(-1), 'repodata'));
      writeFileSync(path.join(args.at(-1), 'repodata', 'repomd.xml'), '<repomd/>');
      return '';
    }
    throw new Error(`unexpected ${program}`);
  };
  return { run, calls, homes };
}

function hash(algorithm, bytes) {
  return createHash(algorithm).update(bytes).digest('hex');
}

test('renders and signs the apt and dnf repositories of every architecture', async () => {
  const { dist, pubkey, site, temp } = fixture();
  const { run, calls, homes } = fakeRun();
  const { value } = await capture(() => renderSite({ dist, site, pubkey, key: 'PRIVATE KEY', temp, run }));
  assert.equal(value, FPR);

  const binary = (arch) => `deb/dists/nightly/main/binary-${arch}`;
  const indices = {};
  for (const arch of ['amd64', 'arm64']) {
    const packages = readFileSync(path.join(site, binary(arch), 'Packages'));
    const gzipped = readFileSync(path.join(site, binary(arch), 'Packages.gz'));
    assert.deepEqual(gunzipSync(gzipped), packages);
    assert.match(packages.toString(), new RegExp(`Architecture: ${arch}`));
    indices[arch] = { packages, gzipped };
  }
  const byHash = (arch) =>
    [
      ['SHA256', 'sha256'],
      ['SHA512', 'sha512'],
    ].flatMap(([dir, algorithm]) => [indices[arch].packages, indices[arch].gzipped].map((bytes) => `${binary(arch)}/by-hash/${dir}/${hash(algorithm, bytes)}`));
  // rpm's version is 0.1.0^202610221006.g8c9e489, its `^` a `.` here.
  const rpm = (service, arch) => `rpm/${arch}/rusty-photon-${service}-0.1.0.202610221006.g8c9e489-1.${arch}.rpm`;
  assert.deepEqual(
    await filesUnder(site),
    [
      ...byHash('amd64'),
      `${binary('amd64')}/Packages`,
      `${binary('amd64')}/Packages.gz`,
      ...byHash('arm64'),
      `${binary('arm64')}/Packages`,
      `${binary('arm64')}/Packages.gz`,
      'deb/dists/nightly/InRelease',
      'deb/dists/nightly/Release',
      'deb/dists/nightly/Release.gpg',
      ...['rp', 'sentinel'].flatMap((service) => ['amd64', 'arm64'].map((arch) => `deb/pool/main/rusty-photon-${service}_${NIGHTLY}_${arch}.deb`)),
      'pubkey.asc',
      rpm('rp', 'aarch64'),
      rpm('sentinel', 'aarch64'),
      'rpm/aarch64/repodata/repomd.xml',
      'rpm/aarch64/repodata/repomd.xml.asc',
      rpm('rp', 'x86_64'),
      rpm('sentinel', 'x86_64'),
      'rpm/x86_64/repodata/repomd.xml',
      'rpm/x86_64/repodata/repomd.xml.asc',
    ].sort(),
  );
  assert.equal(readFileSync(path.join(site, rpm('rp', 'x86_64')), 'utf8'), 'rpm rp x86_64');
  assert.equal(readFileSync(path.join(site, 'pubkey.asc'), 'utf8'), readFileSync(pubkey, 'utf8'));

  const deb = path.join(site, 'deb');
  const suite = path.join(deb, 'dists', 'nightly');
  // Paths in the site with `/` between folders, as on Windows too.
  const posix = (text) => text.split(path.sep).join('/');
  const where = ({ line, cwd }) =>
    cwd ? `${line} @ ${posix(path.relative(site, cwd))}` : line.split(site).map((part, i) => (i === 0 ? part : posix(part))).join('<site>');
  assert.deepEqual(
    calls.filter(({ line }) => !line.startsWith('dpkg ') && !line.startsWith('gpg --batch --quiet --import')).map(where),
    [
      'gpg --batch --list-secret-keys --with-colons',
      `gpg --batch --show-keys --with-colons ${pubkey}`,
      'dpkg-scanpackages --arch amd64 pool @ deb',
      'dpkg-scanpackages --arch arm64 pool @ deb',
      'apt-ftparchive -o APT::FTPArchive::Release::Origin=rusty-photon -o APT::FTPArchive::Release::Label=rusty-photon ' +
        '-o APT::FTPArchive::Release::Suite=nightly -o APT::FTPArchive::Release::Codename=nightly ' +
        '-o APT::FTPArchive::Release::Components=main -o APT::FTPArchive::Release::Architectures=amd64 arm64 ' +
        '-o APT::FTPArchive::Release::Acquire-By-Hash=yes release . @ deb/dists/nightly',
      'gpg --batch --yes --clearsign -o InRelease Release @ deb/dists/nightly',
      'gpg --batch --yes --detach-sign --armor -o Release.gpg Release @ deb/dists/nightly',
      'createrepo_c --general-compress-type gz <site>/rpm/aarch64',
      'gpg --batch --yes --detach-sign --armor -o <site>/rpm/aarch64/repodata/repomd.xml.asc <site>/rpm/aarch64/repodata/repomd.xml',
      'createrepo_c --general-compress-type gz <site>/rpm/x86_64',
      'gpg --batch --yes --detach-sign --armor -o <site>/rpm/x86_64/repodata/repomd.xml.asc <site>/rpm/x86_64/repodata/repomd.xml',
    ],
  );
  assert.equal(readFileSync(path.join(suite, 'Release'), 'utf8'), 'Origin: rusty-photon\n');
  assert.equal(readFileSync(path.join(suite, 'InRelease'), 'utf8'), 'signed Release');
  // Every gpg run used the one home, which is gone.
  assert.equal(homes.size, 1);
  for (const call of calls.filter(({ line }) => line.startsWith('gpg'))) assert.equal(call.home, [...homes][0]);
  assert.ok(!existsSync([...homes][0]));
  assert.ok(existsSync(deb));
});

test('renders the architectures it has packages for, afresh', async () => {
  const { dist, pubkey, site, temp } = fixture({ debs: ['amd64'], rpms: ['x86_64'] });
  mkdirSync(path.join(site, 'deb', 'dists', 'nightly', 'main', 'binary-arm64'), { recursive: true });
  writeFileSync(path.join(site, 'deb', 'dists', 'nightly', 'main', 'binary-arm64', 'Packages'), 'stale');
  mkdirSync(path.join(site, 'rpm', 'aarch64'), { recursive: true });
  writeFileSync(path.join(site, 'rpm', 'aarch64', 'stale.rpm'), 'stale');
  const { run, calls } = fakeRun();
  await capture(() => renderSite({ dist, site, pubkey, key: 'PRIVATE KEY', temp, run }));
  const files = await filesUnder(site);
  assert.ok(!files.some((file) => file.includes('arm64') || file.includes('aarch64')), files.join('\n'));
  assert.ok(calls.some(({ line }) => line.includes('Release::Architectures=amd64 -o')));
});

test('installs only the prerequisites the runner lacks, through sudo unless root', async () => {
  const { run, calls } = fakeRun({ installed: ['apt-utils', 'gnupg'] });
  await capture(() => prerequisites({ run, root: false }));
  assert.deepEqual(
    calls.map(({ line }) => line),
    [
      ...APT_PACKAGES.map((name) => `dpkg -s ${name}`),
      'sudo apt-get update -qq',
      'sudo apt-get install -y --no-install-recommends dpkg-dev createrepo-c',
    ],
  );
  const asRoot = fakeRun({ installed: ['dpkg-dev', 'apt-utils', 'gnupg'] });
  await capture(() => prerequisites({ run: asRoot.run, root: true }));
  assert.deepEqual(asRoot.calls.slice(-2).map(({ line }) => line), ['apt-get update -qq', 'apt-get install -y --no-install-recommends createrepo-c']);
  const all = fakeRun();
  await prerequisites({ run: all.run });
  assert.equal(all.calls.length, APT_PACKAGES.length);
});

test('signs only with the key clients are given, and leaves no GnuPG home behind', async () => {
  const { pubkey, temp } = fixture();
  const sign = (options, fn = async () => 'signed') => withSigningKey({ key: 'PRIVATE KEY', pubkey, temp, ...options }, fn);
  const other = 'B'.repeat(40);
  await assert.rejects(sign({ run: fakeRun({ publicFpr: other }).run }), {
    message: `signing key ${FPR} does not match ${pubkey} (${other}): the secret and the committed public key have drifted`,
  });
  await assert.rejects(sign({ run: fakeRun({ importFails: true }).run }), {
    message: 'importing the signing key failed (it must be an armored private key)',
  });
  await assert.rejects(sign({ run: fakeRun({ noSecret: true }).run }), { message: 'no secret key found after importing the signing key' });
  const { run, homes } = fakeRun();
  await assert.rejects(sign({ run }, async () => {
    throw new Error('rendering failed');
  }), { message: 'rendering failed' });
  assert.ok(!existsSync([...homes][0]), 'the home goes when the rendering fails');
});

test('refuses what it cannot render', async () => {
  const { dist, pubkey, site, temp } = fixture({ debs: [], rpms: ['x86_64'] });
  const render = (options) => capture(() => renderSite({ dist, site, pubkey, key: 'PRIVATE KEY', temp, run: fakeRun().run, ...options }));
  await assert.rejects(render(), { message: `no rusty-photon-*.deb packages in ${dist}` });
  await assert.rejects(render({ key: '' }), { message: 'no signing key given (an armored private key)' });
  await assert.rejects(render({ pubkey: `${pubkey}.missing` }), { message: `${pubkey}.missing not found` });
  await assert.rejects(render({ dist: `${dist}.missing` }), { message: `${dist}.missing not found: the Linux legs' packages go there` });
  const noRpms = fixture({ rpms: [] });
  await assert.rejects(capture(() => renderSite({ ...noRpms, key: 'PRIVATE KEY', run: fakeRun().run })), {
    message: `no rusty-photon-*.rpm packages in ${noRpms.dist}`,
  });
});

test('reads the architecture of a package and the fingerprint of a key', () => {
  assert.equal(debArch(`rusty-photon-ui-htmx_${NIGHTLY}_arm64.deb`), 'arm64');
  assert.equal(rpmArch('rusty-photon-ui-htmx-0.1.0^202610221006.g8c9e489-1.aarch64.rpm'), 'aarch64');
  assert.equal(fingerprint(`sec:u:4096\nfpr:::::::::${FPR}:\nfpr:::::::::${'C'.repeat(40)}:\n`), FPR);
  assert.equal(fingerprint('sec:u:4096\n'), '');
});
