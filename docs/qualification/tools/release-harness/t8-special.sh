#!/usr/bin/env bash
# T8 special files: the real-NFS procedure in
# docs/SPECIAL_FILE_TYPE_SAFETY_HANDOFF.md, repeated with the release binary,
# using the tools in docs/qualification/tools at the release tag. Adds a last
# step: with every node recreated with the right type, the cutover passes.
: "${RUN:?set RUN}"
T=t8-special-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=special-$RUN
tools=$(cd "$H/.." && pwd)
src=$SRC_MNT/$name dst=$DST_MNT/$name wd=$WD/$name
step tools-at-tag 0 bash -c "cd $tools && git rev-parse HEAD && git status --porcelain . && sha256sum *.py"
step make-tree 0 sudo -n python3 -I $tools/make-special-tree.py $src
sudo -n mkdir -p $dst || exit 1
step copy any mg copy --src $SRC_URL/$name --dst $DST_URL/$name --work-dir $wd
step counters 0 sudo -n jq -c '{files_ok, files_failed, files_special_not_copied, bytes_moved}' $wd/progress.json
step dest-tree 0 sudo -n find $dst -mindepth 1 -printf '%y %m %U:%G %s %P\n'
check specials-absent bash -c "! sudo -n test -e $dst/pipe.fifo -o -e $dst/sock.sock -o -e $dst/nested/inner.fifo"
check control-identical sudo -n cmp $src/control.txt $dst/control.txt
sudo -n bash -c "cat $wd/downgrades/*.jsonl" > $TD/downgrades.jsonl
step omission-records 0 jq -c '. + {path: (.path_b64 | @base64d)}' $TD/downgrades.jsonl
check omissions-are-the-specials diff <(jq -r '.path_b64 | @base64d' $TD/downgrades.jsonl | LC_ALL=C sort) <(printf '%s\n' /nested/inner.fifo /pipe.fifo /sock.sock)

step cutover-specials-absent 1 mg sync --work-dir $wd --cutover
failed=$(sudo -n bash -c "ls -d $wd/passes/pass-*-failed-* | tail -1")
sudo -n jq -c '{path: (.path_b64 | @base64d), kind, expected, actual}' $failed/verify/mismatches.jsonl > $TD/mismatches-1.jsonl
step mismatches-1 0 cat $TD/mismatches-1.jsonl
check only-special-not-copied jq -e -s 'length == 3 and all(.kind == "special_not_copied")' $TD/mismatches-1.jsonl
# Inspect the first failed pass, which keeps the walker's own output.
w=$(mktemp -d $TD/walker.XXXX) c=$(mktemp -d $TD/canon.XXXX)
sudo -n bash -c "find $failed/scan -type f -name '*.parquet' -exec cp {} $w/ \\; && cp $failed/canonical/*.parquet $c/ && chown -R $(id -u) $w $c"
cat > $TD/walker-rows.sql <<SQL
select path, file_type, printf('%04o', permissions) as perms, uid || ':' || gid as owner, inode, fsid, hex(path_bytes) as path_bytes
from read_parquet('$w/*.parquet') where file_type <> 'directory' or path <> '/' order by path;
SQL
cat > $TD/canonical-tags.sql <<SQL
select decode(path) as p, file_type as tag, printf('%06o', "mode") as mode_octal, printf('%03o', "mode" >> 12) as s_ifmt
from read_parquet('$c/*.parquet') order by 1;
SQL
step walker-rows 0 bash -c "duckdb < $TD/walker-rows.sql"
step canonical-tags 0 bash -c "duckdb < $TD/canonical-tags.sql"

step recreate-on-dest 0 sudo -n python3 -I $tools/recreate-on-dest.py $dst
step cutover-wrong-type 1 mg sync --work-dir $wd --cutover
failed=$(sudo -n bash -c "ls -d $wd/passes/pass-*-failed-* | tail -1")
sudo -n jq -c '{path: (.path_b64 | @base64d), kind, expected, actual}' $failed/verify/mismatches.jsonl > $TD/mismatches-2.jsonl
step mismatches-2 0 cat $TD/mismatches-2.jsonl
check only-inner-fifo-file-type jq -e -s 'length == 1 and .[0].path == "/nested/inner.fifo" and .[0].kind == "file_type"' $TD/mismatches-2.jsonl

note "replace the wrong-type file with a FIFO of the source's mode and owner"
sudo -n mkdir -p $DST_MNT/$name-moved-aside && sudo -n mv $dst/nested/inner.fifo $DST_MNT/$name-moved-aside/ && \
sudo -n python3 -I -c "import os; p='$dst/nested/inner.fifo'; os.umask(0); os.mkfifo(p, 0o644); os.chown(p, 1006, 1006)" || exit 1
step cutover-all-recreated 0 mg sync --work-dir $wd --cutover
