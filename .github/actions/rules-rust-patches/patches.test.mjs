import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { test } from 'node:test';

import { capture } from '../lib/testing.mjs';
import { FETCH, checkPatches, dataInstrumented } from './patches.mjs';

// --- Fixtures -----------------------------------------------------------------

// rustc.bzl's coverage block as rules_rust 0.74.0 has it with the patch, and
// without it.
const KWARGS_PATCHED = `
    instrumented_files_kwargs = {
        # rusty-photon patch (instrumented_files_include_data): include \`data\` so a
        # spawned, instrumented service binary contributes its sources.
        "dependency_attributes": ["deps", "crate", "data"],
        "extensions": ["rs"],
        "source_attributes": ["srcs"],
    }
`;
const KWARGS_STOCK = `
    instrumented_files_kwargs = {
        "dependency_attributes": ["deps", "crate"],
        "extensions": ["rs"],
        "source_attributes": ["srcs"],
    }
`;
// Another list in rustc.bzl that holds "data".
const OTHER_DATA = `
    _collect = {
        "dependency_attributes": ["data", "compile_data"],
    }
`;
const COLLECTOR_PATCHED = `
/// RUST_COVERAGE_EXTRA_OBJECTS, mirroring how \`test_binary\` is resolved above
fn find_extra_object(
    rel: &str,
) -> Option<PathBuf> {
`;

function tempDir() {
  const base = process.env.TEST_TMPDIR ?? path.join(process.cwd(), '.tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(path.join(base, 'rules-rust-patches-'));
}

// An output base whose fetched rules_rust has the collector and rustc.bzl
// given.
function outputBase({ collector = COLLECTOR_PATCHED, rustc = KWARGS_PATCHED } = {}) {
  const base = tempDir();
  const root = path.join(base, 'external', 'rules_rust+');
  for (const [file, text] of [
    ['util/collect_coverage/collect_coverage.rs', collector],
    ['rust/private/rustc.bzl', rustc],
  ]) {
    mkdirSync(path.dirname(path.join(root, file)), { recursive: true });
    writeFileSync(path.join(root, file), text);
  }
  return base;
}

// A `run` that answers `bazel info output_base` with `base`, and fails the
// command `failing` names (build or info) with `failure`.
function fakeBazel(base, { failing, failure } = {}) {
  const calls = [];
  const run = async (program, args, options) => {
    calls.push({ program, args, options });
    if (args[0] === failing) throw failure;
    return args[0] === 'info' ? `${base}\n` : '';
  };
  return { run, calls };
}

// --- dataInstrumented -----------------------------------------------------------

test('dataInstrumented finds "data" in the coverage block that the patch puts it in', () => {
  assert.equal(dataInstrumented(KWARGS_PATCHED), true);
  assert.equal(dataInstrumented(`instrumented_files_kwargs = { "dependency_attributes": ["deps", "data"] }\n`), true);
});

test('dataInstrumented does not find it in the stock block, nor in another list', () => {
  assert.equal(dataInstrumented(KWARGS_STOCK), false);
  assert.equal(dataInstrumented(OTHER_DATA + KWARGS_STOCK), false);
  assert.equal(dataInstrumented(KWARGS_STOCK + OTHER_DATA), false);
  assert.equal(dataInstrumented(''), false);
});

test('dataInstrumented reads the block to its first closing brace only', () => {
  const closedEarly = `    instrumented_files_kwargs = {\n        "extensions": {"rs": 1},\n        "dependency_attributes": ["deps", "data"],\n    }\n`;
  assert.equal(dataInstrumented(closedEarly), false);
});

// --- checkPatches ---------------------------------------------------------------

test('checkPatches fetches rules_rust without building, then finds both patches effective', async () => {
  const base = outputBase();
  const signal = new AbortController().signal;
  const bazel = fakeBazel(base);
  const { stdout } = await capture(() => checkPatches({ run: bazel.run, signal }));
  assert.deepEqual(bazel.calls, [
    { program: 'bazel', args: ['build', '--nobuild', FETCH], options: { signal } },
    { program: 'bazel', args: ['info', 'output_base'], options: { echo: false, signal } },
  ]);
  const root = path.join(base, 'external', 'rules_rust+');
  assert.equal(
    stdout,
    `third_party/patches/collect_coverage_extra_objects.patch is applied and effective in ${path.join(root, 'util/collect_coverage/collect_coverage.rs')}\n` +
      `third_party/patches/instrumented_files_include_data.patch is applied and effective in ${path.join(root, 'rust/private/rustc.bzl')}\n`,
  );
});

test('checkPatches fails a collector that lacks either mark of its patch', async () => {
  for (const collector of ['fn find_extra_object(\n', 'env::var("RUST_COVERAGE_EXTRA_OBJECTS")\n']) {
    const base = outputBase({ collector });
    const { stdout } = await capture(() =>
      assert.rejects(checkPatches({ run: fakeBazel(base).run }), {
        message: '1 of the 2 rules_rust coverage patches are not effective: see the annotations above',
      }),
    );
    const source = path.join(base, 'external', 'rules_rust+', 'util/collect_coverage/collect_coverage.rs');
    assert.ok(
      stdout.includes(
        `::error::third_party/patches/collect_coverage_extra_objects.patch is not effective: ${source} lacks RUST_COVERAGE_EXTRA_OBJECTS or find_extra_object. ` +
          'A rules_rust upgrade likely needs the patch rebased.\n',
      ),
      stdout,
    );
  }
});

test('checkPatches reports each patch that is not effective, not only the first', async () => {
  const base = outputBase({ collector: '', rustc: KWARGS_STOCK });
  const { stdout } = await capture(() =>
    assert.rejects(checkPatches({ run: fakeBazel(base).run }), {
      message: '2 of the 2 rules_rust coverage patches are not effective: see the annotations above',
    }),
  );
  assert.equal(stdout.match(/^::error::/gm).length, 2);
  assert.match(stdout, /instrumented_files_include_data\.patch is not effective: .* no longer has "data" in the dependency_attributes/);
});

test('checkPatches fails when Bazel does, reading nothing', async () => {
  const failure = new Error('bazel exited with 1: ERROR: Error computing the main repository mapping');
  let read = false;
  const readFile = async () => {
    read = true;
  };
  await assert.rejects(checkPatches({ run: fakeBazel(tempDir(), { failing: 'build', failure }).run, readFile }), failure);
  assert.equal(read, false);
});

test('checkPatches fails when the fetched rules_rust is not where it reads', async () => {
  await capture(() => assert.rejects(checkPatches({ run: fakeBazel(tempDir()).run }), { code: 'ENOENT' }));
});
