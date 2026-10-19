import { getInput, io, run } from '../../lib/helpers.mjs';
import { readResults, readTable, repin, TABLE_FILE, writeTable } from '../astap.mjs';

await run(async () => {
  const { table, changes } = repin(readTable(), await readResults(getInput('results', { required: true })));
  for (const { runner, archive, from, to } of changes) io.stdout(`${runner} (${archive}): ${from} -> ${to}\n`);
  await writeTable(TABLE_FILE, table);
});
