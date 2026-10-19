#!/usr/bin/env python3
"""Write a fan-out tree of random files for the qualification runs.

make-bulk-tree.py ROOT DIRS FILES_PER_DIR BIG_FILES BIG_MIB [SEED]

ROOT/dNN/fNNNN.dat: DIRS x FILES_PER_DIR files of 4-64 KiB; ROOT/big/bNN.dat:
BIG_FILES files of BIG_MIB MiB. Owners cycle through uid/gid 1001-1004 and
modes through 0644, 0640, 0600. Contents are pseudo-random from SEED, so the
tree is reproducible. Run as root (it sets owners).
"""
import os, random, sys

root, dirs, per_dir, big, big_mib = sys.argv[1], *map(int, sys.argv[2:6])
rng = random.Random(int(sys.argv[6]) if len(sys.argv) > 6 else 21)
modes = (0o644, 0o640, 0o600)
os.makedirs(root, exist_ok=False)
n = 0
for d in range(dirs):
    dp = os.path.join(root, f"d{d:02d}")
    os.mkdir(dp)
    for f in range(per_dir):
        p = os.path.join(dp, f"f{f:04d}.dat")
        with open(p, "wb") as fh:
            fh.write(rng.randbytes(rng.randint(4096, 65536)))
        os.chown(p, 1001 + n % 4, 1001 + (n // 4) % 4)
        os.chmod(p, modes[n % 3])
        n += 1
bp = os.path.join(root, "big")
os.mkdir(bp)
chunk = 1 << 20
for b in range(big):
    p = os.path.join(bp, f"b{b:02d}.dat")
    with open(p, "wb") as fh:
        for _ in range(big_mib):
            fh.write(rng.randbytes(chunk))
    os.chown(p, 1001, 1001)
print(f"{root}: {n} small files in {dirs} dirs, {big} x {big_mib} MiB")
