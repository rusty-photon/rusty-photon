import { getBooleanInput, getInput, run } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { install, resolve } from '../svbony-sdk.mjs';

await run(async () => {
  const workspace = process.env.GITHUB_WORKSPACE;
  const temp = process.env.RUNNER_TEMP;
  if (!workspace || !temp) throw new Error('GITHUB_WORKSPACE or RUNNER_TEMP is not set: this runs only as a GitHub Actions step');
  await install({
    ...resolve({ runner: runnerKey() }),
    workspace,
    temp,
    sudo: getBooleanInput('sudo'),
    cached: getInput('cache-hit') === 'true',
    token: getInput('windows-mirror-token'),
  });
});
