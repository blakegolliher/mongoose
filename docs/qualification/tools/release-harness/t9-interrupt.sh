#!/usr/bin/env bash
# T9 interrupted work (PR-09): SIGINT mid-copy exits 130, SIGTERM mid-sync
# exits 143 without advancing the baseline, and re-running resumes both.
: "${RUN:?set RUN}"
T=t9-interrupt-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=interrupt-$RUN
dst=$DST_MNT/$name wd=$WD/$name
sudo -n mkdir -p $dst || exit 1
copy_args=(copy --src $SRC_URL/bulk --dst $DST_URL/$name --work-dir $wd --parallel 2)

bg_start copy-sigint "${copy_args[@]}"
wait_until 900 bash -c "(( \$(sudo -n jq '.completed_shards | length' $wd/progress.json 2>/dev/null || echo 0) >= 4 ))" || note "never reached 4 shards"
sudo -n kill -INT "$BG_PID"
bg_wait copy-sigint 130
step progress-after-sigint 0 sudo -n jq -c '{shards_total, completed: (.completed_shards | length)}' $wd/progress.json
check copy-was-incomplete bash -c "(( \$(sudo -n jq '.completed_shards | length' $wd/progress.json) < 32 ))"
step copy-resume 0 mg "${copy_args[@]}"
check copy-now-complete bash -c "(( \$(sudo -n jq '.completed_shards | length' $wd/progress.json) == 32 ))"

# change N: append to 1,920 small files (30 per dir) and overwrite two bytes in each big file.
change() {
    sudo -n python3 -I - $SRC_MNT/bulk "$1" <<'PY'
import sys
root, tag = sys.argv[1], sys.argv[2].encode()
for i in range(1920):
    d, f = divmod(i, 30)
    with open(f"{root}/d{d:02d}/f{f * 10:04d}.dat", "ab") as fh:
        fh.write(b"appended " + tag + b"\n")
for b in range(16):
    with open(f"{root}/big/b{b:02d}.dat", "r+b") as fh:
        fh.seek(12345)
        fh.write(tag[:2].ljust(2, b"."))
PY
    local rc=$?
    note "change $1: exit $rc"
    return $rc
}
note "change the source and sync, so a baseline exists"
change "$RUN-a"
step sync-1 0 mg sync --work-dir $wd
sudo -n cat $wd/baseline.json > $TD/baseline-before.json
step baseline-before 0 cat $TD/baseline-before.json
note "change the source again, then SIGTERM the sync once it is copying"
change "$RUN-b"
bg_start sync-sigterm sync --work-dir $wd --parallel 2
wait_until 900 grep -q '^shard ' "${BG_LOG[sync-sigterm]}" || note "sync never started copying"
sudo -n kill -TERM "$BG_PID"
bg_wait sync-sigterm 143
sudo -n cat $wd/baseline.json > $TD/baseline-after.json
step baseline-after 0 cat $TD/baseline-after.json
check baseline-not-advanced diff $TD/baseline-before.json $TD/baseline-after.json
step sync-resume 0 mg sync --work-dir $wd
step sync-quiet 0 mg sync --work-dir $wd
step cutover 0 mg sync --work-dir $wd --cutover
