// Test fixtures every action's tests share: reading what the helpers print,
// a loopback HTTP server to stand in for an API, a store or a download
// server, environment variables set for one test, and tar and zip archives
// built in memory, which need no system `tar` or `zip` and can be ones no
// packager would write. Imported only by tests (docs/skills/ci-logic.md
// §Tests).

import { createServer } from 'node:http';
import { crc32 } from 'node:zlib';

import { io } from './helpers.mjs';

/**
 * What `fn` writes through the helpers' `io`, then puts `io` back. Under
 * `node --test`, stdout also carries the runner's reports to its parent, so
 * a test never replaces `process.stdout.write` itself.
 */
export async function capture(fn) {
  const out = [];
  const err = [];
  const saved = { ...io };
  io.stdout = (text) => out.push(text);
  io.stderr = (text) => err.push(text);
  try {
    const value = await fn();
    return { value, stdout: out.join(''), stderr: err.join('') };
  } finally {
    Object.assign(io, saved);
  }
}

/**
 * Serves `handler(req, res, n)` on loopback, on a free port. Each request is
 * read whole first, its body put in `req.body`; `requests` lists every one
 * as `{ method, url, headers, body }`, the nth being request n.
 */
export async function serve(handler) {
  const requests = [];
  const server = createServer(async (req, res) => {
    const chunks = [];
    try {
      for await (const chunk of req) chunks.push(chunk);
    } catch {
      return; // the client went away mid-body
    }
    req.body = Buffer.concat(chunks);
    requests.push({ method: req.method, url: req.url, headers: req.headers, body: req.body });
    handler(req, res, requests.length);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const { port } = server.address();
  return {
    url: `http://127.0.0.1:${port}`,
    requests,
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections();
        server.close(resolve);
      }),
  };
}

/**
 * Sets the environment variables `vars` (undefined deletes one) for the
 * length of `fn`, then puts back what was there.
 */
export async function withEnv(vars, fn) {
  const saved = Object.fromEntries(Object.keys(vars).map((k) => [k, process.env[k]]));
  const assign = (values) => {
    for (const [k, v] of Object.entries(values)) {
      if (v === undefined) delete process.env[k];
      else process.env[k] = v;
    }
  };
  assign(vars);
  try {
    return await fn();
  } finally {
    assign(saved);
  }
}

// --- Tar archives ---------------------------------------------------------------

const BLOCK = 512;

function field(header, start, length, value, encoding = 'utf8') {
  header.write(value, start, length, encoding);
}

function octal(value, width) {
  return `${value.toString(8).padStart(width - 1, '0')}\0`;
}

/**
 * One tar header. `magic` is GNU's 'ustar  \0' (what `cargo package` writes)
 * or POSIX's 'ustar\0', which comes with the version '00' and a `prefix`.
 */
export function tarHeader({ name, size = 0, type = '0', mode = 0o644, mtime = 0, magic = 'gnu', prefix = '', checksum }) {
  const header = Buffer.alloc(BLOCK);
  field(header, 0, 100, name);
  field(header, 100, 8, octal(mode, 8), 'latin1');
  field(header, 108, 8, octal(0, 8), 'latin1');
  field(header, 116, 8, octal(0, 8), 'latin1');
  field(header, 124, 12, octal(size, 12), 'latin1');
  field(header, 136, 12, octal(mtime, 12), 'latin1');
  field(header, 156, 1, type, 'latin1');
  if (magic === 'gnu') {
    field(header, 257, 8, 'ustar  \0', 'latin1');
  } else {
    field(header, 257, 6, 'ustar\0', 'latin1');
    field(header, 263, 2, '00', 'latin1');
    field(header, 345, 155, prefix);
  }
  return seal(header, checksum);
}

/**
 * Writes `header`'s checksum, the sum of its bytes with the field read as
 * spaces, or `checksum` in its place. For a test that edits a header.
 */
