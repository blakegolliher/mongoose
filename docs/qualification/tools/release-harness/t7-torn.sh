#!/usr/bin/env bash
# T7 torn copy (PR-07) on the mover mongoose ships: files rewritten in place
# while the copy reads them are counted as torn and recorded as TORN_COPY
# downgrades; the next sync recopies them; the cutover then passes.
: "${RUN:?set RUN}"
T=t7-torn-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=torn-$RUN
dst=$DST_MNT/$name wd=$WD/$name stop=$TD/stop-mutating
note "fixture: 6 x 1 GiB"
sudo -n mkdir -p $SRC_FIX/$name $dst || exit 1
for i in 0 1 2 3 4 5; do
    sudo -n dd if=/dev/urandom of=$SRC_FIX/$name/f$i.bin bs=4M count=256 status=none || exit 1
done
rm -f $stop
bg_start copy copy --src $SRC_URL/$name --dst $DST_URL/$name --work-dir $wd --parallel 2
wait_until 600 sudo -n test -e $wd/manifest.json || note "index never built"
note "index built; mutating the source while it is copied"
sudo -n python3 -I $H/tools/mutate.py $stop $SRC_FIX/$name/f{0,1,2,3,4,5}.bin > $TD/mutate.out 2>&1 &
mutator=$!
bg_wait copy 0
touch $stop; wait $mutator
step mutator 0 cat $TD/mutate.out
step copy-summary 0 grep -E '^complete:' $(log_of copy)
check some-torn bash -c "grep -E '^complete:' $(log_of copy) | grep -qE '\(([1-9][0-9]*) torn\)'"
sudo -n bash -c "cat $wd/downgrades/*.jsonl" > $TD/downgrades.jsonl 2>/dev/null
step torn-records 0 jq -c '. + {path: (.path_b64 | @base64d)}' $TD/downgrades.jsonl
check torn-records-match-count bash -c "n=\$(grep -oE '\(([0-9]+) torn\)' $(log_of copy) | grep -oE '[0-9]+'); [[ \$(grep -c -i torn $TD/downgrades.jsonl) == \$n ]]"
step sync 0 mg sync --work-dir $wd
check sync-recopied-torn bash -c "grep -E '^complete:' $(log_of sync) | grep -q '(0 torn)'"
step sync-quiet 0 mg sync --work-dir $wd
step cutover 0 mg sync --work-dir $wd --cutover
