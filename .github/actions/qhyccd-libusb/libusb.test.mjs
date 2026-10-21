import assert from 'node:assert/strict';
import { lstatSync, mkdirSync, mkdtempSync, readlinkSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { LIBUSB, linkLibusb } from './libusb.mjs';

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'qhyccd-libusb-'));
}

test('the link is the linker name -lusb-1.0 asks for, to the runtime libusb-1.0-0 has', () => {
  assert.deepEqual(LIBUSB, ['libusb-1.0.so.0', 'libusb-1.0.so', 'libusb-1.0-0']);
});

test('linkLibusb links libusb-1.0.so in the SDK directory to the runtime the loader finds', {
  skip: process.platform === 'win32' && 'the runner without sudo is Linux',
}, async () => {
  const sdk = tempDir();
  const system = tempDir();
  writeFileSync(path.join(system, 'libusb-1.0.so.0'), 'libusb');
  const asked = [];
  const find = async (soname) => {
    asked.push(soname);
    return path.join(system, soname);
  };
  const { stdout } = await capture(() => linkLibusb(sdk, { find }));
  assert.deepEqual(asked, ['libusb-1.0.so.0']);
  assert.ok(lstatSync(path.join(sdk, 'libusb-1.0.so')).isSymbolicLink());
  assert.equal(readlinkSync(path.join(sdk, 'libusb-1.0.so')), path.join(system, 'libusb-1.0.so.0'));
  assert.equal(stdout, `libusb-1.0.so -> ${path.join(system, 'libusb-1.0.so.0')}\n`);
});

test('linkLibusb fails when the SDK step exported no QHYCCD_SDK_DIR, looking for nothing', async () => {
  for (const unset of [undefined, '']) {
    let looked = false;
    const find = async () => {
      looked = true;
    };
    await assert.rejects(linkLibusb(unset, { find }), { message: 'QHYCCD_SDK_DIR is not set: the QHYCCD SDK step must run first' });
    assert.equal(looked, false);
  }
});

test('linkLibusb names the package to install when the runtime is not on the machine', async () => {
  await assert.rejects(linkLibusb(tempDir(), { find: async () => undefined }), /sudo apt-get install -y libusb-1\.0-0$/);
});
