// release.yml's guard on its tag: the tag being released must name the
// workspace's version, which `cargo ci build-msi` stamps into the MSI, or a
// right-named tag would carry wrongly versioned packages. Conventions:
// docs/skills/ci-logic.md.

import { io } from '../lib/helpers.mjs';
import { workspaceVersion } from './plan.mjs';

/**
 * The version the tag `tag` names, its leading `v` dropped, once it is known
 * to be the workspace version in `cargoToml`, read as the packaging scripts
 * read it; any other version throws.
 */
export function checkTag(tag, cargoToml) {
  const version = tag.replace(/^v/, '');
  const workspace = workspaceVersion(cargoToml);
  if (version !== workspace) throw new Error(`the tag ${tag} names the version ${version}, but the workspace's is ${workspace}`);
  io.stdout(`The tag ${tag} names the workspace's version, ${workspace}.\n`);
  return version;
}
