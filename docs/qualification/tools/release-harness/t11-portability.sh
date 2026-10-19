#!/usr/bin/env bash
# T11 portability (PR-11 acceptance): the attested binary, bind-mounted
# read-only and unchanged, runs on the oldest declared glibc platform (Rocky
# Linux 9, glibc 2.34) and on Debian 12, in rootful podman with host
# networking: version, help, the libnfs license output, and a real copy,
# sync, and cutover against the lab NFS server.
: "${RUN:?set RUN}"
T=t11-portability-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
declare -A IMAGES=([rocky9]=quay.io/rockylinux/rockylinux:9 [debian12]=docker.io/library/debian:12)
for os in rocky9 debian12; do
    img=${IMAGES[$os]} job=port-$RUN-$os
    step pull-$os 0 sudo -n podman pull -q $img
    step image-digest-$os 0 sudo -n podman image inspect --format '{{.Digest}}' $img
    sudo -n mkdir -p $WD/$job $DST_MNT/$job || exit 1
    run() { sudo -n podman run --rm --network host --security-opt label=disable \
        -v "$BIN":/usr/local/bin/mongoose:ro -v $WD/$job:/work $img "$@"; }
    step os-$os 0 run sh -c 'grep -E "^(PRETTY_NAME|VERSION_ID)=" /etc/os-release; getconf GNU_LIBC_VERSION; uname -m'
    step version-$os 0 run mongoose --version
    step help-$os 0 run mongoose --help
    step licenses-$os 0 run sh -c 'mongoose licenses --component libnfs | head -20'
    step copy-$os 0 run mongoose copy --src $SRC_URL/cutover-2/fan/d00 --dst $DST_URL/$job --work-dir /work/job
    step sync-$os 0 run mongoose sync --work-dir /work/job
    step cutover-$os 0 run mongoose sync --work-dir /work/job --cutover
    check digest-unchanged-$os check_bin
done
