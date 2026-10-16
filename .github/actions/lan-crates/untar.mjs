// A reader for the tar archives inside `.crate` files, so the action needs no
// system `tar`. It reads what `cargo package` writes (GNU headers, a GNU
// long-name entry for a path over 100 bytes) and what older packagers wrote
// (POSIX ustar with a prefix field, pax `path` records). Anything that is not
// a file or a directory, such as a link, is refused: Cargo packages the file a
// link points to, not the link.

const BLOCK = 512;

// Entry types, from POSIX's ustar and GNU tar's extensions.
const FILE_TYPES = new Set(['0', '\0', '7']);
const DIRECTORY = '5';
const PAX_NEXT = 'x';
const PAX_GLOBAL = 'g';
const GNU_LONG_NAME = 'L';

function text(block, start, length) {
  const field = block.subarray(start, start + length);
  const end = field.indexOf(0);
  return field.subarray(0, end === -1 ? length : end).toString('utf8');
}

function octal(block, start, length, what) {
  const field = text(block, start, length).trim();
  if (!/^[0-7]*$/.test(field)) throw new Error(`tar: bad ${what} field ${JSON.stringify(field)}`);
  return field === '' ? 0 : Number.parseInt(field, 8);
}

// The header's checksum is the sum of its bytes, with its own field read as
// eight spaces.
function checksumMatches(block) {
  let sum = 0;
  for (let i = 0; i < BLOCK; i++) sum += i >= 148 && i < 156 ? 0x20 : block[i];
  return sum === octal(block, 148, 8, 'checksum');
}

// A pax extended header: records of the form "<length> <key>=<value>\n".
// Returns the two this reader uses, `path` and `mtime`, where present.
function paxRecords(data) {
  const found = {};
  let at = 0;
  while (at < data.length) {
    const space = data.indexOf(0x20, at);
    if (space === -1) throw new Error('tar: bad pax record');
    const length = Number.parseInt(data.subarray(at, space).toString('utf8'), 10);
    if (!Number.isInteger(length) || length <= space - at || at + length > data.length) {
      throw new Error('tar: bad pax record length');
    }
    const record = data.subarray(space + 1, at + length - 1).toString('utf8');
    const equals = record.indexOf('=');
    const key = equals === -1 ? '' : record.slice(0, equals);
    const value = record.slice(equals + 1);
    if (key === 'path') found.path = value;
    if (key === 'mtime') {
      // Seconds since the epoch, with an optional fraction (POSIX pax).
      if (!/^\d+(?:\.\d+)?$/.test(value)) throw new Error(`tar: bad pax mtime ${JSON.stringify(value)}`);
      found.mtime = Number(value);
    }
    at += length;
  }
  return found;
}

/**
 * The entries of the uncompressed tar archive `archive`: `{ path, type, mode,
 * mtime, data }`, where `type` is 'file' or 'directory', `mtime` is in
 * seconds since the epoch and `data` is a view into `archive`. Throws on a damaged header, an archive cut short, or an entry
 * that is neither a file nor a directory.
 */
export function untar(archive) {
  const entries = [];
  let longName;
  let pax = {};
  let at = 0;
  for (;;) {
    if (at + BLOCK > archive.length) throw new Error('tar: archive ends without its end-of-archive blocks');
    const header = archive.subarray(at, at + BLOCK);
    if (header.every((byte) => byte === 0)) break;
    if (!checksumMatches(header)) throw new Error(`tar: header checksum mismatch at offset ${at}`);
    const type = String.fromCharCode(header[156]);
    const size = octal(header, 124, 12, 'size');
    const start = at + BLOCK;
    if (start + size > archive.length) throw new Error('tar: entry runs past the end of the archive');
    const data = archive.subarray(start, start + size);
    at = start + Math.ceil(size / BLOCK) * BLOCK;

    if (type === GNU_LONG_NAME) {
      longName = text(data, 0, data.length);
      continue;
    }
    if (type === PAX_NEXT) {
      pax = paxRecords(data);
      continue;
    }
    if (type === PAX_GLOBAL) continue;

    let path = text(header, 0, 100);
    // POSIX ustar ("ustar\0" + "00") keeps a long path's leading part in the
    // prefix field. GNU's header ("ustar  \0") uses those bytes for other
    // things.
    if (header.subarray(257, 263).toString('latin1') === 'ustar\0') {
      const prefix = text(header, 345, 155);
      if (prefix !== '') path = `${prefix}/${path}`;
    }
    path = pax.path ?? longName ?? path;
    const mtime = pax.mtime ?? octal(header, 136, 12, 'mtime');
    longName = undefined;
    pax = {};

    if (FILE_TYPES.has(type)) {
      entries.push({ path, type: 'file', mode: octal(header, 100, 8, 'mode'), mtime, data });
    } else if (type === DIRECTORY) {
      entries.push({ path, type: 'directory', mode: octal(header, 100, 8, 'mode'), mtime, data });
    } else {
      throw new Error(`tar: ${JSON.stringify(path)} is of type ${JSON.stringify(type)}, which is neither a file nor a directory`);
    }
  }
  return entries;
}
