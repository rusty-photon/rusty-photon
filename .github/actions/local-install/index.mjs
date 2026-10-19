import { main } from './local.mjs';

process.exitCode = await main(process.argv.slice(2));
