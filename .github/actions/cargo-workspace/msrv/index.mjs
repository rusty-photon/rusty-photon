import { io, run } from '../../lib/helpers.mjs';
import { verifyMsrv, workspacePackages } from '../workspace.mjs';

// The runner stops a cancelled step with SIGINT, then SIGTERM 7.5 s later and
// a kill 2.5 s after that (actions/runner, ProcessInvoker.cs), and on Linux
// the kill reaches Node alone: either signal stops the cargo running.
const stop = new AbortController();
for (const name of ['SIGINT', 'SIGTERM']) process.on(name, () => stop.abort());

await run(async () => {
  const verified = await verifyMsrv(await workspacePackages({ signal: stop.signal }), { signal: stop.signal });
  io.stdout(`${verified} package(s) build with their rust-version\n`);
});
