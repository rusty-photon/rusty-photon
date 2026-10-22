// nightly-packages.yml's publish job: the nightly tag moved to the packaged
// commit, and the one rolling prerelease given that commit's assets in place
// of the last, through the GitHub API. Conventions: docs/skills/ci-logic.md.

import { readFile } from 'node:fs/promises';
import path from 'node:path';

import { everyPage, github } from '../lib/github.mjs';
import { io, isTransient, retry } from '../lib/helpers.mjs';
import { SUMS, mediaType, readSums } from './assets.mjs';

/** The tag and the release, which publishing moves and rewrites. */
export const TAG = 'nightly';
export const TITLE = 'Nightly packages';

// Uploads under way at once, as gh release upload runs them.
const UPLOADS = 5;

// How long one upload may take, its body included: the MSI is over 100 MB.
const UPLOAD_TIMEOUT_MS = 600_000;

/**
 * The release's notes for the nightly `version` of commit `sha`, built on
 * `date`, with a table of `assets` (`{ name, sha256 }`). The rpm's version is
 * stated here because its asset's name cannot carry its `^`.
 */
export function releaseBody({ server = 'https://github.com', repo, sha, date, version, assets }) {
  const docs = `${server}/${repo}/blob/${sha}/docs`;
  const rpm = version.replace('+nightly.', '^');
  const base = version.split('+')[0];
  return [
    'Rolling nightly build of `main`. Assets are replaced on',
    'every publish — no dated history; fetch `SHA256SUMS.txt`',
    'from this release for the current filenames. Install and',
    `channel semantics: [docs/packaging.md](${docs}/packaging.md#nightly-channel)`,
    `(Windows: [docs/packaging-windows.md](${docs}/packaging-windows.md#nightly-channel),`,
    `macOS: [docs/packaging-macos.md](${docs}/packaging-macos.md#nightly-channel)).`,
    '',
    `- **Source**: ${sha}`,
    `- **Built**: ${date} (UTC)`,
    `- **Nightly version**: \`${version}\` (Debian version, MSI filename, and macOS tarball/formula version; the MSI's Programs & Features version renders as \`${base}.<YYDDD>\`)`,
    `- **RPM version**: \`${rpm}\` (\`rpm -q\` shows this; the asset filenames render the \`^\` as \`.\` because GitHub rewrites it at upload)`,
    "- **macOS**: don't download the tarballs by hand — `brew tap rusty-photon/rusty-photon && brew install rusty-photon-nightly` (the formulas point here)",
    '',
    '| Asset | SHA-256 |',
    '|-------|---------|',
    ...assets.map(({ name, sha256 }) => `| \`${name}\` | \`${sha256}\` |`),
    '',
  ].join('\n');
}

/**
 * Points the nightly tag at `sha`, wherever it was, as a lightweight tag: the
 * ref moved by force, or made when there is none. Moving it is sent again
 * after a 5xx, since setting it to the same commit twice changes nothing;
 * making it is sent once, as a repeat GitHub had acted on would fail.
 */
export async function moveTag({ api, repo, token, sha, ...options }) {
  const refs = `${api}/repos/${repo}/git`;
  const current = await github('Reading the nightly tag', `${refs}/ref/tags/${TAG}`, { token, missing: true, ...options });
  if (current === undefined) {
    await github(`Making the nightly tag at ${sha}`, `${refs}/refs`, { token, json: { ref: `refs/tags/${TAG}`, sha }, ...options });
  } else {
    await github(`Moving the nightly tag to ${sha}`, `${refs}/refs/tags/${TAG}`, {
      token,
      method: 'PATCH',
      json: { sha, force: true },
      attempts: 3,
      ...options,
    });
  }
  io.stdout(`The nightly tag is at ${sha}.\n`);
}

// Runs `fn` on each of `items`, `size` at a time. The first failure rejects,
// once the calls under way have ended, and no further call starts.
async function inTurn(items, size, fn) {
  const queue = [...items];
  const failures = [];
  const worker = async () => {
    while (queue.length > 0 && failures.length === 0) {
      const item = queue.shift();
      try {
        await fn(item);
      } catch (err) {
        failures.push(err);
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(size, items.length) }, worker));
  if (failures.length > 0) throw failures[0];
}

