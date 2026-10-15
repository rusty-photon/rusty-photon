import assert from 'node:assert/strict';
import test from 'node:test';

import { paxRecord, seal, tar, tarHeader } from './fixtures.mjs';
import { untar } from './untar.mjs';

const summary = (entries) => entries.map(({ path, type, mode, data }) => ({ path, type, mode, data: data.toString() }));

test('untar reads the files and directories of a GNU archive, as cargo package writes one', () => {
  const archive = tar([
    { path: 'demo-1.0.0/', type: '5', mode: 0o755 },
    { path: 'demo-1.0.0/Cargo.toml', data: '[package]\n' },
    { path: 'demo-1.0.0/configure', data: '#!/bin/sh\n', mode: 0o755 },
    { path: 'demo-1.0.0/empty', data: '' },
  ]);
  assert.deepEqual(summary(untar(archive)), [
    { path: 'demo-1.0.0/', type: 'directory', mode: 0o755, data: '' },
    { path: 'demo-1.0.0/Cargo.toml', type: 'file', mode: 0o644, data: '[package]\n' },
    { path: 'demo-1.0.0/configure', type: 'file', mode: 0o755, data: '#!/bin/sh\n' },
    { path: 'demo-1.0.0/empty', type: 'file', mode: 0o644, data: '' },
  ]);
});

test('untar reads a file of more than one block, and the entry after it', () => {
  const big = 'x'.repeat(1300);
  const entries = untar(tar([{ path: 'a/big', data: big }, { path: 'a/next', data: 'n' }]));
  assert.equal(entries[0].data.toString(), big);
  assert.equal(entries[1].path, 'a/next');
  assert.equal(entries[1].data.toString(), 'n');
});

test('untar takes a path over 100 bytes from the GNU long-name entry before it', () => {
  const path = `demo-1.0.0/${'d'.repeat(60)}/${'f'.repeat(60)}.rs`;
  const entries = untar(tar([{ path, data: 'fn main() {}' }]));
  assert.deepEqual(summary(entries), [{ path, type: 'file', mode: 0o644, data: 'fn main() {}' }]);
});

test('untar joins a POSIX ustar prefix to the name', () => {
  const header = tarHeader({ name: 'src/lib.rs', size: 2, magic: 'posix', prefix: 'demo-1.0.0' });
  const archive = Buffer.concat([header, Buffer.from('ok'), Buffer.alloc(510), Buffer.alloc(1024)]);
  assert.deepEqual(summary(untar(archive)), [{ path: 'demo-1.0.0/src/lib.rs', type: 'file', mode: 0o644, data: 'ok' }]);
});

test("untar ignores a GNU header's prefix bytes, which are not a prefix there", () => {
  const header = tarHeader({ name: 'demo-1.0.0/a', size: 0 });
  header.write('not-a-prefix', 345, 'utf8');
  seal(header);
  assert.equal(untar(Buffer.concat([header, Buffer.alloc(1024)]))[0].path, 'demo-1.0.0/a');
});

test('untar takes a path from a pax header, and skips a global one', () => {
  const record = Buffer.from(paxRecord('path', 'demo-1.0.0/from-pax.rs'));
  const global = Buffer.from(paxRecord('comment', 'ignored'));
  const archive = tar([
    { path: 'pax_global_header', type: 'g', data: global },
    { path: 'PaxHeader/x', type: 'x', data: record },
    { path: 'demo-1.0.0/short', data: 'pax' },
    { path: 'demo-1.0.0/after', data: 'a' },
  ]);
  assert.deepEqual(
    untar(archive).map((e) => e.path),
    ['demo-1.0.0/from-pax.rs', 'demo-1.0.0/after'],
  );
});

test('untar refuses a link, which Cargo never packages', () => {
  for (const type of ['1', '2']) {
    assert.throws(() => untar(tar([{ path: 'demo-1.0.0/link', type }])), /"demo-1\.0\.0\/link" is of type "[12]", which is neither a file nor a directory/);
  }
});

test('untar refuses a header whose checksum does not match', () => {
  const header = tarHeader({ name: 'demo-1.0.0/a', size: 0, checksum: 1 });
  assert.throws(() => untar(Buffer.concat([header, Buffer.alloc(1024)])), /header checksum mismatch at offset 0/);
});

test('untar refuses an archive cut short', () => {
  const whole = tar([{ path: 'demo-1.0.0/a', data: 'x'.repeat(600) }]);
  assert.throws(() => untar(whole.subarray(0, 700)), /entry runs past the end of the archive/);
  assert.throws(() => untar(tar([{ path: 'demo-1.0.0/a', data: 'x' }], { end: false })), /ends without its end-of-archive blocks/);
});

test('untar refuses a size that is not octal', () => {
  const header = tarHeader({ name: 'demo-1.0.0/a' });
  header.write('99999999999\0', 124, 'latin1');
  seal(header);
  assert.throws(() => untar(Buffer.concat([header, Buffer.alloc(1024)])), /bad size field/);
});
