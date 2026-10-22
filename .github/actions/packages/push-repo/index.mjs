import { getInput, run } from '../../lib/helpers.mjs';
import { bucketStore, publishSite } from '../r2.mjs';

await run(async () => {
  const store = bucketStore({
    endpoint: `https://${getInput('account-id', { required: true })}.r2.cloudflarestorage.com`,
    bucket: getInput('bucket', { required: true }),
    accessKeyId: getInput('access-key-id', { required: true }),
    secretAccessKey: getInput('secret-access-key', { required: true }),
  });
  await publishSite({ site: getInput('site', { required: true }), store });
});
