import { run, setOutput } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { resolve } from '../svbony-sdk.mjs';

await run(async () => {
  setOutput('key', resolve({ runner: runnerKey() }).key);
});
