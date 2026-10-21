import os from 'node:os';

import { getInput, run } from '../lib/helpers.mjs';
import { main } from './run.mjs';

// The runner stops a cancelled step, one that timed out included, with
// SIGINT, then SIGTERM 7.5 s later and a kill 2.5 s after that (actions/runner,
// ProcessInvoker.cs): either signal stops the suites, and the logs are printed
// before the kill.
const stop = new AbortController();
for (const name of ['SIGINT', 'SIGTERM']) process.on(name, () => stop.abort());

await run(() =>
  main({
    services: getInput('services', { required: true }),
    tmp: process.env.RUNNER_TEMP || os.tmpdir(),
    signal: stop.signal,
  }),
);
