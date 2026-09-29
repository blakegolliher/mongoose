# mongoose

Copies everything under one NFS path to another NFS path, then keeps
the copy up to date until you are ready to switch over. One binary for
Linux x86_64. It speaks NFSv3 directly, so nothing needs to be mounted
and nothing else needs to be installed.

## Install

Download the binary from the
[latest release](https://github.com/blakegolliher/mongoose/releases/latest)
and put it on your PATH:

```bash
curl -LO https://github.com/blakegolliher/mongoose/releases/latest/download/mongoose-linux-x86_64
chmod +x mongoose-linux-x86_64
sudo mv mongoose-linux-x86_64 /usr/local/bin/mongoose
```

The release page also has an `.rpm` and a `.deb` if you prefer a
package (they add a man page).

Needs Linux x86_64 with glibc 2.34 or newer: RHEL/Rocky/Alma 9+,
Ubuntu 22.04+, Debian 12+, SLES 15 SP4+. The CPU must support AES-NI
(every mainstream x86-64 server CPU since about 2010); mongoose refuses
to start without it. Run it as root: it needs reserved ports to talk to
the NFS servers.

## Use

Pick a directory for mongoose to keep its notes in (the work dir).
One job per work dir; reuse it to resume or to sync.

**1. Copy everything.**

```bash
sudo mongoose copy \
  --src nfs://old-server/export/data \
  --dst nfs://new-server/export/data \
  --work-dir /var/lib/mongoose/data
```

This scans the source, builds an index, then copies. It prints a
progress line every 15 seconds. If it stops for any reason (Ctrl-C,
reboot, network blip), run the same command again and it picks up
where it left off.

**2. Catch up on what changed while that ran.** Repeat as often as
you like while people are still using the old server.

```bash
sudo mongoose sync --work-dir /var/lib/mongoose/data
```

**3. Switch over.** Stop everything that writes to the old server,
sync once more, then verify:

```bash
sudo mongoose sync --work-dir /var/lib/mongoose/data
sudo mongoose sync --work-dir /var/lib/mongoose/data --cutover
```

`--cutover` copies nothing. It rescans the source to confirm nothing
changed since the last sync, then scans the new server and reads every
file back from both servers, comparing names, types, sizes, owner,
mode, file timestamps, symlink targets, and a SHA-256 of the contents.
It fails if anything differs, so a clean exit means the two trees
match and the new server is ready to use. Expect it to take about as
long as reading the whole tree from each server once; re-run it after
an interruption and it resumes.

If it fails, it prints where the report is (`verify.json` plus a full
`verify/mismatches.jsonl` in the pass directory it names). Fix the
destination, or the source, and run `--cutover` again.

## Options

| flag | what it does |
|---|---|
| `--exclude GLOB` | Skip every directory whose name matches, and everything under it: `--exclude .snapshot`, `--exclude '*.tmp'`. A glob (`*`, `?`, `[...]`) matched against the directory's name only, never its path; files are never matched. Repeatable, `copy` only, checked before anything is written. Remembered in the work dir, so every later sync and the cutover skip the same directories. |
| `--parallel N` | How much to do at once. Default 32. Drop it (say `--parallel 8`) if the old or new server gets sluggish for other users; raise it (up to 100) if both servers are idle and the copy is slow. Not sticky: stop, re-run with a new value, and the job resumes at that level. |
| `-v` | More logging (`-vv` for debug). |

## What to expect

- **Exit code** 0 means done. 1 means it could not run (bad flags,
  unreachable server, not root) or, for `--cutover`, that the trees
  do not match. 2 means it finished but some files failed; the list is
  under `<work-dir>/failures/`, and the next `sync` retries them.
- **Everything is preserved**: files, directories, symlinks,
  hardlinks, owner, mode, and file timestamps. For ownership to carry
  over, the destination export has to let root in (`no_root_squash`),
  and the source export has to let root read everything. Directory
  timestamps are restored on a best-effort basis and are not part of
  what `--cutover` checks.
- **Deleting on the old server never deletes on the new one.** A sync
  notices deletions and writes them to
  `<work-dir>/passes/pass-NNNN/classify/deleted.jsonl`, but leaves the
  destination alone. Anything on the new server that is not on the old
  one, including those leftovers, makes `--cutover` fail until you
  remove it.
- **Fifos, sockets, and device nodes are not copied.** `--cutover`
  lists any it finds on the source; recreate them on the new server or
  remove them from the old one.
- **A scan that cannot read every directory fails.** Transient
  problems are retried; a directory the server will not list (for
  example one root cannot read) makes `copy` or `sync` stop with the
  list of directories and exit 1, and nothing is checkpointed, so a
  subtree can never be silently left out. Fix the cause and re-run.
- **Source and destination must not overlap.** mongoose refuses to
  start if `--dst` is the same path as `--src`, inside it, or a parent
  of it on the same server. It does not trust the spelling: a server
  named two ways (hostname and IP address, two DNS aliases, a spelled
  or default port) is still one server, and before the first file is
  written it asks both servers which directory each mount really is
  and refuses if the two are the same or nested. There is no override.
- **Snapshot directories** (`.snapshot`, `.zfs`, `~snapshot`, and
  friends) should be excluded with `--exclude`; otherwise every
  snapshot gets copied as real data. An excluded directory is left
  out of every scan, so it is never copied, never reported as changed
  by a sync, and never reported as missing by the cutover.
- NFSv3 only.

More detail (what lives in the work dir, resume rules, correctness
posture, known limitations) is in [docs/REFERENCE.md](docs/REFERENCE.md).
Building from source and cutting a release: [docs/BUILDING.md](docs/BUILDING.md).

## License

MIT. See [LICENSE](LICENSE).

The release binaries statically include [libnfs](https://github.com/blakegolliher/libnfs)
(LGPL-2.1-or-later). Its source is the commit pinned in
`packaging/libnfs.lock.json`. Every release carries the exact libnfs source and
an offline relink kit. Run `mongoose licenses --component libnfs` for the full
license text, source revision, and matching release-asset names; see
`docs/BUILDING.md` for the verified replacement procedure.
