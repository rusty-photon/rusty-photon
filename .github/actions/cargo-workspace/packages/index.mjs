import { getInput, run, setOutput } from '../../lib/helpers.mjs';
import { discover } from '../workspace.mjs';

await run(async () => {
  const found = await discover(getInput('table', { required: true }));
  setOutput('packages', JSON.stringify(found));
  setOutput('found', String(found.length > 0));
});
