import path from 'node:path';

import { runContext } from '../../lib/github.mjs';
import { getBooleanInput, getInput, run } from '../../lib/helpers.mjs';
import { publish, readResults } from '../publish.mjs';

await run(async () => {
  await publish({
    ...runContext(),
    token: getInput('token', { required: true }),
    results: await readResults(path.resolve(getInput('results', { required: true }))),
    dryRun: getBooleanInput('dry-run'),
  });
});
