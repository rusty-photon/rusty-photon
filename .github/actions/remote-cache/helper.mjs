// Bazel's credential helper for the remote cache, run by the launcher the
// remote-cache action writes, in every Bazel step, with that step's
// environment. Not the helpers' `run`: what this prints is Bazel's to parse,
// so a failure goes to stderr, which Bazel shows, and the exit status.

import { text } from 'node:stream/consumers';

import { answer } from './remote.mjs';

try {
  process.stdout.write(JSON.stringify(answer(process.argv.slice(2), await text(process.stdin), process.env)));
} catch (err) {
  process.stderr.write(`remote-cache credential helper: ${err.message}\n`);
  process.exitCode = 1;
}
