import os from 'node:os';
import path from 'node:path';

import { execute, fail, getInput, run, warning } from '../lib/helpers.mjs';
import { exitText } from '../lib/processes.mjs';
import { diagnose, trace, watch } from './probe.mjs';

// The runner stops a cancelled step, one that timed out included, with
// SIGINT, then SIGTERM 7.5 s later and a kill 2.5 s after that (actions/runner,
// ProcessInvoker.cs): either signal stops Bazel.
const stop = new AbortController();
for (const name of ['SIGINT', 'SIGTERM']) process.on(name, () => stop.abort());

await run(async () => {
  const args = getInput('args', { required: true }).split(/\s+/).filter(Boolean);
  const seconds = getInput('silence-seconds') || '300';
  if (!/^[1-9][0-9]*$/.test(seconds)) throw new Error(`silence-seconds must be a whole number of seconds; got ${JSON.stringify(seconds)}`);
  // Asked before the build: once it wedges, `bazel info` would queue behind it.
  let outputBase = null;
  try {
    outputBase = (await execute('bazel', ['info', 'output_base'], { echo: false })).trim();
  } catch (err) {
    warning(`bazel info output_base failed, so a dump will not reach the server: ${err.message}`);
  }
  // How much of jvm.out the dumps have printed, so each prints only what it gained.
  const printed = { jvmOut: 0 };
  // Started after `bazel info`, which starts the server, and before the build's client connects to it.
  const traced = trace({ outputBase, file: path.join(process.env.RUNNER_TEMP || os.tmpdir(), 'bazel-stall-probe.pcap') });
  let ended;
  try {
    ended = await watch({
      args,
      silenceMs: Number(seconds) * 1000,
      diagnose: (seen) => diagnose({ ...seen, outputBase, printed, trace: traced }),
      signal: stop.signal,
    });
  } finally {
    await traced?.stop();
  }
  if (ended.code !== 0) {
    fail(`bazel ${args[0]} ${exitText(ended)}`);
    process.exitCode = ended.code || 1;
  }
});
