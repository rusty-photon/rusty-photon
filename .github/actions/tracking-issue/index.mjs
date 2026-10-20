import { runContext } from '../lib/github.mjs';
import { getInput, run, setOutput } from '../lib/helpers.mjs';
import { trackFailure } from './issue.mjs';

await run(async () => {
  const { number } = await trackFailure({
    ...runContext(),
    token: getInput('token', { required: true }),
    title: getInput('title', { required: true }),
    label: getInput('label', { required: true }),
    body: getInput('body', { required: true }),
  });
  setOutput('number', String(number));
});
