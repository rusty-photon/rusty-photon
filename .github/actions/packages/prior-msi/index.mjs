import path from 'node:path';

import { runContext } from '../../lib/github.mjs';
import { getInput, run, setOutput } from '../../lib/helpers.mjs';
import { fetchPriorMsi } from '../prior.mjs';

await run(async () => {
  const file = await fetchPriorMsi({
    ...runContext(),
    token: getInput('token', { required: true }),
    dir: path.join(process.env.RUNNER_TEMP, 'prior-nightly'),
  });
  if (file !== undefined) setOutput('path', file);
});
