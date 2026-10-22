// nightly-packages.yml's repo job: the apt and dnf repositories of the
// static site pkg.rustyphoton.space serves, rendered from the Linux legs'
// verified packages and signed, for verify-repo.mjs to install from as a
// client would before r2.mjs publishes them (docs/packaging.md, Nightly
// channel). Conventions: docs/skills/ci-logic.md.
//
//   <site>/pubkey.asc                    the signing key's public half
//   <site>/deb/pool/main/*.deb
//   <site>/deb/dists/nightly/{InRelease,Release,Release.gpg}
//   <site>/deb/dists/nightly/main/binary-<arch>/Packages(.gz)
//   <site>/deb/dists/nightly/main/binary-<arch>/by-hash/{SHA256,SHA512}/<hash>
//   <site>/rpm/<arch>/*.rpm              x86_64 and aarch64
//   <site>/rpm/<arch>/repodata/          with repomd.xml.asc
//
// One rolling apt suite, `nightly`, and one flat dnf repository per
// architecture, each rendered afresh on every run: the channel replaces its
// packages and keeps none. apt's Release is signed twice, clearsigned as
// InRelease, which current apt reads, and detached as Release.gpg, which
// older apt reads. It sets Acquire-By-Hash, and each index is copied under
// its hashes: with the one generation the push keeps (r2.mjs), a client that
// read the outgoing InRelease still finds the index bytes it names. The
// canonical Packages(.gz) stay for clients that do not fetch by hash, which
// a retry gets past. dnf's metadata is signed in repomd.xml.asc, which a
// client checks with repo_gpgcheck=1, and its checksums cover every package,
// so the packages are not signed themselves (gpgcheck=0 in the client config
// docs/packaging.md gives). An rpm's file name has its `^` as `.`, as its
// release asset's has; its header keeps the `^`.

