import { readFileSync } from 'node:fs';

import { getInput, run, setOutput } from '../../lib/helpers.mjs';
import { checkTag } from '../release.mjs';

await run(async () => {
  setOutput('version', checkTag(getInput('tag', { required: true }), readFileSync('Cargo.toml', 'utf8')));
});
