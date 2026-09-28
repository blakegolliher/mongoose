# Repository instructions

## Non-negotiable LGPL release rule

Mongoose intentionally ships the pinned libnfs build statically so operators
can run one binary. libnfs is LGPL-2.1-or-later. Every distributed binary must
therefore satisfy the static-link obligations in
`docs/LGPL_COMPLIANCE.md`.

- Never publish or describe an artifact as releasable unless
  `scripts/check-lgpl-compliance.sh --release-dir ...` passes for the exact
  artifact set.
- Never remove, bypass, waive, or convert a failure in that gate to a warning
  merely to make a release succeed.
- Changes to libnfs, nfs-walker linkage, build scripts, package metadata,
  release workflows, notices, source bundles, or relink materials must preserve
  the static-link policy and run `make compliance-check`.
- The release must provide the exact libnfs source, the complete mongoose work
  needed to rebuild, relinkable materials/instructions, the LGPL text and
  prominent notice, and a verified modified-libnfs relink exercise.
- Only the project owner may change the static-link distribution decision. A
  change must update this file, `docs/LGPL_COMPLIANCE.md`, the machine-readable
  libnfs lock, packaging, and the release gate in the same reviewed commit.

Do not interpret the repository's MIT license as replacing or weakening the
LGPL terms that apply to the bundled libnfs component.
