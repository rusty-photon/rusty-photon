import { homedir, tmpdir } from 'node:os';
import path from 'node:path';

import { getInput, run } from '../lib/helpers.mjs';
import { lanCrates } from './crates.mjs';

await run(async () => {
  await lanCrates({
    // The pool runner's .env sets it; a hosted runner has none.
    storeUrl: process.env.RP_LAN_CACHE_URL ?? '',
    writeAuth: getInput('write-auth'),
    lockPath: path.resolve(getInput('lockfile', { required: true })),
    dir: path.join(process.env.RUNNER_TEMP || tmpdir(), 'lan-crates'),
    cargoHome: process.env.CARGO_HOME || path.join(homedir(), '.cargo'),
  });
});
