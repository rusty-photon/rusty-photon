// F1 proof: an entry file no test imports, with a syntax error.
import { run } from '../lib/helpers.mjs';

await run(async () => {
  const missing = ;
});
