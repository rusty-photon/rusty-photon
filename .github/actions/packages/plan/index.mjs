import { readFileSync } from 'node:fs';

import { runContext } from '../../lib/github.mjs';
import { command, getInput, run, setOutput, withoutRunnerVariables } from '../../lib/helpers.mjs';
import { plan } from '../plan.mjs';

await run(async () => {
  const outputs = await plan({
    ...runContext(),
    token: getInput('token', { required: true }),
    head: (await command('git', ['rev-parse', 'HEAD'], { env: withoutRunnerVariables(process.env) })).trim(),
    cargoToml: readFileSync('Cargo.toml', 'utf8'),
  });
  for (const [name, value] of Object.entries(outputs)) setOutput(name, value);
});
