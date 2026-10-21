// The last step of repin-bazel.yml, once the lock it refreshed on a
// dependabot pull request's branch resolves: a commit of that
// MODULE.bazel.lock on the branch, made through the GitHub API, or none when
// the branch already holds it. The job holds no credential on disk, and the
// token reaches this step alone. Conventions: docs/skills/ci-logic.md.

import { createHash } from 'node:crypto';

import { github } from '../lib/github.mjs';
import { io } from '../lib/helpers.mjs';

/** The lock's path in the repository. */
export const LOCK = 'MODULE.bazel.lock';

// Who the commit is by, as the job's push from a checkout had it.
const BOT = { name: 'github-actions[bot]', email: '41898282+github-actions[bot]@users.noreply.github.com' };

// The writes a repeat leaves as they were, which fetchWithRetry may send again
// after a 5xx or no answer: a blob or a tree is named by its content, and a
// second commit is one more commit nothing points at. The branch moves once:
// a repeat after GitHub had moved it is no fast-forward from where it was.
const REPEATABLE = { attempts: 3 };

/** The SHA-1 git names `bytes` by as a blob, as `git hash-object` gives it. */
export function blobSha(bytes) {
  return createHash('sha1').update(`blob ${bytes.length}\0`).update(bytes).digest('hex');
}

// The JSON answer to a request of the GitHub API (github).
async function answer(what, url, options) {
  return (await github(what, url, options)).json();
}

/**
 * Commits `lock`, the bytes of the refreshed MODULE.bazel.lock, on `branch`
 * of `repo`, through the GitHub API at `api` with `token`, on `head`, the
 * commit the job checked out. The branch moves to the commit only as a
 * fast-forward, so a branch that moved since fails rather than lose what it
 * gained. When `head` already holds the lock, nothing is written. Resolves
 * to `{ outcome }`, 'current' or 'committed', with the commit's `sha`.
 */
export async function commitRepin({ api, repo, token, branch, head, lock, ...options }) {
  if (!/^[0-9a-f]{40}$/.test(head)) throw new Error(`the checked-out commit, ${JSON.stringify(head)}, is not a SHA-1 in lowercase hex`);
  if (!branch) throw new Error('no branch to commit on');
  const repos = `${api}/repos/${repo}`;
  const context = { token, ...options };
  const { tree } = await answer(`Reading the commit ${head}`, `${repos}/git/commits/${head}`, context);
  const { tree: entries } = await answer(`Reading the tree of ${head}`, `${repos}/git/trees/${tree.sha}`, context);
  const entry = entries.find(({ path }) => path === LOCK);
  if (entry === undefined) throw new Error(`${head} has no ${LOCK}`);
  const sha = blobSha(lock);
  if (entry.sha === sha) {
    io.stdout(`${LOCK} already in sync; nothing to push.\n`);
    return { outcome: 'current' };
  }

  const blob = await answer(`Writing ${LOCK}`, `${repos}/git/blobs`, {
    ...context,
    json: { content: lock.toString('base64'), encoding: 'base64' },
    timeoutMs: 120_000,
    ...REPEATABLE,
  });
  if (blob.sha !== sha) throw new Error(`GitHub stored ${LOCK} as the blob ${blob.sha}, not ${sha}, which its bytes hash to`);
  const madeTree = await answer(`Writing the tree with the new ${LOCK}`, `${repos}/git/trees`, {
    ...context,
    json: { base_tree: tree.sha, tree: [{ path: LOCK, mode: entry.mode, type: 'blob', sha }] },
    ...REPEATABLE,
  });
  const commit = await answer(`Committing the new ${LOCK}`, `${repos}/git/commits`, {
    ...context,
    json: { message: `chore(bazel): repin ${LOCK} for ${branch}`, tree: madeTree.sha, parents: [head], author: BOT, committer: BOT },
    ...REPEATABLE,
  });
  const ref = branch.split('/').map(encodeURIComponent).join('/');
  await github(`Moving ${branch} to ${commit.sha}`, `${repos}/git/refs/heads/${ref}`, {
    ...context,
    method: 'PATCH',
    json: { sha: commit.sha, force: false },
  });
  io.stdout(`Committed ${commit.sha} on ${head}, and moved ${branch} to it\n`);
  return { outcome: 'committed', sha: commit.sha };
}
