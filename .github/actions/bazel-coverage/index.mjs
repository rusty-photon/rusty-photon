import { fail, io, run, setOutput } from '../lib/helpers.mjs';
import { ARGS, combinedReport, failure, runBazel } from './coverage.mjs';

// The runner stops a cancelled step with SIGINT, then SIGTERM 7.5 s later and
// a kill 2.5 s after that (actions/runner, ProcessInvoker.cs), and on Linux
// the kill reaches Node alone: either signal interrupts Bazel.
const stop = new AbortController();
for (const name of ['SIGINT', 'SIGTERM']) process.on(name, () => stop.abort());

await run(async () => {
  const why = failure(await runBazel(ARGS, { signal: stop.signal }));
  if (why) {
    fail(why);
    return;
  }
  const report = await combinedReport();
  io.stdout(`Combined report: ${report}\n`);
  setOutput('report', report);
});
