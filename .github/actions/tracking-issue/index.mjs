import { runContext } from '../lib/github.mjs';
import { getInput, run, setOutput } from '../lib/helpers.mjs';
import { runAttempt, trackFailure } from './issue.mjs';

await run(async () => {
  const issue = await trackFailure({
    ...runContext(),
    ...runAttempt(),
    token: getInput('token', { required: true }),
    title: getInput('title', { required: true }),
    label: getInput('label', { required: true }),
    body: getInput('body', { required: true }),
    job: getInput('job'),
  });
  setOutput('number', issue ? String(issue.number) : '');
});
