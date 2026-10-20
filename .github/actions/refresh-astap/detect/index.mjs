import { mkdtemp } from 'node:fs/promises';
import path from 'node:path';

import { getBooleanInput, getInput, run, setOutput } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { detect, FIXTURES } from '../detect.mjs';

await run(async () => {
  const workspace = process.env.GITHUB_WORKSPACE;
  const temp = process.env.RUNNER_TEMP;
  if (!workspace || !temp) throw new Error('GITHUB_WORKSPACE and RUNNER_TEMP are not set: this runs only as a GitHub Actions step');
  const { rotated, upstream } = await detect({
    runner: getInput('runner', { required: true }),
    verify: getBooleanInput('verify'),
    here: runnerKey(),
    work: await mkdtemp(path.join(temp, 'refresh-astap-')),
    results: path.resolve(workspace, getInput('results', { required: true })),
    fixtures: path.join(workspace, FIXTURES),
  });
  setOutput('rotated', String(rotated));
  setOutput('sha', upstream);
});
