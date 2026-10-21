import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

import { run } from '../lib/helpers.mjs';
import { configure } from './remote.mjs';

await run(async () => {
  await configure({
    env: process.env,
    workspace: process.env.GITHUB_WORKSPACE || process.cwd(),
    runnerTemp: process.env.RUNNER_TEMP || tmpdir(),
    node: process.execPath,
    helper: fileURLToPath(new URL('./helper.mjs', import.meta.url)),
  });
});
