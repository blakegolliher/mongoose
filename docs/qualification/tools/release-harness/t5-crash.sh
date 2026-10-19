#!/usr/bin/env bash
# T5 crash (PR-05, with PR-06's stale-lock recovery): SIGKILL in the copy,
# the sync delta copy, and the cutover read-back. A per-file failure recorded
# before the kill survives it and is retried by the next sync; every rerun
# resumes past the leftover lock file. The cutover then fails only on the
# stale `.partial` files the killed processes left (REFERENCE.md: "an extra
# fails, including ... any stale `.partial` file from an interrupted copy");
# moved aside, as an operator would remove them, the cutover passes.
# The failure: a non-empty directory on the destination where the source has
# the file d00/f0004.dat, so its publish cannot rename over it.
: "${RUN:?set RUN}"
T=t5-crash-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=crash-$RUN
dst=$DST_MNT/$name wd=$WD/$name
blocker=d00/f0004.dat
sudo -n mkdir -p $dst/$blocker $DST_MNT/$name-moved-aside || exit 1
echo "blocks the publish of $blocker" | sudo -n tee $dst/$blocker/keep >/dev/null || exit 1
copy_args=(copy --src $SRC_URL/bulk --dst $DST_URL/$name --work-dir $wd --parallel 2)
n_done() { sudo -n jq '.completed_shards | length' $wd/progress.json 2>/dev/null || echo 0; }

bg_start copy-killed "${copy_args[@]}"
wait_until 900 bash -c "sudo -n jq -e '.completed_shards | index(\"canonical/part-r01-00000.parquet\")' $wd/progress.json && (( \$(sudo -n jq '.completed_shards | length' $wd/progress.json) >= 4 ))" || note "shard r01 never completed"
sudo -n kill -KILL "$BG_PID"
bg_wait copy-killed 137
step progress-after-kill 0 sudo -n jq -c '{shards_total, completed: (.completed_shards | length)}' $wd/progress.json
check copy-was-incomplete bash -c "(( \$(sudo -n jq '.completed_shards | length' $wd/progress.json) < 32 ))"
index_paths_by_shard() {
    local tmp; tmp=$(mktemp -d "$TD/index.XXXX")
    sudo -n cp $wd/canonical/*.parquet "$tmp"/ && sudo -n chown -R "$(id -u)" "$tmp"
    duckdb -noheader -list -c "select regexp_extract(src_file, 'part-r[0-9]+-[0-9]+', 0) from read_parquet('$tmp/*.parquet', filename='src_file') where decode(path) = '/$blocker'"
    rm -r "$tmp"
}
shard=$(index_paths_by_shard)
note "blocker $blocker is in shard $shard"
check blocker-shard-completed sudo -n jq -e --arg s "canonical/$shard.parquet" '.completed_shards | index($s)' $wd/progress.json
step failure-record 0 sudo -n cat $wd/failures/$shard.jsonl
check failure-record-names-blocker bash -c "sudo -n jq -r '.path_b64 | @base64d' $wd/failures/$shard.jsonl | grep -qx /$blocker"
step stale-lock-file 0 sudo -n cat $wd/.mongoose.lock

step copy-resume 2 mg "${copy_args[@]}"
check copy-now-complete bash -c "(( \$(sudo -n jq '.completed_shards | length' $wd/progress.json) == 32 ))"

note "move the blocker aside (out of the job's tree), change the source, SIGKILL the sync mid-copy"
sudo -n mv $dst/$blocker $DST_MNT/$name-moved-aside/ || exit 1
sudo -n python3 -I - $SRC_MNT/bulk "$T" <<'PY'
import sys
root, tag = sys.argv[1], sys.argv[2].encode()
for d in range(64):
    for f in range(1, 300, 10):
        with open(f"{root}/d{d:02d}/f{f:04d}.dat", "ab") as fh:
            fh.write(b"appended by " + tag + b"\n")
PY
step baseline-before-sync 0 bash -c "sudo -n cat $wd/baseline.json 2>/dev/null || echo absent"
bg_start sync-killed sync --work-dir $wd --parallel 2
wait_until 900 grep -q '^shard ' "${BG_LOG[sync-killed]}" || note "sync never started copying"
sudo -n kill -KILL "$BG_PID"
bg_wait sync-killed 137
step baseline-after-kill 0 bash -c "sudo -n cat $wd/baseline.json 2>/dev/null || echo absent"
check baseline-not-advanced diff <(tail -n +2 $(log_of baseline-before-sync)) <(tail -n +2 $(log_of baseline-after-kill))
step sync-resume 0 mg sync --work-dir $wd
check blocker-retried-and-copied sudo -n cmp $SRC_MNT/bulk/$blocker $dst/$blocker
step sync-quiet 0 mg sync --work-dir $wd

bg_start cutover-killed sync --work-dir $wd --cutover --parallel 2
wait_until 900 grep -q 'verify the destination against the source' "${BG_LOG[cutover-killed]}" || note "cutover never reached read-back"
sleep 2
sudo -n kill -KILL "$BG_PID"
bg_wait cutover-killed 137
step cutover-resume 1 mg sync --work-dir $wd --cutover
report=$(sudo -n bash -c "ls -d $wd/passes/pass-*-failed-* | tail -1")
sudo -n jq -c '. + {path: (.path_b64 | @base64d)}' $report/verify/mismatches.jsonl > $TD/mismatches.jsonl
step mismatches 0 cat $TD/mismatches.jsonl
killed="$(cat $TD/copy-killed.pid)|$(cat $TD/sync-killed.pid)|$(cat $TD/cutover-killed.pid)"
check only-stale-partials-of-killed-runs bash -c "jq -e -s --arg re '/\\.[^/]+\\.$(hostname)\\.($killed)\\.partial\$' 'length > 0 and all(.kind == \"extra\" and (.path | test(\$re)))' $TD/mismatches.jsonl"
note "move the stale .partial files aside"
for p in $(jq -r .path $TD/mismatches.jsonl); do
    sudo -n mv "$dst$p" $DST_MNT/$name-moved-aside/ || exit 1
done
step cutover-after-cleanup 0 mg sync --work-dir $wd --cutover
