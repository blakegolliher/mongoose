# PR-11C handoff — release hardening and provenance

Status (2026-09-29):

- **11C-1, libnfs archive reproducibility:** merged in PR #10 (`2e7e712`).
- **11C-2, release gate hardening:** implemented on branch
  `pr-11c-release-gates`.
- **11C-3, SBOM, signed checksums, and provenance:** waiting on the owner
  decisions below.
- **11C-4, real-NFS qualification and the release record:** waiting on the
  owner decisions below.

This is the third slice of PR-11 in `docs/PRODUCTION_RELEASE_HANDOFF.md`. It
covers PR-11's required outcomes 4 to 8, and the acceptance items about
portability, CPU requirements, and publishing only after every gate passes.
It does not claim that mongoose is ready to release.

## 11C-1 — libnfs archive reproducibility (merged)

`scripts/build-libnfs-static.sh` maps Zig's lib directory to a fixed name,
so the same `libnfs.a` builds at any Zig install path. The lock's digest is
now enforced by `make libnfs-stage`, `make release`, and CI.
`docs/BUILDING.md` records the evidence.

## 11C-2 — release gate hardening (this branch)

Maps to PR-11 required outcomes 4, 5, and 6, and to the CPU acceptance item.

- **Toolchain versions are verified, not only recorded.**
  - `scripts/check-release-toolchain.sh` checks rustc, cargo, cargo-zigbuild,
    Zig, CMake, binutils (`ar`, `ranlib`, `objdump`, `readelf`), GNU tar, gzip,
    and cargo-about against `packaging/release-toolchain.lock.json`.
  - `make toolchain-check` runs it, and `build-portable` runs it first.
  - `make libnfs-stage` now takes the expected Zig version from that lock,
    instead of the build script's hardcoded default.
- **Architecture labels come from the build target.** The RPM architecture,
  the Debian architecture, and the tarball and bare-binary names derive from
  the build triple instead of the host's `uname -m` and `dpkg`. The x86_64
  names are unchanged, including the load-bearing `mongoose-linux-x86_64`.
- **Artifact gate.** `make release` runs `scripts/check-release-artifacts.sh`
  after writing `SHA256SUMS`. It fails the release unless every check below
  passes:
  - the bare binary is ELF64 x86-64 and uses the standard loader;
  - its only dynamic dependencies are `libc.so.6` and `libm.so.6`;
  - it uses no glibc symbol newer than 2.34;
  - `--version` prints `mongoose <version>`, and `--help` lists `copy` and
    `sync`;
  - `SHA256SUMS` lists exactly the files in `DIST`, and all of them verify;
  - the RPM installs, runs, and uninstalls cleanly on Rocky Linux 9.8 (glibc
    2.34, the oldest declared platform), with its man page and license files
    present;
  - the DEB does the same on Debian 12 (glibc 2.36);
  - the tarball and bare binary run on both of those systems;
  - under qemu-user, the binary refuses to start on an emulated Nehalem CPU,
    which lacks AES-NI, and runs on an emulated Westmere CPU, which has it.

  The container images are pinned by digest.
- **AES-NI.** The embedded walker hashes paths with gxhash, which is compiled
  for AES-NI and has no fallback, so a CPU without it would crash with SIGILL
  on the first scan. `mongoose` now checks for AES-NI before doing anything
  else. Without it, mongoose exits with status 1 and a clear message. The
  README states the requirement.

Acceptance for 11C-2:

- ShellCheck and `make compliance-check` pass;
- `make toolchain-check` passes on the release host and fails when the lock
  and a tool disagree;
- the full `make release` passes from a clean committed revision, including
  the new artifact gate;
- the artifact gate fails against a binary built without the AES-NI check;
- hosted CI is green.

## 11C-3 — SBOM, signed checksums, and provenance (owner decisions)

Maps to PR-11 required outcome 7, and to "publishes checksums/provenance only
after every software and hardware gate succeeds". Today, the only integrity
record is an unsigned `SHA256SUMS`, served from the same place as the
artifacts.

Decisions needed:

1. **Where release artifacts are built and signed.**
   - **Recommended: a GitHub Actions release workflow**, dispatched on a tag.
     It runs `make release`, including the artifact gate, on a GitHub runner
     with the locked toolchain. This is now possible because the libnfs
     archive is path-independent. The workflow then produces GitHub artifact
     attestations (SLSA build provenance, signed by Sigstore without a
     long-lived key) and a signed `SHA256SUMS`. Publishing waits for a
     protected environment that the owner approves after 11C-4's hardware
     qualification. There are no keys to manage, and anyone can verify which
     workflow, commit, and runner built each artifact.
   - **Alternative: keep building on the owner's release host.** The owner
     signs `SHA256SUMS` with a key they hold (minisign or GPG), and its public
     key is published in the repository. This gives no build provenance beyond
     the signature, and the key has to be protected and rotated.
2. **SBOM format.** CycloneDX JSON (recommended, via `cargo-cyclonedx`) or
   SPDX JSON. Either way, the SBOM must add two components that Cargo does not
   list:
   - the statically linked libnfs, as C source at the locked revision under
     LGPL-2.1-or-later;
   - the git-pinned nfs-walker.

   The SBOM would be published as a release asset and listed in `SHA256SUMS`.

## 11C-4 — real-NFS qualification and the release record (owner decisions)

Maps to PR-11 required outcome 8 and to the production exit criteria.
GitHub-hosted runners cannot reach the NFS systems, so this needs:

- the qualification environment: servers, exports, and the client host;
- who approves, and how. Recommended: a protected GitHub environment with the
  owner as required reviewer, whose approval attaches the qualification
  record;
- a release-record format. It lists the exact artifact digests, the
  software-gate results, the hardware results from the exclusion, scan-error,
  cutover, overlap, crash, concurrency, and torn-copy tests named in the exit
  criteria, and the approval.

## Non-goals for 11C-2

- Signing, provenance, SBOMs, a release workflow, and hardware qualification
  (11C-3 and 11C-4).
- Changes to libnfs, the static-link policy, the relink kit, or the LGPL gate.
- Changes to copy, sync, or cutover behavior. The AES-NI check only refuses
  to start on hardware where mongoose could not have run.
