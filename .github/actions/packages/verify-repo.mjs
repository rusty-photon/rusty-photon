// nightly-packages.yml's repo job, before anything is published: the apt
// and dnf repositories repo.mjs rendered, installed from as a client would
// (docs/packaging.md). Conventions: docs/skills/ci-logic.md.
//
// It serves the site on loopback, then in podman containers that hold only
// the public signing key, adds the repository the way docs/packaging.md
// tells a machine to and installs a service through the real resolver. That
// proves what installing a downloaded package never does: the metadata's
// signature verifies for a client with only the public key (apt always
// checks; dnf does because the .repo sets repo_gpgcheck=1, as a real
// client's does), and the package resolves and installs from the index. The
// other architecture, where the site has it, is resolved and downloaded,
// its checksum checked, and never run. What the service does once installed
// is cargo ci verify-packages's to prove, on the same packages, before this.

import { createReadStream, existsSync } from 'node:fs';
import { copyFile, mkdir, mkdtemp, rm, stat, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import os from 'node:os';
import path from 'node:path';

import { execute, group, io } from '../lib/helpers.mjs';
import { SUITE } from './repo.mjs';

/** The images the clients run in. */
export const DEBIAN = 'debian:trixie';
export const FEDORA = 'registry.fedoraproject.org/fedora:44';

/**
 * The Fedora client's image. Every real Fedora host has systemd, whose
 * systemctl the rpm scriptlets call, and dnf5 fails the transaction without
 * it; the base image has none. dnf5-plugins gives `dnf download`.
 */
export const FEDORA_CLIENT = `FROM ${FEDORA}\nRUN dnf -y install systemd dnf5-plugins && dnf clean all\n`;
export const FEDORA_CLIENT_IMAGE = 'localhost/rusty-photon-repo-verify-rpm';

/** The architectures as apt and dnf name them, this machine's first. */
export function architectures(arch) {
  if (arch === 'x64') return { deb: ['amd64', 'arm64'], rpm: ['x86_64', 'aarch64'] };
  if (arch === 'arm64') return { deb: ['arm64', 'amd64'], rpm: ['aarch64', 'x86_64'] };
  throw new Error(`unsupported architecture ${arch} (need x64 or arm64)`);
}

/** The apt source line docs/packaging.md gives a machine, for `base`. */
export function aptSource(base) {
  return `deb [signed-by=/etc/apt/keyrings/rusty-photon.asc] ${base}/deb ${SUITE} main\n`;
}

/**
 * The .repo file docs/packaging.md gives a machine, for `base`:
 * repo_gpgcheck=1 makes dnf check repomd.xml's signature, which it otherwise
 * never does; gpgcheck=0, as the signed metadata's checksums cover the
 * packages.
 */
export function dnfRepo(base) {
  return [
    '[rusty-photon-nightly]',
    'name=Rusty Photon nightly',
    `baseurl=${base}/rpm/$basearch/`,
    'enabled=1',
    'repo_gpgcheck=1',
    'gpgcheck=0',
    `gpgkey=${base}/pubkey.asc`,
    '',
  ].join('\n');
}

/**
 * Serves the files under `root` on loopback, on a free port: a GET or HEAD
 * of a file answers it, and anything else, a directory or a path that leaves
 * `root` included, 404. Resolves to `{ url, close }`.
 */
export async function serveDirectory(root) {
  const server = createServer(async (req, res) => {
    try {
      let file;
      try {
        file = path.join(root, decodeURIComponent(new URL(req.url, 'http://loopback').pathname));
      } catch {
        file = null; // a malformed escape
      }
      const inside = file !== null && !path.relative(root, file).startsWith('..');
      const found = inside ? await stat(file).catch(() => null) : null;
      if (!found?.isFile() || !['GET', 'HEAD'].includes(req.method)) {
        res.writeHead(404).end();
        return;
      }
      res.writeHead(200, { 'content-length': found.size });
      if (req.method === 'HEAD') {
        res.end();
      } else {
        createReadStream(file).on('error', () => res.destroy()).pipe(res);
      }
    } catch {
      if (!res.headersSent) res.writeHead(500);
      res.end();
    }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections();
        server.close(resolve);
      }),
  };
}

/**
 * Runs `fn` with a container of `image` that sleeps, named `name`, on the
 * host's network so that it reaches the loopback server, with `mounts`
 * (`[host, container]`, read only); removed however `fn` ends. `fn` gets
 * `exec(args, options)`, which runs in it.
 */
async function withContainer({ name, image, mounts, run }, fn) {
  const volumes = mounts.flatMap(([host, inside]) => ['-v', `${host}:${inside}:ro,Z`]);
  await run('podman', ['run', '-d', '--rm', '--network', 'host', '--name', name, ...volumes, image, 'sleep', 'infinity'], {
    echo: false,
  });
  try {
    const exec = (args, { env = [], cwd, echo = true } = {}) =>
      run('podman', ['exec', ...env.flatMap((pair) => ['-e', pair]), ...(cwd ? ['-w', cwd] : []), name, ...args], { echo });
    return await fn(exec);
  } finally {
    await run('podman', ['rm', '-f', name], { echo: false }).catch(() => {});
  }
}