export function seal(header, checksum) {
  header.fill(0x20, 148, 156);
  let sum = 0;
  for (const byte of header) sum += byte;
  field(header, 148, 8, `${(checksum ?? sum).toString(8).padStart(6, '0')}\0 `, 'latin1');
  return header;
}

function padded(data) {
  const size = Math.ceil(data.length / BLOCK) * BLOCK;
  return Buffer.concat([data, Buffer.alloc(size - data.length)]);
}

/**
 * An uncompressed tar archive of `entries`: `{ path, data, type, mode, mtime }`,
 * or `{ raw }` for bytes placed as they are. A path over 100 bytes gets the
 * GNU long-name entry `cargo package` writes. The end-of-archive blocks
 * close it unless `end` is false.
 */
export function tar(entries, { end = true } = {}) {
  const parts = [];
  for (const entry of entries) {
    if (entry.raw) {
      parts.push(entry.raw);
      continue;
    }
    const data = Buffer.from(entry.data ?? '');
    let name = entry.path;
    if (Buffer.byteLength(name) > 100) {
      const long = Buffer.from(`${name}\0`);
      parts.push(tarHeader({ name: '././@LongLink', size: long.length, type: 'L' }), padded(long));
      name = name.slice(0, 100);
    }
    parts.push(
      tarHeader({ name, size: data.length, type: entry.type ?? '0', mode: entry.mode ?? 0o644, mtime: entry.mtime ?? 0 }),
      padded(data),
    );
  }
  if (end) parts.push(Buffer.alloc(2 * BLOCK));
  return Buffer.concat(parts);
}

/** A pax extended header record, "<length> <key>=<value>\n". */
export function paxRecord(key, value) {
  const body = ` ${key}=${value}\n`;
  let length = Buffer.byteLength(body);
  while (String(length).length + Buffer.byteLength(body) !== length) {
    length = String(length).length + Buffer.byteLength(body);
  }
  return `${length}${body}`;
}

// --- Zip archives ---------------------------------------------------------------

/**
 * A zip archive of `entries`, `{ path, data, mode }`, each stored
 * uncompressed; a path ending in '/' is a directory. Each records its Unix
 * `mode`, as Info-ZIP's zip does, so an unpacker that keeps modes keeps it.
 */
export function zip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const entry of entries) {
    const name = Buffer.from(entry.path);
    const data = Buffer.from(entry.data ?? '');
    const directory = entry.path.endsWith('/');
    const mode = (directory ? 0o040000 : 0o100000) | (entry.mode ?? (directory ? 0o755 : 0o644));
    const crc = crc32(data);
    // The fields both headers share, from "version needed" on: version 1.0,
    // no flags, stored, midnight on 1980-01-01 (the earliest date a zip can
    // hold), the CRC-32 and both sizes.
    const shared = Buffer.alloc(26);
    shared.writeUInt16LE(10, 0);
    // A name beyond ASCII is UTF-8, which the flag says, as zip tools set it.
    if (!/^[\x20-\x7e]*$/.test(entry.path)) shared.writeUInt16LE(0x0800, 2);
    shared.writeUInt16LE((1 << 5) | 1, 8);
    shared.writeUInt32LE(crc, 10);
    shared.writeUInt32LE(data.length, 14);
    shared.writeUInt32LE(data.length, 18);
    shared.writeUInt16LE(name.length, 22);
    const local = Buffer.alloc(4);
    local.writeUInt32LE(0x04034b50, 0);
    locals.push(local, shared, name, data);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    // Made by Unix (3), version 3.0, so the mode below is read as one.
    central.writeUInt16LE((3 << 8) | 30, 4);
    shared.copy(central, 6);
    central.writeUInt32LE(((mode << 16) | (directory ? 0x10 : 0)) >>> 0, 38);
    central.writeUInt32LE(offset, 42);
    centrals.push(central, name);
    offset += 30 + name.length + data.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}
