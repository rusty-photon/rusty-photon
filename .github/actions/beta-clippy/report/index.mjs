import { run, setOutput } from '../../lib/helpers.mjs';
import { report } from '../report.mjs';

// The runner stops a cancelled step with SIGINT, then SIGTERM 7.5 s later and
// a kill 2.5 s after that (actions/runner, ProcessInvoker.cs), and on Linux
// the kill reaches Node alone: either signal stops the cargo running.
const stop = new AbortController();
for (const name of ['SIGINT', 'SIGTERM']) process.on(name, () => stop.abort());

await run(async () => {
  const census = await report({ signal: stop.signal });
  if (census !== undefined) setOutput('census', census);
});
