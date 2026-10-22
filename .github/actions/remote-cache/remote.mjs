// The remote cache as this job's venue reaches it (ADR-020): the LAN
// bazel-remote on the pool, over gRPC while its Remote Asset API is on, and
// the cloud cache on a hosted runner. `configure` writes the Bazel flags for
// it as the `ci-remote` config, in a file .bazelrc imports, and every Bazel
// step adds --config=ci-remote.
//
// The write credential is not among those flags: .bazelrc's
// `build:ci --announce_rc` prints every option a step reads from a file, so a
// --remote_header there would be printed in every push-to-main step. Bazel
// asks a credential helper for it instead (helper.mjs). Bazel runs the helper
// in each Bazel step with that step's environment, where the workflows' env
// blocks put the credential on trusted events only. Plan item J10 in
// docs/plans/archive/ci-shell-to-code.md.

import { chmod, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { io, maskUrl, setSecret } from '../lib/helpers.mjs';

/** The cloud cache, as .bazelrc's `build:remote-cache` names it. A test holds the two equal. */
export const CLOUD_CACHE = 'https://cache.rustyphoton.space';

/** The file `configure` writes in the workspace's root, which .bazelrc try-imports. */
export const RC_FILE = 'ci-remote.bazelrc';

/**
 * The host of a cache's URL as Bazel matches a credential helper's scope
 * against it: java.net.URI's getHost(), which keeps the URL's own case, where
 * `new URL` would lowercase it. A host the scope cannot name, such as an IPv6
 * address or one after a user, throws, without showing the URL, which may be
 * a LAN address.
 */
export function cacheHost(url, name) {
  const host = /^[a-z][a-z0-9+.-]*:\/\/([a-z0-9.-]+)(?::\d*)?(?:[/?#]|$)/i.exec(url)?.[1];
  if (!host) {
    throw new Error(
      `${name} needs a host name or an IPv4 address, with no user, to scope Bazel's credential helper to (it is not shown, as it may be a LAN address)`,
    );
  }
  return host;
}

/**
 * The cache this job uses, from the job's environment: the pool runner's
 * RP_LAN_CACHE_URL, the mask step's RP_LAN_CACHE_GRPC_URL, and the write
 * credentials, which the workflows' env blocks set on trusted events only
 * (LAN_CACHE_WRITE_AUTH, a whole Authorization value, on push to main;
 * CACHE_WRITE_TOKEN, the cloud cache's Bearer token, on push and the
 * schedule). Returns the Bazel flags, the cache's `host`, which the
 * credential helper answers for, the Authorization value it answers with
 * (empty when the job only reads), and a description that shows no address.
 */
export function venue(env) {
  let flags;
  let url;
  let name;
  let authorization;
  let what;
  // The pool's two venues take remote-cache-base, remote-cache's settings
  // without the cloud cache's URL, so that --remote_cache is set once: Bazel
  // warns about an option that two config expansions both set.
  if (env.RP_LAN_CACHE_GRPC_URL) {
    // The pool, with the remote downloader on: the LAN bazel-remote serves
    // the cache and the Remote Asset API on one gRPC endpoint. The cache
    // channel must be gRPC too, as Bazel refuses a remote downloader beside
    // an http(s) --remote_cache. Bazel routes every repository download
    // through the asset Fetch RPC, and bazel-remote allows that RPC only
    // with the write credential, even for a cache hit. So only push-to-main
    // fetches through the LAN and fills it, the nightly coverage toolchain
    // included, the most expensive cold fetch a clone can meet after a pin
    // bump. An anonymous (PR) job gets UNAUTHENTICATED on each fetch, and
    // the local fallback makes the direct download it would have made
    // anyway: one failed LAN round trip and a log warning. The flags stay on
    // anonymous jobs even so: both run one code path, and a bazel-remote
    // that learns to serve anonymous checksummed hits from its CAS would
    // make a PR's fetches LAN hits with no workflow change.
    url = env.RP_LAN_CACHE_GRPC_URL;
    name = 'RP_LAN_CACHE_GRPC_URL';
    flags = [
      '--config=remote-cache-base',
      `--remote_cache=${url}`,
      `--experimental_remote_downloader=${url}`,
      '--experimental_remote_downloader_local_fallback',
    ];
    authorization = env.LAN_CACHE_WRITE_AUTH ?? '';
    what = "the LAN bazel-remote over gRPC, with its Remote Asset API for Bazel's downloads";
  } else if (env.RP_LAN_CACHE_URL) {
    // The pool over HTTP.
    url = env.RP_LAN_CACHE_URL.replace(/\/+$/, '');
    name = 'RP_LAN_CACHE_URL';
    flags = ['--config=remote-cache-base', `--remote_cache=${url}`];
    authorization = env.LAN_CACHE_WRITE_AUTH ?? '';
    what = 'the LAN bazel-remote over HTTP';
  } else {
    url = CLOUD_CACHE;
    name = 'The cloud cache';
    flags = ['--config=remote-cache'];
    authorization = env.CACHE_WRITE_TOKEN ? `Bearer ${env.CACHE_WRITE_TOKEN}` : '';
    what = `the cloud cache, ${CLOUD_CACHE}`;
  }
  if (!authorization) flags.push('--remote_upload_local_results=false');
  return { flags, host: cacheHost(url, name), lan: url !== CLOUD_CACHE, authorization, what };
}

/**
 * The text of the `ci-remote` config, one flag a line. A bazelrc line splits
 * at whitespace, reads quotes and backslashes as escapes and stops at `#`, so
 * a flag holding any of them throws rather than reach Bazel as other words.
 * The error does not show the flag, which may hold a LAN address.
 */
export function bazelrc(flags) {
  if (flags.some((flag) => /[\s#'"\\]/.test(flag))) {
    throw new Error("A remote-cache flag holds whitespace, a quote, a backslash or '#', which a bazelrc line cannot carry");
  }
  return [
    "# This job's remote cache, written by .github/actions/remote-cache. Not checked in.",
    ...flags.map((flag) => `build:ci-remote ${flag}`),
    '',
  ].join('\n');
}

/**
 * The launcher Bazel runs as the credential helper: a script that runs
 * `helper` on `node`, passing on its arguments. Bazel starts a helper as a
 * program, and a pool runner's PATH may have no Node, so it names the Node
 * this action runs on, the runner's own. On Windows a .cmd file, which
 * Windows runs through cmd.exe; its `@` keeps cmd.exe from echoing the line
 * into what Bazel parses. A path the script cannot quote throws.
 */
export function launcher(node, helper, platform) {
  if (platform === 'win32') {
    if ([node, helper].some((p) => /["%]/.test(p))) throw new Error('A path holds " or %, which a .cmd file cannot quote');
    return { name: 'bazel-credential-helper.cmd', text: `@"${node}" "${helper}" %*\r\n` };
  }
  if ([node, helper].some((p) => p.includes("'"))) throw new Error("A path holds ', which the launcher cannot quote");
  return { name: 'bazel-credential-helper', text: `#!/bin/sh\nexec '${node}' '${helper}' "$@"\n` };
}

/**
 * Masks the LAN cache's address, writes the credential helper's launcher to
 * `runnerTemp` and the `ci-remote` config to RC_FILE in `workspace`, and
 * prints which cache the job uses and whether it writes. `node` runs the
 * helper; `helper` is helper.mjs.
 *
 * The helper is configured on every job, read-only ones included, where it
 * answers with no headers: so every pull request's legs start it as push to
 * main does, and a helper Bazel cannot start shows there first.
 */
export async function configure({ env, workspace, runnerTemp, node, helper, platform = process.platform }) {
  for (const name of ['RP_LAN_CACHE_URL', 'RP_LAN_CACHE_GRPC_URL']) {
    if (env[name]) maskUrl(env[name], name);
  }
  const cache = venue(env);
  // maskUrl masks the host as `new URL` spells it, in lower case.
  if (cache.lan) setSecret(cache.host);

  const { name, text } = launcher(node, helper, platform);
  let launcherPath = path.join(runnerTemp, name);
  await writeFile(launcherPath, text);
  await chmod(launcherPath, 0o755);
  // A backslash is an escape in a bazelrc line; Bazel reads a Windows path
  // with forward slashes.
  if (platform === 'win32') launcherPath = launcherPath.replaceAll('\\', '/');

  await writeFile(
    path.join(workspace, RC_FILE),
    bazelrc([...cache.flags, `--credential_helper=${cache.host}=${launcherPath}`]),
  );
  io.stdout(
    `Remote cache: ${cache.what}. This job ${cache.authorization ? 'reads and writes it' : 'reads it only, with no write credential'}.\n`,
  );
}

/**
 * The credential helper's answer to Bazel (the Credential Helper
 * Specification): `get`, with the request `input` on stdin, answers with
 * the write credential for the cache's own host, and with no headers for any
 * other host or when the job has no credential. Bazel asks only about the
 * host the helper's scope names; checking it here as well keeps the
 * credential to the cache if the scope were ever dropped. Errors name
 * neither the request nor the credential.
 */
export function answer(args, input, env) {
  if (args[0] !== 'get') {
    throw new Error(`Bazel's credential helper takes the command get, not ${JSON.stringify(args[0] ?? '')}`);
  }
  let uri;
  try {
    uri = JSON.parse(input)?.uri;
  } catch {
    throw new Error('The request on stdin is not JSON');
  }
  const { host, authorization } = venue(env);
  let asked;
  try {
    asked = cacheHost(String(uri), 'The request');
  } catch {
    return {};
  }
  return authorization && asked === host ? { headers: { Authorization: [authorization] } } : {};
}
