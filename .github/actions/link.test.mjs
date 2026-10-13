// Every JavaScript file under .github/actions must parse and link: each
// static import names a file that exists, or a Node built-in, and each name
// it imports is exported there. Nothing is run, so a dynamic import() is not
// followed, which is why actions use static imports only. node_link_test in
// node_test.bzl puts every file in this test's runfiles, and says why.
//
// vm.SourceTextModule parses a module, and `link` resolves its imports and
// instantiates it without evaluating it, so an entry file's action never
// starts. It needs --experimental-vm-modules, which node_link_test passes.

import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import { pathToFileURL } from 'node:url';
import vm from 'node:vm';

// The test runs in the main repository's directory in the runfiles tree.
const ROOT = join('.github', 'actions');

const files = readdirSync(ROOT, { recursive: true })
  .filter((file) => /\.[cm]?js$/.test(file))
  .sort();

// A module for `url`: a Node built-in's exports, or a file parsed as an ES
// module. `cache` belongs to one root, so each file is checked on its own.
async function load(url, cache) {
  if (!cache.has(url)) {
    cache.set(
      url,
      (async () => {
        if (url.startsWith('node:')) {
          const exports = await import(url);
          const names = Object.keys(exports);
          return new vm.SyntheticModule(
            names,
            function () {
              for (const name of names) this.setExport(name, exports[name]);
            },
            { identifier: url },
          );
        }
        return new vm.SourceTextModule(readFileSync(new URL(url), 'utf8'), { identifier: url });
      })(),
    );
  }
  return cache.get(url);
}

// Actions import Node built-ins and the repo's own .mjs files, and nothing
// else: ADR-022 allows no npm packages.
async function link(file) {
  const cache = new Map();
  const root = await load(pathToFileURL(join(ROOT, file)).href, cache);
  await root.link(async (specifier, referencing) => {
    if (specifier.startsWith('node:')) return load(specifier, cache);
    if (/^\.\.?\//.test(specifier) && specifier.endsWith('.mjs')) {
      return load(new URL(specifier, referencing.identifier).href, cache);
    }
    throw new Error(`${referencing.identifier} imports ${specifier}: an action imports only node: built-ins and relative .mjs files`);
  });
}

test('the check sees the files it is meant to', () => {
  // A floor, not the list: an empty or partial `data` glob would otherwise
  // pass by checking nothing.
  for (const known of [join('lib', 'helpers.mjs'), join('lib', 'helpers.test.mjs'), 'link.test.mjs']) {
    assert.ok(files.includes(known), `${known} is missing from ${JSON.stringify(files)}`);
  }
});

for (const file of files) {
  test(`${file} parses and links`, async () => {
    assert.ok(file.endsWith('.mjs'), `${file}: write it as an ES module, .mjs (docs/skills/ci-logic.md)`);
    await link(file);
  });
}
