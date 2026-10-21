// What the workflows read from the Cargo workspace's `cargo metadata`: the
// packages that a [package.metadata.<table>] gives a command, which a job runs
// one each (conformu.yml's ConformU services, scheduled.yml's Miri ones), and
// the MSRV check of each package that declares a rust-version (check.yml).
// Conventions: docs/skills/ci-logic.md.

import { execute, group, io } from '../lib/helpers.mjs';

/** The workspace's own packages, as `cargo metadata` describes them. */
export async function workspacePackages({ run = execute, signal } = {}) {
  const metadata = await run('cargo', ['metadata', '--format-version', '1', '--no-deps'], { echo: false, signal });
  return JSON.parse(metadata).packages;
}

/**
 * The packages whose [package.metadata.<table>] gives a `command`, as
 * `{ name, command }`, in the workspace's order. A `command` that is not a
 * command line throws, naming the package, rather than leave it out.
 */
export function tagged(packages, table) {
  const found = [];
  for (const { name, metadata } of packages) {
    const command = metadata?.[table]?.command;
    if (command === undefined || command === null) continue;
    if (typeof command !== 'string' || command.trim() === '') {
      throw new Error(`${name}'s [package.metadata.${table}] command must be a command line, not ${JSON.stringify(command)}`);
    }
    found.push({ name, command });
  }
  return found;
}

/** `tagged`, with what it found in the log. */
export async function discover(table, options = {}) {
  const found = tagged(await workspacePackages(options), table);
  io.stdout(`${found.length} package(s) with a [package.metadata.${table}] command:\n`);
  for (const { name, command } of found) io.stdout(`  - ${name}: ${command}\n`);
  return found;
}

/**
 * Runs `cargo msrv verify` on each package that declares a rust-version, in
 * a log group each, and stops at the first that fails. cargo-msrv does not
 * read [workspace.package]'s rust-version from a virtual workspace's root, so
 * each member is verified by its own manifest. Resolves to how many it
 * verified.
 */
export async function verifyMsrv(packages, { run = execute, signal } = {}) {
  const declared = packages.filter((p) => p.rust_version);
  for (const { name, manifest_path: manifest } of declared) {
    await group(name, async () => {
      try {
        await run('cargo', ['msrv', 'verify', '--manifest-path', manifest], { signal });
      } catch (err) {
        throw new Error(`cargo msrv verify failed for ${name}`, { cause: err });
      }
    });
  }
  return declared.length;
}
