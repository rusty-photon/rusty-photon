import { getBooleanInput, getInput, run } from '../lib/helpers.mjs';
import { report } from './times.mjs';

await run(async () => {
  await report({
    csv: getInput('csv', { required: true }),
    os: getInput('os', { required: true }),
    check: getBooleanInput('check'),
  });
});
