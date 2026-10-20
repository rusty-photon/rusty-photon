import { getBooleanInput, getInput, run, setOutput } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { install, resolve } from '../zwo-sdk.mjs';

await run(async () => {
  const workspace = process.env.GITHUB_WORKSPACE;
  const temp = process.env.RUNNER_TEMP;
  if (!workspace || !temp) throw new Error('GITHUB_WORKSPACE or RUNNER_TEMP is not set: this runs only as a GitHub Actions step');
  const downloaded = await install({
    ...resolve({ runner: runnerKey() }),
    workspace,
    temp,
    sudo: getBooleanInput('sudo'),
    cached: getInput('cache-hit') === 'true',
    restored: getInput('cache-matched-key'),
  });
  setOutput('downloaded', String(downloaded));
});
