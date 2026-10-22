// nightly-packages.yml's msi job: the MSI the nightly release carries now,
// which the job's verifier installs first and upgrades over. Conventions:
// docs/skills/ci-logic.md.

import path from 'node:path';

import { github } from '../lib/github.mjs';
import { io } from '../lib/helpers.mjs';
import { download } from '../lib/install.mjs';

// The SHA-256 in an asset's digest, which GitHub gives for an asset uploaded
// since mid-2025.
const SHA256 = /^sha256:([0-9a-f]{64})$/;

/**
 * Downloads the first MSI the nightly release carries into `dir`, checked
 * against the SHA-256 GitHub gives for it, and resolves to its path. Publish
 * replaces the assets only after this job, so this is the nightly before the
 * one being built, and installing over it is the real nightly-over-nightly
 * upgrade. Only an empty channel resolves to undefined, for no upgrade seed:
 * no release yet, or one with no MSI yet. A failed read or download throws,
 * so a transient error cannot drop the upgrade proof unnoticed. The asset is
 * fetched through the API by its id, with `token`; `download`'s options are
 * lib/install.mjs's.
 */
export async function fetchPriorMsi({ api, repo, token, dir, download: options, ...rest }) {
  const response = await github('Reading the nightly release', `${api}/repos/${repo}/releases/tags/nightly`, { token, missing: true, ...rest });
  if (response === undefined) {
    io.stdout('There is no nightly release yet; skipping the upgrade seed.\n');
    return undefined;
  }
  const { assets = [] } = await response.json();
  const asset = assets.find(({ name }) => /\.msi$/i.test(name));
  if (asset === undefined) {
    io.stdout('The nightly release has no MSI yet; skipping the upgrade seed.\n');
    return undefined;
  }
  const sha256 = typeof asset.digest === 'string' ? asset.digest.match(SHA256)?.[1] : undefined;
  if (sha256 === undefined) {
    throw new Error(
      `The nightly release's ${asset.name} has no SHA-256 from GitHub to check it against (its digest is ${JSON.stringify(asset.digest ?? null)})`,
    );
  }
  const file = path.join(dir, asset.name);
  await download({ name: asset.name, url: `${api}/repos/${repo}/releases/assets/${asset.id}`, pin: { sha256 } }, file, {
    ...options,
    headers: { accept: 'application/octet-stream', ...(token && { authorization: `Bearer ${token}` }) },
  });
  io.stdout(`Upgrade seed: ${asset.name}\n`);
  return file;
}
