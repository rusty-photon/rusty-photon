import { getBooleanInput, getInput, run } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { install, resolve, staggerSeconds } from '../astap.mjs';

await run(async () => {
  const workspace = process.env.GITHUB_WORKSPACE;
  if (!workspace) throw new Error('GITHUB_WORKSPACE is not set: this runs only as a GitHub Actions step');
  const stagger = staggerSeconds(getInput('download-stagger-seconds'));
  const { cli, database } = resolve({ runner: runnerKey() });
  await install({
    workspace,
    cli,
    database: getBooleanInput('download-database') ? database : undefined,
    cliCached: getInput('cli-cache-hit') === 'true',
    databaseCached: getInput('database-cache-hit') === 'true',
    stagger,
  });
});
