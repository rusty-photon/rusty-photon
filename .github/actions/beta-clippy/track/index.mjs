import { runContext } from '../../lib/github.mjs';
import { getInput, run } from '../../lib/helpers.mjs';
import { readCensus, track } from '../track.mjs';

await run(async () => {
  await track({
    ...runContext(),
    token: getInput('token', { required: true }),
    census: readCensus(getInput('census', { required: true })),
  });
});
