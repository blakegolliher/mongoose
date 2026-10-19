#!/usr/bin/env bash
# T1 exclusion (PR-01): excluded directories and their subtrees are neither
# copied nor indexed, in the copy and in every later sync and the cutover;
# files never match; an invalid pattern is refused before a scan, a run.json,
# or any destination write. VAST reserves `.snapshot` (a hidden read-only
# virtual directory), so the fixture uses `.zfs` and `*.tmp` for real
# subtrees; `--exclude .snapshot` is still passed, as an operator would.
: "${RUN:?set RUN}"
T=t1-exclude-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=exclude-$RUN
src=$SRC_MNT/$name dst=$DST_MNT/$name wd=$WD/$name
note "fixture: $src"
sudo -n mkdir -p $src/keep/sub/.zfs $src/build.tmp/obj $src/keep/sub/cache.tmp $src/.zfs/snapshot
for f in keep/a.txt keep/sub/b.txt file.tmp keep/sub/notes.tmp \
         keep/sub/.zfs/c.txt build.tmp/obj/e.o keep/sub/cache.tmp/f.bin .zfs/snapshot/g.txt; do
    echo "content of $f" | sudo -n tee "$src/$f" >/dev/null || exit 1
done
sudo -n mkdir -p $dst
step fixture-tree 0 snap $src
kept() { printf '%s\n' file.tmp keep keep/a.txt keep/sub keep/sub/b.txt keep/sub/notes.tmp "$@" | LC_ALL=C sort; }

step invalid-pattern 1 mg copy --src $SRC_URL/$name --dst $DST_URL/$name --work-dir $wd-invalid --exclude 'keep/sub'
check invalid-no-run-json-or-scan bash -c "! sudo -n test -e $wd-invalid/run.json && ! sudo -n test -e $wd-invalid/scan"
step invalid-work-dir-contents 0 sudo -n ls -A $wd-invalid
check invalid-dest-untouched bash -c "test -z \"\$(sudo -n ls -A $dst)\""

step copy 0 mg copy --src $SRC_URL/$name --dst $DST_URL/$name --work-dir $wd --exclude .snapshot --exclude '*.tmp' --exclude .zfs
check dest-has-exactly-the-kept-entries diff <(sudo -n find $dst -mindepth 1 -printf '%P\n' | LC_ALL=C sort) <(kept)
check run-json-keeps-excludes sudo -n jq -e '.exclude == [".snapshot", "*.tmp", ".zfs"]' $wd/run.json
index_paths $wd > $TD/index-paths.txt
check index-has-exactly-the-kept-paths diff $TD/index-paths.txt <(kept | sed 's|^|/|')

note "add files under kept and excluded directories, then sync"
for f in keep/new.txt .zfs/snapshot/new.txt build.tmp/new.o keep/sub/.zfs/new.txt keep/sub/cache.tmp/new.bin; do
    echo "new $f" | sudo -n tee "$src/$f" >/dev/null || exit 1
done
step sync 0 mg sync --work-dir $wd
check sync-copied-only-kept diff <(sudo -n find $dst -mindepth 1 -printf '%P\n' | LC_ALL=C sort) <(kept keep/new.txt)
step sync-before-cutover 0 mg sync --work-dir $wd
step cutover 0 mg sync --work-dir $wd --cutover
step cutover-report 0 bash -c "sudo -n find $wd/passes -name verify.json | LC_ALL=C sort | tail -1 | xargs sudo -n cat"
