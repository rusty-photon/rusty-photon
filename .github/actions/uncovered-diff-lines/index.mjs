import { mkdtemp } from 'node:fs/promises';
import path from 'node:path';

import { runContext } from '../lib/github.mjs';
import { getInput, run } from '../lib/helpers.mjs';
import { annotate } from './annotate.mjs';

await run(async () => {
  await annotate({
    ...runContext(),
    lcov: getInput('lcov', { required: true }),
    pr: getInput('pull-request', { required: true }),
    sha: getInput('sha', { required: true }),
    token: getInput('token', { required: true }),
    dir: await mkdtemp(path.join(process.env.RUNNER_TEMP, 'uncovered-diff-lines-')),
  });
});
