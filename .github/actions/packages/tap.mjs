// The Homebrew tap's formulas: a channel's formulas in the tap's Formula
// directory replaced by the ones generate-brew-formulas.sh rendered, in one
// commit made through the GitHub API, or none when the tap already holds
// them. The token reaches this step alone, and no clone of the tap is kept
// to push from. Conventions: docs/skills/ci-logic.md.

import { readFile, readdir } from 'node:fs/promises';
import path from 'node:path';

import { github } from '../lib/github.mjs';
import { io } from '../lib/helpers.mjs';

/** The tap, its branch, and the directory its formulas are in. */
export const TAP = 'rusty-photon/homebrew-rusty-photon';
export const BRANCH = 'main';
export const FORMULA = 'Formula';

// Who the commit is by, as the job's push from a clone had it.
const BOT = { name: 'github-actions[bot]', email: 'github-actions[bot]@users.noreply.github.com' };

// Each channel's commit message, for a version.
const MESSAGES = {
  nightly: (version) => `nightly: ${version}`,
  stable: (version) => `release: ${version}`,
};

// The writes a repeat leaves as they were, which fetchWithRetry may send again
// after a 5xx or no answer: a tree is named by its content, and a second
// commit is one more commit nothing points at. The branch moves once: a
// repeat after GitHub had moved it is no fast-forward from where it was.
const REPEATABLE = { attempts: 3 };

/**
 * Whether the file `name` in the tap's Formula directory is `channel`'s: the
 * nightly channel's formulas end in -nightly.rb, and the stable channel's are
 * the other .rb files. Anything else is neither's.
 */
export function belongsTo(channel, name) {
  if (!name.endsWith('.rb')) return false;
  return name.endsWith('-nightly.rb') === (channel === 'nightly');
}

/** The formulas in `dir`, as `{ name, content }`, by name. */
export async function readFormulas(dir) {
  const names = (await readdir(dir, { withFileTypes: true }))
    .filter((entry) => entry.isFile() && entry.name.endsWith('.rb'))
    .map(({ name }) => name)
    .sort();
  return Promise.all(names.map(async (name) => ({ name, content: await readFile(path.join(dir, name), 'utf8') })));
}

// The JSON answer to a request of the GitHub API (github).
async function answer(what, url, options) {
  return (await github(what, url, options)).json();
}

/**
 * Commits `formulas` (`{ name, content }`) to the tap's Formula directory in
 * place of every formula `channel` ('nightly' or 'stable') had there, so a
 * service dropped from the set does not linger; the other channel's formulas
 * stay as they are. The commit goes on the tip of the tap's branch, which
 * moves to it only as a fast-forward, so a branch that moved meanwhile fails
 * rather than lose what it gained. When the tap already holds exactly these
 * formulas, nothing is committed. Resolves to `{ outcome }`, 'current' or
 * 'committed', with the commit's `sha`.
 *
 * With `check`, it stops once it has written the tree, which takes the same
 * right to write the tap as the commit: a run can thus find a token that
 * can no longer write the tap before it publishes anything the formulas
 * point at. The tree is named by its content, and nothing points at it.
 * Resolves to `{ outcome: 'checked' }`.
 */
export async function commitFormulas({ api, token, channel, version, formulas, check = false, tap = TAP, branch = BRANCH, ...options }) {
  if (!Object.hasOwn(MESSAGES, channel)) throw new Error(`no channel ${JSON.stringify(channel)}; it is nightly or stable`);
  if (formulas.length === 0) throw new Error(`no ${channel} formula to commit`);
  const strays = formulas.filter(({ name }) => !belongsTo(channel, name)).map(({ name }) => name);
  if (strays.length > 0) throw new Error(`${strays.join(', ')} would not be the ${channel} channel's in the tap`);

  const repos = `${api}/repos/${tap}`;
  const context = { token, ...options };
  const { object } = await answer(`Reading ${tap}'s ${branch}`, `${repos}/git/ref/heads/${branch}`, context);
  const head = object.sha;
  const { tree } = await answer(`Reading ${tap}'s commit ${head}`, `${repos}/git/commits/${head}`, context);
  const root = await answer(`Reading the tree of ${head}`, `${repos}/git/trees/${tree.sha}`, context);
  const dir = root.tree.find(({ path: name, type }) => name === FORMULA && type === 'tree');
  const present = dir ? (await answer(`Reading ${FORMULA} at ${head}`, `${repos}/git/trees/${dir.sha}`, context)).tree : [];

  const rendered = new Set(formulas.map(({ name }) => name));
  const gone = present.filter(({ path: name }) => belongsTo(channel, name) && !rendered.has(name));
  const entries = [
    ...gone.map(({ path: name, mode }) => ({ path: `${FORMULA}/${name}`, mode, type: 'blob', sha: null })),
    ...formulas.map(({ name, content }) => ({ path: `${FORMULA}/${name}`, mode: '100644', type: 'blob', content })),
  ];
  // A tree is named by its content, so one equal to the commit's own holds
  // the formulas already.
  const made = await answer(`Writing the tree with the ${channel} formulas`, `${repos}/git/trees`, {
    ...context,
    json: { base_tree: tree.sha, tree: entries },
    ...REPEATABLE,
  });
  if (check) {
    io.stdout(`The token can write ${tap}: it wrote the tree of the ${channel} formulas, ${made.sha}. Nothing is committed yet.\n`);
    return { outcome: 'checked' };
  }
  if (made.sha === tree.sha) {
    io.stdout(`${tap} already holds these ${channel} formulas; nothing to commit.\n`);
    return { outcome: 'current' };
  }
  const commit = await answer(`Committing the ${channel} formulas`, `${repos}/git/commits`, {
    ...context,
    json: { message: MESSAGES[channel](version), tree: made.sha, parents: [head], author: BOT, committer: BOT },
    ...REPEATABLE,
  });
  await github(`Moving ${tap}'s ${branch} to ${commit.sha}`, `${repos}/git/refs/heads/${branch}`, {
    ...context,
    method: 'PATCH',
    json: { sha: commit.sha, force: false },
  });
  io.stdout(`Committed ${formulas.length} ${channel} formula(s) to ${tap} as ${commit.sha}, deleting ${gone.length}.\n`);
  return { outcome: 'committed', sha: commit.sha };
}
