import { getInput, run } from '../../lib/helpers.mjs';
import { renderSite } from '../repo.mjs';
import { verifyRepo } from '../verify-repo.mjs';

await run(async () => {
  const site = getInput('site', { required: true });
  const temp = process.env.RUNNER_TEMP;
  await renderSite({
    dist: getInput('dist', { required: true }),
    site,
    pubkey: getInput('pubkey', { required: true }),
    key: getInput('signing-key', { required: true }),
    temp,
  });
  await verifyRepo({ site, service: getInput('service', { required: true }), temp });
});
