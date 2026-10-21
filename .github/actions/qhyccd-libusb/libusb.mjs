// pi-nightly.yml's libusb link for the QHYCCD SDK on the sudo-less Pi. The
// static libqhyccd.a pulls in a dynamic `-lusb-1.0`, whose linker name,
// libusb-1.0.so, only the -dev package has, and libqhyccd-sys's build script
// adds QHYCCD_SDK_DIR alone to the link search. So a link of that name there,
// to the system's libusb runtime, stands in for the -dev package, as
// install-zwo-sdk's links do for libzwo-sys. Conventions:
// docs/skills/ci-logic.md.

import { linkRuntime } from '../lib/install.mjs';

/** libusb's runtime soname, the linker's name for it, and the package that has the runtime. */
export const LIBUSB = ['libusb-1.0.so.0', 'libusb-1.0.so', 'libusb-1.0-0'];

/**
 * Links libusb's linker name in `sdkDir`, the QHYCCD_SDK_DIR the SDK's
 * install exported, to the runtime library. Throws when `sdkDir` is not set,
 * and when the runtime is not on the machine, naming its package.
 */
export async function linkLibusb(sdkDir, options = {}) {
  if (!sdkDir) throw new Error('QHYCCD_SDK_DIR is not set: the QHYCCD SDK step must run first');
  await linkRuntime(sdkDir, [LIBUSB], options);
}
