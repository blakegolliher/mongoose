#!/usr/bin/env bash
# T4 alias overlap (PR-04): two names for one server, a spelled port and
# case, and two different addresses of one server are refused when the paths
# overlap, before any destination write; sibling paths on one export and two
# exports on one server are allowed.
: "${RUN:?set RUN}"
T=t4-alias-$RUN
source "$(dirname "$0")/lib.sh"
check_bin
name=alias-$RUN
base=$SRC_MNT/$name
IP1=${QUAL_VIP_A:?set QUAL_VIP_A} IP3=${QUAL_VIP_B:?set QUAL_VIP_B}   # two addresses of the server
P=/source/$QT/$name
note "fixture: $base"
sudo -n mkdir -p $base/tree/sub $base/tree/inner $base/sibling $DST_MNT/$name/tree || exit 1
echo a | sudo -n tee $base/tree/a.txt >/dev/null && echo b | sudo -n tee $base/tree/sub/b.txt >/dev/null || exit 1
step fixture-tree 0 snap $base

# refuse CASE PROBE SRC DST: refused, nothing in the tree changed, and no
# copy started. PROBE=probe: the refusal comes from the mounted-identity
# probe, which makes and removes one directory under the destination root,
# so only that root's times may change, and the local scan and plan exist.
refuse() {
    local c=$1 probe=$2 src=$3 dst=$4
    snap $base > $TD/before-$c.txt
    step refuse-$c 1 mg copy --src "$src" --dst "$dst" --work-dir $WD/$name-$c
    snap $base > $TD/after-$c.txt
    step tree-diff-$c any diff $TD/before-$c.txt $TD/after-$c.txt
    check same-entries-$c diff <(cut -f1-5 $TD/before-$c.txt) <(cut -f1-5 $TD/after-$c.txt)
    if [[ $probe == probe ]]; then
        check only-dest-root-times-$c diff <(grep -v -P '^tree/inner\t' $TD/before-$c.txt) <(grep -v -P '^tree/inner\t' $TD/after-$c.txt)
        check no-copy-started-$c bash -c "! sudo -n test -e $WD/$name-$c/progress.json"
    else
        check no-change-$c diff $TD/before-$c.txt $TD/after-$c.txt
        check no-scan-or-plan-$c bash -c "! sudo -n test -e $WD/$name-$c/scan && ! sudo -n test -e $WD/$name-$c/manifest.json"
    fi
}
refuse name-vs-address  - nfs://$HOST$P/tree  nfs://$IP1$P/tree/inner
refuse port-and-case    - nfs://$HOST$P/tree  "nfs://$(tr a-z A-Z <<<"$HOST"):2049$P"
refuse same-path        - nfs://$HOST$P/tree  nfs://$IP3$P/tree
refuse two-addresses    probe nfs://$IP1$P/tree   nfs://$IP3$P/tree/inner

step allow-sibling 0 mg copy --src nfs://$HOST$P/tree --dst nfs://$IP3$P/sibling --work-dir $WD/$name-sibling
check sibling-copied diff <(sudo -n find $base/tree -mindepth 1 -printf '%P %y %s\n' | LC_ALL=C sort) <(sudo -n find $base/sibling -mindepth 1 -printf '%P %y %s\n' | LC_ALL=C sort)
step allow-sibling-sync 0 mg sync --work-dir $WD/$name-sibling
step allow-sibling-cutover 0 mg sync --work-dir $WD/$name-sibling --cutover

step allow-two-exports 0 mg copy --src nfs://$IP1$P/tree --dst nfs://$HOST/dest/$QT/$name/tree --work-dir $WD/$name-exports
step allow-two-exports-sync 0 mg sync --work-dir $WD/$name-exports
step allow-two-exports-cutover 0 mg sync --work-dir $WD/$name-exports --cutover
