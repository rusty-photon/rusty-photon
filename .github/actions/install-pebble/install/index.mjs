import { getInput, run } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { install, resolve } from '../pebble.mjs';

await run(async () => {
  const workspace = process.env.GITHUB_WORKSPACE;
  if (!workspace) throw new Error('GITHUB_WORKSPACE is not set: this runs only as a GitHub Actions step');
  const { assets } = resolve({ runner: runnerKey() });
  await install({ assets, workspace, cached: getInput('cache-hit') === 'true' });
});
