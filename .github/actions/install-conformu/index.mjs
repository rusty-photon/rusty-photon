import path from 'node:path';

import { exportVariable, getInput, run, setOutput } from '../lib/helpers.mjs';
import { runnerKey } from '../lib/install.mjs';
import { installAt, resolve, windowsDir } from './conformu.mjs';

await run(async () => {
  const temp = process.env.RUNNER_TEMP;
  if (!temp) throw new Error('RUNNER_TEMP is not set: this runs only as a GitHub Actions step');
  const resolved = await resolve({ runner: runnerKey(), token: getInput('token') });
  const dir = resolved.kind === 'installer' ? windowsDir() : path.join(temp, 'conformu');
  for (const [name, value] of Object.entries(await installAt(dir, resolved))) exportVariable(name, value);
  setOutput('version', resolved.version);
});
