import { mkdtemp } from 'node:fs/promises';
import path from 'node:path';

import { runContext } from '../lib/github.mjs';
import { getInput, run } from '../lib/helpers.mjs';
import { annotate } from './annotate.mjs';

// The runner stops a cancelled step with SIGINT, then SIGTERM 7.5 s later and
// a kill 2.5 s after that (actions/runner, ProcessInvoker.cs), and on Linux
// the kill reaches Node alone: either signal stops the program running.
const stop = new AbortController();
for (const name of ['SIGINT', 'SIGTERM']) process.on(name, () => stop.abort());

await run(async () => {
  await annotate({
    ...runContext(),
    lcov: getInput('lcov', { required: true }),
    pr: getInput('pull-request', { required: true }),
    sha: getInput('sha', { required: true }),
    token: getInput('token', { required: true }),
    dir: await mkdtemp(path.join(process.env.RUNNER_TEMP, 'uncovered-diff-lines-')),
    signal: stop.signal,
  });
});
