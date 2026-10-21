import path from 'node:path';

import { run } from '../lib/helpers.mjs';
import { runnerKey } from '../lib/install.mjs';
import { install, lint, resolve } from './shellcheck.mjs';

await run(async () => {
  const temp = process.env.RUNNER_TEMP;
  if (!temp) throw new Error('RUNNER_TEMP is not set: this runs only as a GitHub Actions step');
  const shellcheck = await install(resolve({ runner: runnerKey() }), path.join(temp, 'shellcheck'));
  // An action runs in the checkout, which the scripts' paths start from.
  await lint({ shellcheck, root: process.cwd() });
});
