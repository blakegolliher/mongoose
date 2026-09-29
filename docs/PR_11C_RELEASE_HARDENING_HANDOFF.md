# PR-11C handoff — release hardening and provenance

Status (2026-09-29):

- **11C-1, libnfs archive reproducibility:** merged in PR #10 (`2e7e712`).
- **11C-2, release gate hardening:** implemented on branch
  `pr-11c-release-gates`.
- **11C-3, SBOM, signed checksums, and provenance:** approved (GitHub
  Actions with Sigstore, and a CycloneDX SBOM); next after 11C-2 merges.
- **11C-4, real-NFS qualification and the release record:** defaults
  approved; waiting on the owner's NFS and client-host inventory.

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

## 11C-3 — SBOM, signed checksums, and provenance (approved)

Maps to PR-11 required outcome 7, and to "publishes checksums/provenance only
after every software and hardware gate succeeds". Today, the only integrity
record is an unsigned `SHA256SUMS`, served from the same place as the
artifacts.

Owner decisions (2026-09-29):

- **Build and sign in a GitHub Actions release workflow with Sigstore.** An
  owner-held key on the release host was rejected: it would show who signed
  the files, but not how they were built.
- **Ship a CycloneDX JSON SBOM.** Besides the Cargo graph, it must list two
  components that Cargo does not describe fully:
  - the statically linked libnfs: C source at the locked revision, under
    LGPL-2.1-or-later, with the locked archive digest;
  - the git-pinned nfs-walker at its locked revision.

**Required order.** Nothing may be rebuilt or regenerated between gating,
signing, and publishing:

1. Build all release artifacts.
2. Generate the CycloneDX SBOM and validate it.
3. Generate the final `SHA256SUMS`, including the SBOM.
4. Run the complete release gate and LGPL gate.
5. Freeze those exact bytes.
6. Sign `SHA256SUMS`, and attest the artifacts.
7. After approval in the protected environment, publish only those
   already-gated files.

Signature bundles and attestations are metadata outside the artifact list in
`SHA256SUMS`. The publish step must account for them explicitly: it uploads
the gated files, `SHA256SUMS`, and the signature bundle, and nothing else.

**Workflow requirements:**

- Pin every third-party Action by full commit SHA.
- Grant only the `contents`, `id-token`, and `attestations` permissions, and
  only to the jobs that need them.
- Fail unless the tag, the Cargo version, and the checked-out commit agree.
- Attest every published artifact, including the source bundle and the LGPL
  relink bundle.
- Pass the gated artifact set between jobs by digest. Every later job
  re-verifies each file against the frozen digests before it signs, attests,
  or publishes.
- Prove, from a clean environment and with documented commands, that a
  downloader can verify the signature on `SHA256SUMS`, each file's
  attestation, and each file's checksum.

## 11C-4 — real-NFS qualification and the release record (defaults approved)

Maps to PR-11 required outcome 8 and to the production exit criteria.
GitHub-hosted runners cannot reach the NFS systems.

Approved defaults (2026-09-29):

- The project owner approves releases.
- Qualification runs the exact attested binary from 11C-3, checked against
  its attested digest, never a rebuild.
- Each release gets a record in two forms, machine-readable JSON and readable
  Markdown. The record contains:
  - the commit and tag, and the artifact digests;
  - the environment details and the NFS configuration;
  - the commands run, their results, and timestamps;
  - the digests of the logs;
  - the approval.
- The required tests are the hardware tests named in the production exit
  criteria: exclusion, scan-error, cutover, alias overlap, crash,
  concurrency, and torn-copy.
- A missing or failed required test blocks publication. There are no silent
  waivers.

Remaining input from the owner: the inventory of NFS servers and exports, and
of client hosts.

## Non-goals for 11C-2

- Signing, provenance, SBOMs, a release workflow, and hardware qualification
  (11C-3 and 11C-4).
- Changes to libnfs, the static-link policy, the relink kit, or the LGPL gate.
- Changes to copy, sync, or cutover behavior. The AES-NI check only refuses
  to start on hardware where mongoose could not have run.
