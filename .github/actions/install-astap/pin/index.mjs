import { getInput, io, run, setOutput } from '../../lib/helpers.mjs';
import { pinOf, TABLE } from '../astap.mjs';

await run(async () => {
  const sha = pinOf(TABLE, getInput('runner', { required: true }), getInput('archive', { required: true }));
  io.stdout(`Pinned SHA: ${sha}\n`);
  setOutput('sha', sha);
});