/** apt: installs `rusty-photon-<service>`, and downloads the other architecture's when the site has it. */
async function verifyApt({ site, base, service, deb, temp, run }) {
  const [native, foreign] = deb;
  const keyrings = path.join(temp, 'apt-keyrings');
  await mkdir(keyrings);
  // A directory over /etc/apt/keyrings, which the image has empty, so the
  // key sits where docs/packaging.md puts it.
  await copyFile(path.join(site, 'pubkey.asc'), path.join(keyrings, 'rusty-photon.asc'));
  const list = path.join(temp, 'rusty-photon-nightly.list');
  await writeFile(list, aptSource(base));
  const pkg = `rusty-photon-${service}`;
  const mounts = [
    [keyrings, '/etc/apt/keyrings'],
    [list, '/etc/apt/sources.list.d/rusty-photon-nightly.list'],
  ];
  io.stdout(`== apt: install ${pkg} from the repository (${DEBIAN})\n`);
  await withContainer({ name: `rusty-photon-repo-verify-${process.pid}-apt`, image: DEBIAN, mounts, run }, async (exec) => {
    const apt = ['DEBIAN_FRONTEND=noninteractive'];
    await exec(['apt-get', 'update'], { env: apt });
    await exec(['apt-get', 'install', '-y', pkg], { env: apt });
    const status = await exec(['dpkg-query', '-W', '-f', '${Status} ${Version}', pkg], { echo: false });
    if (!status.startsWith('install ok installed ')) throw new Error(`${pkg} not installed (dpkg says: ${status})`);
    io.stdout(`apt: installed ${status.split(' ').pop()} (${native})\n`);
    if (!existsSync(path.join(site, 'deb', 'dists', SUITE, 'main', `binary-${foreign}`, 'Packages.gz'))) return;
    await exec(['dpkg', '--add-architecture', foreign]);
    await exec(['apt-get', 'update'], { env: apt, echo: false });
    await exec(['apt-get', 'download', `${pkg}:${foreign}`], { cwd: '/tmp' });
    const files = (await exec(['ls', '/tmp'], { echo: false })).split('\n');
    if (!files.some((file) => file.startsWith(`${pkg}_`) && file.endsWith(`_${foreign}.deb`))) {
      throw new Error(`apt download of ${pkg}:${foreign} left no ${pkg}_*_${foreign}.deb`);
    }
    io.stdout(`apt: ${foreign} resolved, and its download checksum-verified\n`);
  });
}

/** dnf: installs `rusty-photon-<service>`, and downloads the other architecture's when the site has it. */
async function verifyDnf({ site, base, service, rpm, temp, run }) {
  const [native, foreign] = rpm;
  const context = path.join(temp, 'fedora-client');
  await mkdir(context);
  await writeFile(path.join(context, 'Containerfile'), FEDORA_CLIENT);
  await run('podman', ['build', '-q', '-t', FEDORA_CLIENT_IMAGE, '-f', path.join(context, 'Containerfile'), context], { echo: false });
  const repo = path.join(temp, 'rusty-photon-nightly.repo');
  await writeFile(repo, dnfRepo(base));
  const pkg = `rusty-photon-${service}`;
  io.stdout(`== dnf: install ${pkg} from the repository (${FEDORA} with systemd)\n`);
  const mounts = [[repo, '/etc/yum.repos.d/rusty-photon-nightly.repo']];
  await withContainer({ name: `rusty-photon-repo-verify-${process.pid}-dnf`, image: FEDORA_CLIENT_IMAGE, mounts, run }, async (exec) => {
    await exec(['dnf', '-y', 'install', pkg]);
    const version = await exec(['rpm', '-q', '--qf', '%{VERSION}', pkg], { echo: false });
    io.stdout(`dnf: installed ${version} (${native})\n`);
    if (!existsSync(path.join(site, 'rpm', foreign, 'repodata'))) return;
    await exec(['dnf', '-y', 'download', '--forcearch', foreign, pkg], { cwd: '/tmp' });
    const files = (await exec(['ls', '/tmp'], { echo: false })).split('\n');
    if (!files.some((file) => file.startsWith(`${pkg}-`) && file.endsWith(`.${foreign}.rpm`))) {
      throw new Error(`dnf download of ${pkg} for ${foreign} left no ${pkg}-*.${foreign}.rpm`);
    }
    io.stdout(`dnf: ${foreign} resolved, and its download checksum-verified\n`);
  });
}

/**
 * Installs `rusty-photon-<service>` from the site's apt and dnf
 * repositories, as clients with only its public key, each flavor the site
 * has. Rejects at the first failure.
 */
export async function verifyRepo({ site, service = 'sentinel', arch = process.arch, temp = os.tmpdir(), run = execute }) {
  if (!existsSync(path.join(site, 'pubkey.asc'))) throw new Error(`${site}/pubkey.asc missing: render the site first`);
  const root = path.resolve(site);
  const { deb, rpm } = architectures(arch);
  const work = await mkdtemp(path.join(temp, 'repo-verify-'));
  const server = await serveDirectory(root);
  try {
    io.stdout(`Serving ${root} at ${server.url}\n`);
    for (const [flavor, verify] of [
      ['deb', () => verifyApt({ site: root, base: server.url, service, deb, temp: work, run })],
      ['rpm', () => verifyDnf({ site: root, base: server.url, service, rpm, temp: work, run })],
    ]) {
      if (existsSync(path.join(root, flavor))) {
        await group(`Installing from the ${flavor === 'deb' ? 'apt' : 'dnf'} repository`, verify);
      } else {
        io.stdout(`== no ${site}/${flavor} tree: skipped\n`);
      }
    }
    io.stdout(`\nverify-repo: OK (${server.url}, service ${service})\n`);
  } finally {
    await server.close();
    await rm(work, { recursive: true, force: true });
  }
}
