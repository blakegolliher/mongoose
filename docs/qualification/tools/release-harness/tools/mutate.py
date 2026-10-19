#!/usr/bin/env python3
"""Keep rewriting 4 KiB at random offsets of the given files until STOP exists.

mutate.py STOP FILE...   Size never changes; mtime and ctime do. Prints how
many writes it made. Run as root.
"""
import os, random, sys, time

stop, files = sys.argv[1], sys.argv[2:]
rng = random.Random(5)
sizes = {f: os.path.getsize(f) for f in files}
n = 0
while not os.path.exists(stop):
    for f in files:
        with open(f, "r+b") as fh:
            fh.seek(rng.randrange(0, sizes[f] - 4096))
            fh.write(rng.randbytes(4096))
        n += 1
    time.sleep(0.02)
print(f"{n} writes")
