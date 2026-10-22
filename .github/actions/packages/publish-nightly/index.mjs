import { runContext } from '../../lib/github.mjs';
import { getInput, run } from '../../lib/helpers.mjs';
import { publishNightly } from '../publish.mjs';

await run(async () => {
  const { api, repo } = runContext();
  await publishNightly({
    api,
    repo,
    token: getInput('token', { required: true }),
    sha: getInput('sha', { required: true }),
    date: getInput('date', { required: true }),
    version: getInput('version', { required: true }),
    dir: getInput('dir', { required: true }),
  });
});
