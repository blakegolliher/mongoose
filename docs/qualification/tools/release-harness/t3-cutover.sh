#!/usr/bin/env bash
# T3 cutover verification (PR-03) on real NFSv3: a 4 GiB file, fan-out over
# 32 directories, symlinks, an empty file and directory, a private directory,
# and an excluded subtree. A clean tree passes. Ten kinds of destination
# damage made after the last sync each fail it, the baseline stays put and
# the failed pass is kept; repaired, it passes again.
: "${RUN:?set RUN}"
T=t3-cutover-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=cutover-$RUN
src=$SRC_MNT/$name dst=$DST_MNT/$name wd=$WD/$name aside=$DST_MNT/$name-moved-aside
note "fixture: $src"
sudo -n python3 -I $H/tools/make-bulk-tree.py $SRC_FIX/$name/fan 32 100 2 256 7 || exit 1
sudo -n mkdir -p $SRC_FIX/$name/big $SRC_FIX/$name/empty-dir $SRC_FIX/$name/private $SRC_FIX/$name/.zfs/snap || exit 1
sudo -n dd if=/dev/urandom of=$SRC_FIX/$name/big/huge.bin bs=4M count=1024 status=none || exit 1
sudo -n bash -c "cd $SRC_FIX/$name && : > empty.txt && echo secret > private/key && chown -R 1002:1002 private && chmod 0700 private && chmod 0600 private/key \
    && ln -s fan/d00/f0000.dat link-rel && ln -s /nowhere/at/all link-dangling && echo excluded > .zfs/snap/x.txt" || exit 1
sudo -n mkdir -p $dst $aside || exit 1
step fixture-summary 0 bash -c "sudo -n find $src -printf '%y\n' | sort | uniq -c; sudo -n du -s --apparent-size --block-size=1 $src"

step copy 0 mg copy --src $SRC_URL/$name --dst $DST_URL/$name --work-dir $wd --exclude .zfs
step sync 0 mg sync --work-dir $wd
step cutover-clean 0 mg sync --work-dir $wd --cutover
sudo -n cat $wd/baseline.json > $TD/baseline-before.json

note "damage the destination ten ways"
sudo -n bash -c "set -e; cd $dst
  printf XXXX | dd of=fan/d01/f0001.dat bs=1 seek=100 conv=notrunc status=none
  truncate -s 1000 fan/d02/f0002.dat
  echo extra > fan/d03/extra.txt
  mv fan/d04/f0004.dat $aside/f0004.dat
  mv fan/d05/f0005.dat $aside/f0005.dat && mkdir fan/d05/f0005.dat
  mv link-rel $aside/link-rel && ln -s fan/d09/f0009.dat link-rel
  chmod 0777 fan/d06/f0006.dat
  chown 1004:1004 fan/d07/f0007.dat
  touch -m -d '2001-01-01 00:00:00' fan/d08/f0008.dat
  python3 -I -c 'import sys; f = open(sys.argv[1], \"r+b\"); f.seek(3221225472); b = f.read(1)[0]; f.seek(3221225472); f.write(bytes([b ^ 0xFF]))' big/huge.bin" || exit 1
check huge-byte-differs bash -c "! cmp -s <(sudo -n dd if=$src/big/huge.bin bs=1 skip=3221225472 count=1 status=none) <(sudo -n dd if=$dst/big/huge.bin bs=1 skip=3221225472 count=1 status=none)"
expected='/big/huge.bin /fan/d01/f0001.dat /fan/d02/f0002.dat /fan/d03/extra.txt /fan/d04/f0004.dat /fan/d05/f0005.dat /fan/d06/f0006.dat /fan/d07/f0007.dat /fan/d08/f0008.dat /link-rel'
step cutover-damaged 1 mg sync --work-dir $wd --cutover
failed=$(sudo -n bash -c "ls -d $wd/passes/pass-*-failed-* | tail -1")
sudo -n jq -c '{path: (.path_b64 | @base64d), kind, expected, actual}' $failed/verify/mismatches.jsonl > $TD/mismatches.jsonl
step mismatches 0 cat $TD/mismatches.jsonl
check every-damage-reported diff <(jq -r .path $TD/mismatches.jsonl | LC_ALL=C sort -u) <(tr ' ' '\n' <<<"$expected" | LC_ALL=C sort)
sudo -n cat $wd/baseline.json > $TD/baseline-after.json
check baseline-not-advanced diff $TD/baseline-before.json $TD/baseline-after.json
step failed-pass-kept 0 sudo -n jq -c '{result, mismatches: (.mismatches // .mismatch_counts // null)}' $failed/verify.json

note "repair the destination"
sudo -n bash -c "set -e; cd $dst; S=$src
  cp -p \$S/fan/d01/f0001.dat fan/d01/f0001.dat
  cp -p \$S/fan/d02/f0002.dat fan/d02/f0002.dat
  mv fan/d03/extra.txt $aside/extra.txt
  mv $aside/f0004.dat fan/d04/f0004.dat
  mv fan/d05/f0005.dat $aside/f0005.dat.dir && mv $aside/f0005.dat fan/d05/f0005.dat
  mv link-rel $aside/link-rel.wrong && mv $aside/link-rel link-rel
  chmod --reference=\$S/fan/d06/f0006.dat fan/d06/f0006.dat
  chown --reference=\$S/fan/d07/f0007.dat fan/d07/f0007.dat
  touch -m -r \$S/fan/d08/f0008.dat fan/d08/f0008.dat
  dd if=\$S/big/huge.bin of=big/huge.bin bs=1 skip=3221225472 seek=3221225472 count=1 conv=notrunc status=none
  touch -m -r \$S/big/huge.bin big/huge.bin" || exit 1
step cutover-repaired 0 mg sync --work-dir $wd --cutover
