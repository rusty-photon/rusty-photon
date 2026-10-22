import { getInput, run } from '../../lib/helpers.mjs';
import { dotRpmNames, writeSums } from '../assets.mjs';

await run(async () => {
  const dir = getInput('dir', { required: true });
  await dotRpmNames(dir);
  await writeSums(dir);
});
