import { getInput, run } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { install, resolve } from '../omnisim.mjs';

await run(async () => {
  const workspace = process.env.GITHUB_WORKSPACE;
  if (!workspace) throw new Error('GITHUB_WORKSPACE is not set: this runs only as a GitHub Actions step');
  const { asset, binary } = resolve({
    repo: getInput('repo', { required: true }),
    version: getInput('version', { required: true }),
    runner: runnerKey(),
  });
  await install({ asset, binary, workspace, cached: getInput('cache-hit') === 'true' });
});
