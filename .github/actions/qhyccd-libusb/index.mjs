import { run } from '../lib/helpers.mjs';
import { linkLibusb } from './libusb.mjs';

await run(async () => {
  await linkLibusb(process.env.QHYCCD_SDK_DIR);
});
