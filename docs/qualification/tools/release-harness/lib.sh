# Shared helpers for the hardware qualification (see README.md). Source it;
# set T first. The environment names the binary, the server, and where logs go.
H=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
Q=${QUAL_LOGS:?set QUAL_LOGS to the log directory}
BIN=${MONGOOSE_BIN:?set MONGOOSE_BIN to the attested binary}
BIN_SHA=${MONGOOSE_SHA256:?set MONGOOSE_SHA256 to its attested SHA-256}
HOST=${QUAL_SERVER:?set QUAL_SERVER to the NFS server name}
QT=${QUAL_TREE:-qual-0.2.1}   # the test root under each export
SRC_MNT=/mnt/qual-src-noac   # noac,lookupcache=none mount of /source/$QT
DST_MNT=/mnt/qual-dst-noac   # noac,lookupcache=none mount of /dest/$QT
SRC_FIX=/mnt/source/$QT   # cached mount, for writing large fixtures fast
SRC_URL=nfs://$HOST/source/$QT
DST_URL=nfs://$HOST/dest/$QT
WD=/var/lib/mongoose/$QT
: "${T:?set T to the test name}"
TD=$Q/$T
mkdir -p "$TD"
n_step=$(ls "$TD"/*.log 2>/dev/null | wc -l)

now() { date -u +%Y-%m-%dT%H:%M:%S.%3NZ; }
check_bin() {
    echo "$BIN_SHA  $BIN" | sha256sum -c --quiet - || { echo "binary digest mismatch" >&2; exit 1; }
}
note() { echo "[$T] $*"; jq -cn --arg t "$(now)" --arg m "$*" '{note: $m, at: $t}' >> "$TD/steps.jsonl"; }

# step NAME EXPECT CMD...: run CMD, log to NN-NAME.log, record the result.
# EXPECT is an exit code or "any". Returns CMD's exit code.
step() {
    local name=$1 expect=$2; shift 2
    n_step=$((n_step + 1))
    local log
    log=$(printf '%s/%02d-%s.log' "$TD" "$n_step" "$name")
    local start end rc
    start=$(now)
    { printf '$'; printf ' %q' "$@"; printf '\n'; } > "$log"
    "$@" >> "$log" 2>&1
    rc=$?
    end=$(now)
    local ok=true
    [[ "$expect" == any || "$rc" == "$expect" ]] || ok=false
    jq -cn --arg name "$name" --arg log "$(basename "$log")" --arg sha "$(sha256sum "$log" | cut -d' ' -f1)" \
        --arg cmd "$(printf '%q ' "$@")" --arg start "$start" --arg end "$end" \
        --argjson rc "$rc" --arg expect "$expect" --argjson ok "$ok" \
        '{step: $name, command: $cmd, started: $start, finished: $end, exit: $rc, expected: $expect, ok: $ok, log: $log, log_sha256: $sha}' \
        >> "$TD/steps.jsonl"
    echo "[$T] $name: exit $rc (expected $expect) $([[ $ok == true ]] && echo ok || echo MISMATCH)  $(basename "$log")"
    return $rc
}

# check NAME CMD...: an assertion; recorded like a step, expected exit 0.
check() { local name=$1; shift; step "check-$name" 0 "$@"; }

mg() { sudo -n "$BIN" "$@"; }

# Snapshot a tree (path, type, mode, owner, size, mtime, ctime) for no-change proofs.
snap() { sudo -n find "$1" -printf '%P\t%y\t%m\t%U:%G\t%s\t%T@\t%C@\n' | LC_ALL=C sort; }

# Every path in a work dir's canonical index, sorted (copied out so the
# user's duckdb can read it).
index_paths() {
    local tmp
    tmp=$(mktemp -d "$TD/index.XXXX")
    sudo -n cp "$1"/canonical/*.parquet "$tmp"/ && sudo -n chown -R "$(id -u)" "$tmp"
    duckdb -noheader -list -c "select decode(path) from read_parquet('$tmp/*.parquet')" | LC_ALL=C sort
    rm -r "$tmp"
}

# bg_start NAME MONGOOSE-ARGS...: start mongoose in the background as root.
# Its own PID (not sudo's) goes to $TD/NAME.pid and BG_PID; finish with
# bg_wait NAME EXPECT, which records the step like `step` does.
declare -A BG_SUDO BG_LOG BG_START BG_CMD
bg_start() {
    local name=$1; shift
    n_step=$((n_step + 1))
    local log
    log=$(printf '%s/%02d-%s.log' "$TD" "$n_step" "$name")
    { printf '$ mongoose'; printf ' %q' "$@"; printf '   (background)\n'; } > "$log"
    rm -f "$TD/$name.pid"
    BG_START[$name]=$(now)
    BG_CMD[$name]="mongoose $(printf '%q ' "$@")"
    sudo -n bash -c 'echo $$ > "$0"; exec "$@"' "$TD/$name.pid" "$BIN" "$@" >> "$log" 2>&1 &
    BG_SUDO[$name]=$!
    BG_LOG[$name]=$log
    local i
    for i in $(seq 100); do [[ -s $TD/$name.pid ]] && break; sleep 0.1; done
    BG_PID=$(cat "$TD/$name.pid")
    echo "[$T] $name: started, pid $BG_PID"
}
bg_wait() {
    local name=$1 expect=$2 rc
    wait "${BG_SUDO[$name]}"
    rc=$?
    local log=${BG_LOG[$name]} ok=true
    [[ "$expect" == any || "$rc" == "$expect" ]] || ok=false
    jq -cn --arg name "$name" --arg log "$(basename "$log")" --arg sha "$(sha256sum "$log" | cut -d' ' -f1)" \
        --arg cmd "${BG_CMD[$name]}" --arg start "${BG_START[$name]}" --arg end "$(now)" \
        --argjson rc "$rc" --arg expect "$expect" --argjson ok "$ok" \
        '{step: $name, command: $cmd, started: $start, finished: $end, exit: $rc, expected: $expect, ok: $ok, log: $log, log_sha256: $sha, background: true}' \
        >> "$TD/steps.jsonl"
    echo "[$T] $name: exit $rc (expected $expect) $([[ $ok == true ]] && echo ok || echo MISMATCH)  $(basename "$log")"
    return $rc
}
# wait_until TIMEOUT_S CMD...: poll CMD every 0.2 s; 1 on timeout.
wait_until() {
    local deadline=$(( $(date +%s) + $1 )); shift
    until "$@" >/dev/null 2>&1; do
        (( $(date +%s) < deadline )) || return 1
        sleep 0.2
    done
}
# completed_shards WD: how many shards progress.json lists as completed.
completed_shards() { sudo -n jq '(.completed_shards // .completed // []) | length' "$1/progress.json" 2>/dev/null || echo 0; }
# log_of NAME: the newest NN-NAME.log of this test.
log_of() { ls "$TD"/[0-9][0-9]-"$1".log | tail -1; }
