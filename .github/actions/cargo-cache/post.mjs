import { homedir, tmpdir } from 'node:os';
import path from 'node:path';

import { getInput, run } from '../lib/helpers.mjs';
import { post } from './cache.mjs';

await run(async () => {
  await post({
    storeUrl: process.env.RP_LAN_CACHE_URL ?? '',
    writeAuth: getInput('write-auth'),
    workspace: process.env.GITHUB_WORKSPACE || process.cwd(),
    cargoHome: process.env.CARGO_HOME || path.join(homedir(), '.cargo'),
    runnerTemp: process.env.RUNNER_TEMP || tmpdir(),
    eventPath: process.env.GITHUB_EVENT_PATH ?? '',
    ref: process.env.GITHUB_REF ?? '',
    sha: process.env.GITHUB_SHA ?? '',
    run: process.env.GITHUB_RUN_ID ?? '',
    apiUrl: process.env.GITHUB_API_URL || 'https://api.github.com',
    repository: process.env.GITHUB_REPOSITORY ?? '',
    token: getInput('github-token'),
  });
});
