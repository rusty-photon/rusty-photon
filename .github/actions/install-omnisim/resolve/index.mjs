import { getInput, run, setOutput } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { resolve } from '../omnisim.mjs';

await run(async () => {
  const { key } = resolve({
    repo: getInput('repo', { required: true }),
    version: getInput('version', { required: true }),
    runner: runnerKey(),
  });
  setOutput('key', key);
});
