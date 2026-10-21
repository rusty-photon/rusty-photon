// The coverage job's `uncovered-diff-lines` check on a pull request's head
// (bazel-coverage.yml): diff-cover scores the pull request's diff against the
// job's coverage report, and tools/coverage/post_check_annotations.py posts
// what it finds as the check run, with an annotation on each run of uncovered
// added lines. Main's ruleset requires the check, and a required check that
// never reports blocks the merge, so a diff that could not be scored still
// gets the check, neutral, naming the stage that failed. Conventions:
// docs/skills/ci-logic.md.

import { writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { github } from '../lib/github.mjs';
import { describe, execute, warning, withoutRunnerVariables } from '../lib/helpers.mjs';

/** The check run's name, which main's ruleset requires. */
export const CHECK = 'uncovered-diff-lines';

/**
 * What pip installs. The Linux pool template has this pin installed for the
 * `ci` user, so there the install is an offline no-op, and a new pin needs a
 * template rebuild (docs/skills/proxmox-runner-pool.md).
 */
export const DIFF_COVER = 'diff-cover==10.5.1';

/** The script that turns diff-cover's findings into the check run. */
export const POSTER = fileURLToPath(new URL('../../../tools/coverage/post_check_annotations.py', import.meta.url));

/**
 * Scores pull request `pr`'s diff against the LCOV report `lcov` and posts
 * the check on its head `sha`, working in the directory `dir`. Each program
 * gets `env` without the runner's variables (withoutRunnerVariables), as the
 * `run:` step it replaces gave them, and only the poster gets the token.
 * Never throws: a stage that fails is a warning, with why, and the check is
 * posted neutral, naming the stage alone, since the check's text is public
 * and a program's error can hold a LAN address (a proxy's). When `signal`
 * aborts (the runner stopping the step), the program or request under way
 * is stopped and nothing is posted: the job is cancelled, and its required
 * `bazel coverage` with it. Other options go to the API requests.
 */
export async function annotate({ lcov, token, pr, sha, dir, api, repo, runUrl, env = process.env, spawn, signal, ...options }) {
  const base = withoutRunnerVariables(env);
  const run = (program, args, extra = {}) => execute(program, args, { env: { ...base, ...extra }, spawn, signal });
  let stage = 'installing diff-cover';
  try {
    await run('python3', ['-m', 'pip', 'install', '--user', '--quiet', DIFF_COVER]);
    // pip's --user scripts directory, which the runner's PATH does not have.
    const userBase = (await run('python3', ['-c', 'import site; print(site.USER_BASE)'])).trim();

    // The checkout is a shallow merge commit, with no base for `git diff`.
    // The API serves the merge-base diff, the lines the pull request
    // changes, and refuses one past its size limit.
    stage = "fetching the pull request's diff";
    const diff = path.join(dir, 'pr.diff');
    const response = await github(`Fetching pull request #${pr}'s diff`, `${api}/repos/${repo}/pulls/${pr}`, {
      token,
      accept: 'application/vnd.github.diff',
      signal,
      ...options,
    });
    await writeFile(diff, await response.text());

    stage = 'running diff-cover';
    const report = path.join(dir, 'diff-cover.json');
    await run(path.join(userBase, 'bin', 'diff-cover'), [
      lcov,
      '--diff-file',
      diff,
      '--format',
      `json:${report}`,
      '--total-percent-float',
      '--quiet',
    ]);

    stage = 'posting the check run';
    await run(
      'python3',
      [POSTER, report, '--diff', diff, '--lcov', lcov, '--repo', repo, '--sha', sha, '--details-url', runUrl],
      { GITHUB_TOKEN: token },
    );
  } catch (err) {
    if (signal?.aborted) {
      warning(`${CHECK}: the step was stopped while ${stage}, so no check is posted`);
      return;
    }
    warning(`${CHECK}: ${stage} failed, so the check is posted as not scored: ${describe(err)}`);
    await postUnscored({ stage, token, sha, api, repo, runUrl, ...options });
  }
}

// The check, neutral, saying that the diff was not scored and at which stage.
// A repeat of the request posts a second check run of the same words, which
// is harmless, so it is retried.
async function postUnscored({ stage, token, sha, api, repo, runUrl, ...options }) {
  try {
    await github(`Posting ${CHECK} as not scored`, `${api}/repos/${repo}/check-runs`, {
      token,
      attempts: 3,
      ...options,
      json: {
        name: CHECK,
        head_sha: sha,
        status: 'completed',
        conclusion: 'neutral',
        details_url: runUrl,
        output: {
          title: `Not scored: ${stage} failed`,
          summary:
            `The coverage job could not score this pull request's diff: ${stage} failed. ` +
            'So this check says nothing about which added lines are covered. ' +
            `The [coverage run's log](${runUrl}) says why; re-running the coverage job scores the diff again.`,
        },
      },
    });
  } catch (err) {
    warning(`${CHECK}: could not post the check either, so this pull request lacks it: ${describe(err)}`);
  }
}
