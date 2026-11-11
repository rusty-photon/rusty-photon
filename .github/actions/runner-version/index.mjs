import { readFile } from 'node:fs/promises';

import { runContext } from '../lib/github.mjs';
import { fail, getInput, notice, run, setOutput } from '../lib/helpers.mjs';
import { assess, parseGraceDays, parseVersion, releases, report } from './check.mjs';

await run(async () => {
  const pinned = parseVersion(await readFile(getInput('version-file', { required: true }), 'utf8'));
  const graceDays = parseGraceDays(getInput('grace-days', { required: true }));
  const { api } = runContext();
  const published = await releases({ api, token: getInput('token') });
  const assessment = assess({ pinned, published, now: new Date(), graceDays });
  setOutput('latest', assessment.status === 'current' ? pinned : assessment.latest.version);
  if (assessment.status === 'behind' && assessment.due) fail(report(assessment));
  else notice(report(assessment));
});
