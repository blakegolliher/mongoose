# LGPL compliance policy

This file is a release-blocking engineering policy, not legal advice. The
project owner has selected static linking to preserve mongoose's single-binary
operator experience. The selected libnfs revision is
LGPL-2.1-or-later; mongoose itself remains MIT licensed.

The governing license text is GNU LGPL 2.1, especially section 6:
<https://www.gnu.org/licenses/old-licenses/lgpl-2.1.en.html#SEC6>.
The GNU static-versus-dynamic linking FAQ is:
<https://www.gnu.org/licenses/gpl-faq.html#LGPLStaticVsDynamic>.

## Invariant

No mongoose binary containing libnfs may be published, attached to a release,
copied into a package repository, or represented as a release artifact unless
the exact release set passes `scripts/check-lgpl-compliance.sh` in release
mode. A failing or unavailable check blocks distribution.

The gate is deliberately fail-closed. Missing materials are not warnings, and
there is no release waiver flag. The check may be strengthened without an
owner decision; weakening it or changing the linkage model requires the
project owner's explicit approval and a same-commit update to every policy and
packaging surface named in `AGENTS.md`.

## Static-link release contract

Every distributed binary and package must meet all of these conditions:

1. **Prominent notice and license copy.** The binary exposes an offline
   `mongoose licenses --component libnfs` command that identifies libnfs,
   `LGPL-2.1-or-later`, the exact source revision, the fact that it is linked
   statically, and the corresponding-source location. Its output includes the
   complete LGPL 2.1 text. Packages and archives carry the same information.
2. **Exact Library source.** The release provides the complete source for the
   pinned libnfs revision, including local changes, generated inputs needed to
   build it, license notices, and patches. A mutable branch or a commit URL by
   itself is not the release source bundle.
3. **Complete work that uses the Library.** The release provides the exact
   mongoose and nfs-walker source, locked/vendored dependencies, build inputs,
   and any object/archive material required by the relink procedure. Recipients
   must be able to modify libnfs and produce a working modified mongoose.
   The relink kit's Cargo source map must send every locked dependency source
   to its `vendor/` directory, so the kit builds offline from an empty Cargo
   home; the release gate checks this on the packaged kit.
4. **Reproducible instructions.** The relink kit records the Rust, Cargo,
   cargo-zigbuild, Zig, target, linker flags, libnfs configuration, source
   revisions, and commands used by that release.
5. **No prohibited restrictions.** Distribution terms must continue to permit
   modification for the recipient's own use and reverse engineering for
   debugging those modifications. No EULA or artifact policy may take those
   rights away.
6. **Equivalent access.** Anyone offered a binary download is offered the
   corresponding source and relink materials from the same durable release
   location. Keep those assets available for as long as the binary is offered.
7. **Relink proof.** Before publication, a clean environment rebuilds a
   deliberately modified, interface-compatible libnfs, relinks mongoose with
   it offline from an empty, isolated Cargo home, runs the smoke suite, and
   records the result in the release evidence.
8. **Artifact consistency.** The bare binary, RPM, DEB, and tarball use the
   same digest-identified executable. Their notices, package license metadata,
   source pointers, and checksums describe that exact build.

## Required release assets

For version `X.Y.Z` and the short libnfs revision recorded in
`packaging/libnfs.lock.json`, the release directory must contain:

```text
mongoose-linux-x86_64
mongoose-X.Y.Z-source.tar.gz
mongoose-X.Y.Z-relink-kit.tar.gz
libnfs-<revision>-source.tar.gz
LICENSES.txt
THIRD_PARTY_LICENSES.md
LIBNFS_SOURCE.md
RELINK-VERIFICATION.txt
LICENSE-MIT
LICENSE-LGPL-2.1.txt
LICENSE-BSD-2-Clause-libnfs.txt
```

RPM, DEB, and the runtime tarball must contain the MIT license, the LGPL 2.1
text, the libnfs BSD text for generated protocol sources,
`THIRD_PARTY_LICENSES.md`, and instructions locating corresponding source and
the relink kit. Checksums and provenance are handled by the general release
gate, not used as a substitute for these materials.

## Change triggers

The compliance gate and release evidence must be reviewed whenever any of the
following changes:

- `packaging/libnfs.lock.json` or the contents of `libnfs.a`;
- the nfs-walker revision or either crate's libnfs build/link directives;
- LTO, stripping, linker, target, or toolchain configuration;
- package/release contents or artifact names;
- the embedded license command or third-party inventory;
- repository or release-host retention policy.

Ordinary development builds do not need a relink bundle. Distribution does.
`make compliance-check` validates the checked-in policy; `make release` runs
the stricter artifact check and refuses to finish until all PR-10 deliverables
exist and agree.
