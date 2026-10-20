import { run, setOutput } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { resolve } from '../zwo-sdk.mjs';

await run(async () => {
  const { key, restoreKey } = resolve({ runner: runnerKey() });
  setOutput('key', key);
  setOutput('restore-key', restoreKey);
});
