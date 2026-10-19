#!/usr/bin/env bash
# T10 raw-mover throughput: copy the fs-sim web tree (115,320 files,
# 58.6 GB) with the default --parallel 32, then verify it. The same tree's
# copy phase took 58-59 s with main builds in October; no threshold is
# defined, so the time is recorded against that figure.
: "${RUN:?set RUN}"
T=t10-throughput-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=throughput-$RUN
sudo -n mkdir -p $DST_MNT/$name || exit 1
step copy 0 mg copy --src nfs://$HOST/source/web --dst $DST_URL/$name --work-dir $WD/$name
step copy-summary 0 grep -E '^(  found|complete:|index ready)' $(log_of copy)
step sync 0 mg sync --work-dir $WD/$name
step cutover 0 mg sync --work-dir $WD/$name --cutover
