"""Build the real-NFS special-file qualification tree.

Usage: make-special-tree.py ROOT   (run as root)

Creates, under ROOT: a regular control file, a hardlinked pair, a symlink
and a dangling symlink, a regular file with a non-UTF-8 name, FIFOs (one
with a non-UTF-8 name, one in a subdirectory), and a bound Unix socket.
Entries that already exist are left alone, so a run that stopped can be
finished. An entry the server refuses is reported as a server limitation.
Prints one line per entry with its type, mode, owner and inode, as the
kernel client sees them.
"""
import os
import socket
import stat
import sys

root = os.fsencode(sys.argv[1])
os.umask(0)
if not os.path.lexists(root):
    os.mkdir(root, 0o755)
j = lambda *parts: os.path.join(root, *parts)


def make(path, create):
    if os.path.lexists(path):
        return
    try:
        create(path)
    except OSError as e:
        print(f"server limitation: {path!r}: {e}")


def regular(data, mode, uid, gid):
    def create(path):
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
        os.write(fd, data)
        os.close(fd)
        os.chown(path, uid, gid)
    return create


def fifo(mode, uid, gid):
    def create(path):
        os.mkfifo(path, mode)
        os.chown(path, uid, gid)
    return create


def unix_socket(mode, uid, gid):
    def create(path):
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.bind(path)
        finally:
            s.close()
        os.chmod(path, mode)
        os.chown(path, uid, gid)
    return create


make(j(b"control.txt"), regular(b"control file for special-file qualification\n", 0o644, 1001, 1001))
make(j(b"data.bin"), regular(os.urandom(65536), 0o640, 1002, 1002))
make(j(b"data-link.bin"), lambda p: os.link(j(b"data.bin"), p))
make(j(b"link-to-control"), lambda p: os.symlink(b"control.txt", p))
make(j(b"dangling-link"), lambda p: os.symlink(b"missing-target", p))
make(j(b"caf\xe9.txt"), regular(b"latin-1 name\n", 0o644, 1001, 1001))
make(j(b"pipe.fifo"), fifo(0o640, 1003, 1004))
make(j(b"pipe-\xff.fifo"), fifo(0o600, 1005, 1005))
make(j(b"nested"), lambda p: os.mkdir(p, 0o755))
make(j(b"nested", b"inner.fifo"), fifo(0o644, 1006, 1006))
make(j(b"sock.sock"), unix_socket(0o660, 1007, 1007))

for dirpath, dirnames, filenames in os.walk(root):
    for name in sorted(dirnames + filenames):
        p = os.path.join(dirpath, name)
        st = os.lstat(p)
        kind = {stat.S_IFREG: "file", stat.S_IFDIR: "dir", stat.S_IFLNK: "symlink",
                stat.S_IFIFO: "fifo", stat.S_IFSOCK: "socket"}[stat.S_IFMT(st.st_mode)]
        rel = os.path.relpath(p, root)
        print(f"{kind:8} {stat.S_IMODE(st.st_mode):04o} {st.st_uid}:{st.st_gid} "
              f"ino={st.st_ino} nlink={st.st_nlink} {rel!r}")
