import { homedir, tmpdir } from 'node:os';
import path from 'node:path';

import { getInput, run } from '../lib/helpers.mjs';
import { main } from './cache.mjs';

await run(async () => {
  await main({
    // The pool runner's .env sets it; a hosted runner has none.
    storeUrl: process.env.RP_LAN_CACHE_URL ?? '',
    writeAuth: getInput('write-auth'),
    workspace: process.env.GITHUB_WORKSPACE || process.cwd(),
    cargoHome: process.env.CARGO_HOME || path.join(homedir(), '.cargo'),
    runnerTemp: process.env.RUNNER_TEMP || tmpdir(),
    job: process.env.GITHUB_JOB ?? '',
  });
});
