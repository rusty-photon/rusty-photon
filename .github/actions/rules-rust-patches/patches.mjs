// rules-rust-patches.yml's guards over the two patches to rules_rust that
// `bazel coverage` depends on (MODULE.bazel's single_version_override;
// .bazelrc's coverage section says what each does). A patch that no longer
// applies fails every Bazel command when rules_rust is fetched; these catch a
// rules_rust bump after which a patch still applies but no longer does its
// job, which no test would notice. Each looks for its patch's work in the
// patched file of rules_rust's fetched source. The bazel-coverage job, which
// compiles and runs the patched collector on every pull request, is the
// behavioural backstop. Conventions: docs/skills/ci-logic.md.

import { readFile as fsReadFile } from 'node:fs/promises';
import path from 'node:path';

import { error, execute, io } from '../lib/helpers.mjs';

/** A target whose `bazel build --nobuild` fetches and patches rules_rust, compiling nothing. */
export const FETCH = '@rules_rust//util/collect_coverage:collect_coverage';

/**
 * Whether rustc.bzl's coverage `instrumented_files_kwargs = { ... }` block
 * lists "data" in its dependency_attributes, which puts a spawned binary's
 * src/main.rs in the test's instrumented-files manifest, where it survives
 * Bazel's LcovMerger. Only lines inside that block count: rules_rust has
 * other lists that hold "data", such as ["data", "compile_data"].
 */
export function dataInstrumented(text) {
  let inside = false;
  for (const line of text.split('\n')) {
    if (line.includes('instrumented_files_kwargs = {')) inside = true;
    if (inside && line.includes('"dependency_attributes":') && line.includes('"data"')) return true;
    if (inside && line.includes('}')) inside = false;
  }
  return false;
}

/**
 * Each patch: its file in the repository, the file it patches in rules_rust,
 * whether the patched text still shows the patch's work, and what is missing
 * when it does not.
 */
export const PATCHES = [
  {
    patch: 'third_party/patches/collect_coverage_extra_objects.patch',
    file: 'util/collect_coverage/collect_coverage.rs',
    // The collector exports the coverage of the binaries a test spawns, which
    // RUST_COVERAGE_EXTRA_OBJECTS names and find_extra_object finds.
    effective: (text) => text.includes('RUST_COVERAGE_EXTRA_OBJECTS') && text.includes('fn find_extra_object'),
    missing: 'lacks RUST_COVERAGE_EXTRA_OBJECTS or find_extra_object',
  },
  {
    patch: 'third_party/patches/instrumented_files_include_data.patch',
    file: 'rust/private/rustc.bzl',
    effective: dataInstrumented,
    missing: 'no longer has "data" in the dependency_attributes of its coverage instrumented_files_kwargs block',
  },
];

/**
 * Fetches rules_rust as MODULE.bazel patches it and checks each of PATCHES
 * in what was fetched, with an error annotation for each patch that is not
 * effective, then throws if any is not.
 */
export async function checkPatches({ run = execute, readFile = fsReadFile, signal } = {}) {
  await run('bazel', ['build', '--nobuild', FETCH], { signal });
  const outputBase = (await run('bazel', ['info', 'output_base'], { echo: false, signal })).trim();
  let broken = 0;
  for (const { patch, file, effective, missing } of PATCHES) {
    const source = path.join(outputBase, 'external', 'rules_rust+', file);
    if (effective(await readFile(source, 'utf8'))) {
      io.stdout(`${patch} is applied and effective in ${source}\n`);
      continue;
    }
    error(`${patch} is not effective: ${source} ${missing}. A rules_rust upgrade likely needs the patch rebased.`);
    broken += 1;
  }
  if (broken > 0) throw new Error(`${broken} of the ${PATCHES.length} rules_rust coverage patches are not effective: see the annotations above`);
}
