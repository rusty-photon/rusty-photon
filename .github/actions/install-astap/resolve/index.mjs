import { run, setOutput } from '../../lib/helpers.mjs';
import { runnerKey } from '../../lib/install.mjs';
import { resolve } from '../astap.mjs';

await run(async () => {
  const { keys } = resolve({ runner: runnerKey() });
  setOutput('cli-key', keys.cli);
  setOutput('database-key', keys.database);
});
