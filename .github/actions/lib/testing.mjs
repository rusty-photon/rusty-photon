// Test fixtures every action's tests share: reading what the helpers print,
// and a loopback HTTP server to stand in for an API, a store or a download
// server. Imported only by tests (docs/skills/ci-logic.md §Tests).

import { createServer } from 'node:http';

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
