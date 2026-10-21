import { readFileSync } from 'node:fs';

import { runContext } from '../lib/github.mjs';
import { command, getInput, run } from '../lib/helpers.mjs';
import { LOCK, commitRepin } from './repin.mjs';

await run(async () => {
  await commitRepin({
    ...runContext(),
    token: getInput('token', { required: true }),
    branch: getInput('branch', { required: true }),
    head: (await command('git', ['rev-parse', 'HEAD'])).trim(),
    lock: readFileSync(LOCK),
  });
});
