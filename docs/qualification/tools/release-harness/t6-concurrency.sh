#!/usr/bin/env bash
# T6 concurrency (PR-06): while a copy holds a work dir, a second copy and a
# sync on it fail at once, naming the owner, and start nothing (no new scan
# attempt, no pass); two copies racing to create one new work dir leave
# exactly one running.
: "${RUN:?set RUN}"
T=t6-concurrency-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=conc-$RUN
dst=$DST_MNT/$name wd=$WD/$name
sudo -n mkdir -p $dst $DST_MNT/$name-race || exit 1
copy_args=(copy --src $SRC_URL/bulk --dst $DST_URL/$name --work-dir $wd --parallel 2)

bg_start copy-owner "${copy_args[@]}"
owner=$BG_PID
wait_until 900 bash -c "(( \$(sudo -n jq '.completed_shards | length' $wd/progress.json 2>/dev/null || echo 0) >= 2 ))" || note "owner never reached 2 shards"
step lock-file 0 sudo -n cat $wd/.mongoose.lock
state() { sudo -n bash -c "ls -A $wd; echo scan:; ls -A $wd/scan; echo passes:; ls -A $wd/passes 2>&1"; }
state > $TD/state-before.txt
step second-copy 1 timeout 60 sudo -n "$BIN" "${copy_args[@]}"
step second-sync 1 timeout 60 sudo -n "$BIN" sync --work-dir $wd
state > $TD/state-after.txt
check contenders-started-nothing diff $TD/state-before.txt $TD/state-after.txt
check contenders-name-the-owner bash -c "grep -q \"in use (pid=$owner\$\" $(log_of second-copy) && grep -q \"in use (pid=$owner\$\" $(log_of second-sync)"
check owner-still-running sudo -n kill -0 $owner
bg_wait copy-owner 0
step sync 0 mg sync --work-dir $wd
step cutover 0 mg sync --work-dir $wd --cutover

note "two copies race to create one new work dir"
race_args=(copy --src $SRC_URL/bulk --dst $DST_URL/$name-race --work-dir $wd-race --parallel 2)
bg_start race-a "${race_args[@]}"
bg_start race-b "${race_args[@]}"
bg_wait race-a any; ra=$?
bg_wait race-b any; rb=$?
check exactly-one-ran bash -c "[[ \"$ra $rb\" == '0 1' || \"$ra $rb\" == '1 0' ]]"
step race-sync 0 mg sync --work-dir $wd-race
step race-cutover 0 mg sync --work-dir $wd-race --cutover
