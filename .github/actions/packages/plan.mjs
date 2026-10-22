// nightly-packages.yml's plan job: whether the published nightly already has
// the checked-out commit, and if not, the version every package leg stamps.
// Conventions: docs/skills/ci-logic.md.

import { github } from '../lib/github.mjs';
import { io } from '../lib/helpers.mjs';

/**
 * The workspace's version: the first line of `cargoToml` that reads
 * `version = "<version>"` from its first column, which is how the packaging
 * scripts read it. None throws.
 */
export function workspaceVersion(cargoToml) {
  for (const line of cargoToml.split(/\r?\n/)) {
    const version = line.match(/^version = "(.*)"$/)?.[1];
    if (version !== undefined) return version;
  }
  throw new Error('could not read the workspace version from Cargo.toml');
}

/**
 * The nightly's version, `<base>+nightly.<stamp>.g<short>`, and the stamp's
 * UTC date (YYYYMMDD). The stamp is `now` in UTC to the minute, merged into
 * one numeric segment: the g<short> suffix carries no order (hex compares
 * lexically), so the stamp alone must sort a second same-day publish above
 * the first. Minute ties between published stamps cannot happen: the
 * workflow's concurrency group runs one at a time, and a publish lands only
 * after its run's build legs, which take minutes, so the next run mints its
 * stamp well after the previous publish. A failed run's stamp publishes
 * nothing and orders against nothing. The Debian packages, the MSI's file
 * name and the macOS tarballs carry this string; the rpm's `^` form and the
 * MSI's ProductVersion are derived from it.
 */
export function nightlyVersion({ base, now, short }) {
  const stamp = now.toISOString().replace(/\D/g, '').slice(0, 12);
  return { date: stamp.slice(0, 8), version: `${base}+nightly.${stamp}.g${short}` };
}

/**
 * The commit the repository's `nightly` tag names, or undefined when there is
 * no such tag. Publishing pushes the tag lightweight, so it names the commit;
 * an annotated tag, made by hand, is peeled to the commit it tags.
 */
export async function publishedNightly({ api, repo, token, ...options }) {
  const response = await github('Reading the nightly tag', `${api}/repos/${repo}/git/ref/tags/nightly`, { token, missing: true, ...options });
  if (response === undefined) return undefined;
  let { object } = await response.json();
  while (object.type === 'tag') {
    const tag = await github(`Reading the tag object ${object.sha}`, `${api}/repos/${repo}/git/tags/${object.sha}`, { token, ...options });
    ({ object } = await tag.json());
  }
  return object.sha;
}

/**
 * The plan job's outputs for the checked-out commit `head`: `skip` ('true'
 * when the nightly tag already names it, and nothing else is computed), its
 * `sha`, and the nightly's `date` and `nightly_version`, from the workspace
 * version in `cargoToml` and the time `now`.
 */
export async function plan({ api, repo, token, head, cargoToml, now = new Date(), ...options }) {
  const published = await publishedNightly({ api, repo, token, ...options });
  if (published === head) {
    io.stdout(`The nightly tag is already at ${head}; nothing to do.\n`);
    return { skip: 'true', sha: head };
  }
  io.stdout(`Publishing ${head} (nightly tag: ${published ?? 'none'}).\n`);
  const { date, version } = nightlyVersion({ base: workspaceVersion(cargoToml), now, short: head.slice(0, 7) });
  io.stdout(`Nightly version: ${version}\n`);
  return { skip: 'false', sha: head, date, nightly_version: version };
}
