# Release qualification harness

These scripts ran the v0.2.1 hardware qualification
(`docs/qualification/2026-10-19-v0.2.1-release.md`). Each one runs a
single test against the attested release binary and writes, for every step,
a log and a line of `steps.jsonl` (command, start and finish times, exit
status, expected status, and the log's SHA-256). Every test first checks the
binary against its attested digest.

The server name, its addresses, and local paths come from the environment.
Otherwise the scripts are as they ran, apart from the special-file
inspection queries noted in the record.

## Setup

Run on the client, as a user with passwordless `sudo`. You need `jq`,
`duckdb`, Python 3, and, for `t11`, rootful podman.

```
export MONGOOSE_BIN=/path/to/mongoose-linux-x86_64   # from the release set
export MONGOOSE_SHA256=<its digest in SHA256SUMS>
export QUAL_SERVER=<nfs server name>
export QUAL_VIP_A=<one server address> QUAL_VIP_B=<another>   # for t4
export QUAL_LOGS=$HOME/mongoose-qual-<version>
export QUAL_TREE=qual-<version>   # test root under /source and /dest
```

The source export is mounted at `/mnt/source` and the destination export at
`/mnt/dest`, both `vers=3`. Every check reads through separate uncached
mounts, so attribute caching cannot hide a change:

```
sudo mkdir /mnt/source/$QUAL_TREE /mnt/dest/$QUAL_TREE /var/lib/mongoose/$QUAL_TREE
sudo mkdir -p /mnt/qual-src-noac /mnt/qual-dst-noac
sudo mount -t nfs -o vers=3,noac,lookupcache=none,actimeo=0 $QUAL_SERVER:/source/$QUAL_TREE /mnt/qual-src-noac
sudo mount -t nfs -o vers=3,noac,lookupcache=none,actimeo=0 $QUAL_SERVER:/dest/$QUAL_TREE /mnt/qual-dst-noac
```

The crash, concurrency, and interrupt tests share one source tree:

```
sudo python3 -I tools/make-bulk-tree.py /mnt/source/$QUAL_TREE/bulk 64 300 16 64
```

## Tests

Run each with `RUN=1 ./tN-name.sh`. A rerun needs a new `RUN`, because
fixtures on the server are never reused.

| Script | Test |
|---|---|
| `t1-exclude.sh` | Exclusion |
| `t3-cutover.sh` | Cutover verification and ten kinds of destination damage |
| `t4-alias.sh` | Alias overlap |
| `t5-crash.sh` | SIGKILL in the copy, the sync, and the cutover |
| `t6-concurrency.sh` | Work-dir lock |
| `t7-torn.sh` | Torn copy |
| `t8-special.sh` | Special files (uses `../make-special-tree.py` and `../recreate-on-dest.py`) |
| `t9-interrupt.sh` | SIGINT and SIGTERM exit status |
| `t10-throughput.sh` | Copy of the fs-sim web tree |
| `t11-portability.sh` | Rocky Linux 9 and Debian 12 containers |

There is no scan-error script yet. It needs a source directory that the
server refuses to list to root, for example a directory with mode 000 under
a root-squashed view.

The tests leave their trees on both exports and their work dirs under
`/var/lib/mongoose/$QUAL_TREE`. Remove them by hand when the record is done.
