"""Recreate the qualification tree's special nodes on the destination.

Usage: recreate-on-dest.py DEST_ROOT   (run as root)

Same type, mode and owner as the source for pipe.fifo (FIFO) and
sock.sock (bound socket); a regular file where nested/inner.fifo should
be, to check that a wrong type is reported. Refuses to replace anything
that already exists.
"""
import os
import socket
import stat
import sys

root = os.fsencode(sys.argv[1])
os.umask(0)
j = lambda *parts: os.path.join(root, *parts)
for p in (j(b"pipe.fifo"), j(b"sock.sock"), j(b"nested", b"inner.fifo")):
    if os.path.lexists(p):
        sys.exit(f"{p!r} already exists; not touching it")

os.mkfifo(j(b"pipe.fifo"), 0o640)
os.chown(j(b"pipe.fifo"), 1003, 1004)

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.bind(j(b"sock.sock"))
finally:
    s.close()
os.chmod(j(b"sock.sock"), 0o660)
os.chown(j(b"sock.sock"), 1007, 1007)

fd = os.open(j(b"nested", b"inner.fifo"), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
os.close(fd)
os.chown(j(b"nested", b"inner.fifo"), 1006, 1006)

for p in (j(b"pipe.fifo"), j(b"sock.sock"), j(b"nested", b"inner.fifo")):
    st = os.lstat(p)
    kind = {stat.S_IFREG: "file", stat.S_IFIFO: "fifo", stat.S_IFSOCK: "socket"}[stat.S_IFMT(st.st_mode)]
    print(f"{kind:7} {stat.S_IMODE(st.st_mode):04o} {st.st_uid}:{st.st_gid} {os.path.relpath(p, root)!r}")