// The release's assets, every page.
function listAssets(releases, release, context) {
  return everyPage('Listing the nightly release\'s assets', `${releases}/${release.id}/assets?per_page=100`, context);
}

// Deletes `asset` from the release; one already gone is no error.
async function deleteAsset(releases, asset, context) {
  await github(`Deleting ${asset.name}`, `${releases}/assets/${asset.id}`, { ...context, method: 'DELETE', missing: true });
}

/**
 * Uploads `file` to the release as `name`. An upload that fails transiently
 * is tried again, up to three times in all, and before each retry an asset
 * the failed attempt left under that name, half made or whole, is deleted,
 * since GitHub refuses a second asset of one name. GitHub's name for the
 * asset must be `name`: one it renamed would not match the sums.
 */
async function uploadAsset(releases, release, { name, file }, context) {
  const uploads = release.upload_url.replace(/\{.*\}$/, '');
  const body = await readFile(file);
  await retry(
    async (attempt) => {
      if (attempt > 1) {
        for (const left of (await listAssets(releases, release, context)).filter((asset) => asset.name === name)) {
          await deleteAsset(releases, left, context);
        }
      }
      const response = await github(`Uploading ${name}`, `${uploads}?${new URLSearchParams({ name })}`, {
        ...context,
        upload: { body, type: mediaType(name) },
        attempts: 1,
        timeoutMs: UPLOAD_TIMEOUT_MS,
      });
      const asset = await response.json();
      if (asset.name !== name) throw new Error(`GitHub named the upload of ${name} ${asset.name}, which the sums do not list`);
    },
    { attempts: 3, delayMs: context.delayMs, label: `upload ${name}`, isRetryable: (err) => isTransient(err.cause ?? err) },
  );
}

/**
 * Gives the nightly release `body` and the assets `files` (`{ name, file }`)
 * in place of every asset it had: a package dropped from the set must not
 * linger, so the old assets all go first, and the release is briefly without
 * any, which this channel accepts. A release that does not exist yet is made,
 * on the tag moveTag put in place. The rewrite of an existing release is sent
 * again after a 5xx, since it sets the same fields; making one is sent once.
 */
export async function publishRelease({ api, repo, token, body, files, ...options }) {
  const releases = `${api}/repos/${repo}/releases`;
  const context = { token, ...options };
  const fields = { name: TITLE, body, prerelease: true };
  const found = await github('Reading the nightly release', `${releases}/tags/${TAG}`, { ...context, missing: true });
  let release;
  if (found === undefined) {
    release = await (await github('Making the nightly release', releases, { ...context, json: { tag_name: TAG, ...fields } })).json();
    io.stdout('Made the nightly release.\n');
  } else {
    release = await found.json();
    const old = await listAssets(releases, release, context);
    await inTurn(old, UPLOADS, (asset) => deleteAsset(releases, asset, context));
    io.stdout(`Deleted the nightly release's ${old.length} asset(s).\n`);
    release = await (
      await github('Rewriting the nightly release', `${releases}/${release.id}`, { ...context, method: 'PATCH', json: fields, attempts: 3 })
    ).json();
  }
  await inTurn(files, UPLOADS, (asset) => uploadAsset(releases, release, asset, context));
  io.stdout(`Uploaded ${files.length} asset(s) to ${release.html_url}\n`);
}

/**
 * Publishes the nightly `version` of commit `sha`, built on `date`: the tag
 * moved to `sha`, then the release given the assets SUMS in `dir` lists, SUMS
 * among them, and notes that list them.
 */
export async function publishNightly({ api, repo, token, sha, date, version, dir, ...options }) {
  if (!/^[0-9a-f]{40}$/.test(sha)) throw new Error(`the packaged commit, ${JSON.stringify(sha)}, is not a SHA-1 in lowercase hex`);
  const assets = await readSums(dir);
  const files = [...assets.map(({ name }) => name), SUMS].map((name) => ({ name, file: path.join(dir, name) }));
  await moveTag({ api, repo, token, sha, ...options });
  await publishRelease({ api, repo, token, body: releaseBody({ repo, sha, date, version, assets }), files, ...options });
}
