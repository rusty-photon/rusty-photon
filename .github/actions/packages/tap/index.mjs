import { runContext } from '../../lib/github.mjs';
import { getBooleanInput, getInput, run } from '../../lib/helpers.mjs';
import { commitFormulas, readFormulas } from '../tap.mjs';

await run(async () => {
  await commitFormulas({
    api: runContext().api,
    token: getInput('token', { required: true }),
    channel: getInput('channel', { required: true }),
    version: getInput('version', { required: true }),
    formulas: await readFormulas(getInput('formulas', { required: true })),
    check: getBooleanInput('check'),
  });
});
