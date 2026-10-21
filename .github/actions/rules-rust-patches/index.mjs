import { run } from '../lib/helpers.mjs';
import { checkPatches } from './patches.mjs';

// The runner stops a cancelled step with SIGINT, then SIGTERM 7.5 s later and
// a kill 2.5 s after that (actions/runner, ProcessInvoker.cs), and on Linux
// the kill reaches Node alone: either signal interrupts Bazel.
const stop = new AbortController();
for (const name of ['SIGINT', 'SIGTERM']) process.on(name, () => stop.abort());

await run(async () => {
  await checkPatches({ signal: stop.signal });
});