import { createHash } from 'node:crypto';
import { existsSync } from 'node:fs';
import { copyFile, mkdir, mkdtemp, readdir, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { gzipSync } from 'node:zlib';

import { execute, group, io, withoutRunnerVariables } from '../lib/helpers.mjs';

/** The suite apt clients name, the one this channel has. */
export const SUITE = 'nightly';

/**
 * What the rendering runs, from Ubuntu's archive: dpkg-scanpackages
 * (dpkg-dev), apt-ftparchive (apt-utils), createrepo_c and gpg.
 */
export const APT_PACKAGES = ['dpkg-dev', 'apt-utils', 'gnupg', 'createrepo-c'];

/** The packages in `dir` with `suffix`, by name. */
async function packages(dir, suffix) {
  return (await readdir(dir)).filter((name) => name.startsWith('rusty-photon-') && name.endsWith(suffix)).sort();
}

/** A deb's architecture, the last `_` field of its name: `amd64`, `arm64`. */
export function debArch(name) {
  return path.basename(name, '.deb').split('_').pop();
}

/** An rpm's architecture, the last `.` field of its name: `x86_64`, `aarch64`. */
export function rpmArch(name) {
  return path.basename(name, '.rpm').split('.').pop();
}

/** The fingerprint of the first key in `gpg --with-colons` output. */
export function fingerprint(colons) {
  const line = colons.split('\n').find((row) => row.startsWith('fpr:'));
  return line?.split(':')[9] ?? '';
}

/**
 * Installs what the rendering runs that the runner lacks, with apt, as the
 * runner's passwordless sudo allows.
 */
export async function prerequisites({ run = execute, root = process.getuid?.() === 0 } = {}) {
  // Every package dpkg knows, in one listing: a query for one it does not
  // know would put its complaint in the log.
  const listed = await run('dpkg-query', ['-W', '-f', '${Package} ${Status}\n'], { echo: false });
  const installed = new Set(
    listed
      .split('\n')
      .filter((line) => line.endsWith(' installed'))
      .map((line) => line.split(' ')[0]),
  );
  const missing = APT_PACKAGES.filter((name) => !installed.has(name));
  if (missing.length === 0) return;
  io.stdout(`Installing prerequisites: ${missing.join(' ')}\n`);
  const aptGet = (args) => (root ? run('apt-get', args) : run('sudo', ['apt-get', ...args]));
  await aptGet(['update', '-qq']);
  await aptGet(['install', '-y', '--no-install-recommends', ...missing]);
}

/**
 * Runs `fn` with the environment that signs with `key`, the armored private
 * key, imported into a GnuPG home of its own under `temp`, which goes however
 * `fn` ends. The key must be the one `pubkey` holds, the public key clients
 * are given: one that is not would publish a site no client can verify.
 * Resolves to what `fn` does.
 */
export async function withSigningKey({ key, pubkey, temp = os.tmpdir(), run = execute }, fn) {
  const home = await mkdtemp(path.join(temp, 'gnupg-'));
  try {
    const env = { ...withoutRunnerVariables(process.env), GNUPGHOME: home };
    // Only inside the home, which mkdtemp makes for this user alone, and
    // gone once imported.
    const file = path.join(home, 'key.asc');
    await writeFile(file, `${key}\n`, { mode: 0o600 });
    await run('gpg', ['--batch', '--quiet', '--import', file], { env, echo: false }).catch(() => {
      throw new Error('importing the signing key failed (it must be an armored private key)');
    });
    await rm(file);
    const signing = fingerprint(await run('gpg', ['--batch', '--list-secret-keys', '--with-colons'], { env, echo: false }));
    if (!signing) throw new Error('no secret key found after importing the signing key');
    const published = fingerprint(await run('gpg', ['--batch', '--show-keys', '--with-colons', pubkey], { env, echo: false }));
    if (signing !== published) {
      throw new Error(
        `signing key ${signing} does not match ${pubkey} (${published || 'no key'}): the secret and the committed public key have drifted`,
      );
    }
    return await fn({ env, fingerprint: signing });
  } finally {
    await rm(home, { recursive: true, force: true });
  }
}

// The hex digest of `bytes` with `algorithm`.
function digest(algorithm, bytes) {
  return createHash(algorithm).update(bytes).digest('hex');
}

/**
 * Renders the apt repository of the debs in `dist` into `<site>/deb`, signed
 * with `env`'s key, and resolves to its architectures.
 */
export async function renderApt({ dist, site, env, run = execute }) {
  const debs = await packages(dist, '.deb');
  const arches = [...new Set(debs.map(debArch))].sort();
  if (arches.length === 0) throw new Error(`no rusty-photon-*.deb packages in ${dist}`);
  io.stdout(`Rendering the apt tree (${site}/deb; arches: ${arches.join(' ')})\n`);
  const root = path.join(site, 'deb');
  await rm(root, { recursive: true, force: true });
  const pool = path.join(root, 'pool', 'main');
  await mkdir(pool, { recursive: true });
  for (const name of debs) await copyFile(path.join(dist, name), path.join(pool, name));
  const suite = path.join(root, 'dists', SUITE);
  for (const arch of arches) {
    const dir = path.join(suite, 'main', `binary-${arch}`);
    await mkdir(path.join(dir, 'by-hash', 'SHA256'), { recursive: true });
    await mkdir(path.join(dir, 'by-hash', 'SHA512'), { recursive: true });
    // Run from the tree's root, so each package's Filename is pool/main/….
    const index = Buffer.from(await run('dpkg-scanpackages', ['--arch', arch, 'pool'], { cwd: root, echo: false }));
    const gzipped = gzipSync(index, { level: 9 });
    // apt fetches by the strongest hash Release lists, SHA512 on current
    // apt; SHA256 is kept for a client that prefers it.
    for (const [name, bytes] of [
      ['Packages', index],
      ['Packages.gz', gzipped],
    ]) {
      await writeFile(path.join(dir, name), bytes);
      await writeFile(path.join(dir, 'by-hash', 'SHA256', digest('sha256', bytes)), bytes);
      await writeFile(path.join(dir, 'by-hash', 'SHA512', digest('sha512', bytes)), bytes);
    }
  }
  const field = (name, value) => ['-o', `APT::FTPArchive::Release::${name}=${value}`];
  const release = await run(
    'apt-ftparchive',
    [
      ...field('Origin', 'rusty-photon'),
      ...field('Label', 'rusty-photon'),
      ...field('Suite', SUITE),
      ...field('Codename', SUITE),
      ...field('Components', 'main'),
      ...field('Architectures', arches.join(' ')),
      ...field('Acquire-By-Hash', 'yes'),
      'release',
      '.',
    ],
    { cwd: suite, echo: false },
  );
  await writeFile(path.join(suite, 'Release'), release);
  await run('gpg', ['--batch', '--yes', '--clearsign', '-o', 'InRelease', 'Release'], { cwd: suite, env });
  await run('gpg', ['--batch', '--yes', '--detach-sign', '--armor', '-o', 'Release.gpg', 'Release'], { cwd: suite, env });
  return arches;
}

/**
 * Renders a dnf repository per architecture of the rpms in `dist` into
 * `<site>/rpm/<arch>`, signed with `env`'s key, and resolves to the
 * architectures.
 */
export async function renderYum({ dist, site, env, run = execute }) {
  const rpms = await packages(dist, '.rpm');
  const arches = [...new Set(rpms.map(rpmArch))].sort();
  if (arches.length === 0) throw new Error(`no rusty-photon-*.rpm packages in ${dist}`);
  io.stdout(`Rendering the dnf trees (${site}/rpm; arches: ${arches.join(' ')})\n`);
  await rm(path.join(site, 'rpm'), { recursive: true, force: true });
  for (const arch of arches) {
    const dir = path.join(site, 'rpm', arch);
    await mkdir(dir, { recursive: true });
    for (const name of rpms.filter((rpm) => rpmArch(rpm) === arch)) {
      await copyFile(path.join(dist, name), path.join(dir, name.replaceAll('^', '.')));
    }
    // gz: any dnf reads it, the oldest included.
    await run('createrepo_c', ['--general-compress-type', 'gz', dir], { echo: false });
    const repomd = path.join(dir, 'repodata', 'repomd.xml');
    await run('gpg', ['--batch', '--yes', '--detach-sign', '--armor', '-o', `${repomd}.asc`, repomd], { env });
  }
  return arches;
}

/** Every file under `dir`, by its path from there with `/` between folders, sorted. */
export async function filesUnder(dir) {
  const entries = await readdir(dir, { recursive: true, withFileTypes: true });
  return entries
    .filter((entry) => entry.isFile())
    .map((entry) => path.relative(dir, path.join(entry.parentPath, entry.name)).split(path.sep).join('/'))
    .sort();
}

/**
 * Renders the site from the packages in `dist`: `pubkey`, the apt tree and
 * the dnf trees, signed with `key`, after installing what that needs.
 * Resolves to the signing key's fingerprint.
 */
export async function renderSite({ dist, site, pubkey, key, temp, run = execute }) {
  if (!key) throw new Error('no signing key given (an armored private key)');
  if (!existsSync(pubkey)) throw new Error(`${pubkey} not found`);
  if (!existsSync(dist)) throw new Error(`${dist} not found: the Linux legs' packages go there`);
  await group('Installing the prerequisites', () => prerequisites({ run }));
  return withSigningKey({ key, pubkey, temp, run }, async ({ env, fingerprint: signing }) => {
    await mkdir(site, { recursive: true });
    await copyFile(pubkey, path.join(site, 'pubkey.asc'));
    await group('Rendering the apt repository', () => renderApt({ dist, site, env, run }));
    await group('Rendering the dnf repositories', () => renderYum({ dist, site, env, run }));
    io.stdout(`\nSite in ${site} (signed by ${signing}):\n${(await filesUnder(site)).join('\n')}\n`);
    return signing;
  });
}
