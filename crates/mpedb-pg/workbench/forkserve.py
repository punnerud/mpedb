#!/usr/bin/env python3
"""inetd for `mpedb-pg serve --inherited-fd`: one PROCESS per connection.

`mpedb-pg serve --unix` accepts serially on purpose — the crate's model is one
process per connection, with systemd providing the parallelism (see the comment
on `accept_loop`). That is a deployment decision, not a limitation, but it means
any client holding a POOL — every ORM does — blocks on its second connection.

So a benchmark has to run the server the way it is meant to be run. This is the
smallest thing that does: accept, fork, dup the connected socket onto fd 0, exec.
`systemd-socket-activate --accept` was tried first and waits for the child, which
is the same serialisation one layer up.

    forkserve.py <socket-dir> <mpedb-pg binary> <db.toml>
"""
import os
import socket
import sys

sock_dir, binary, db = sys.argv[1], sys.argv[2], sys.argv[3]
path = os.path.join(sock_dir, ".s.PGSQL.5432")   # libpq's unix socket name
if os.path.exists(path):
    os.unlink(path)
os.makedirs(sock_dir, exist_ok=True)

lsn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
lsn.bind(path)
lsn.listen(128)
print(f"listening on {path}", flush=True)

while True:
    conn, _ = lsn.accept()
    if os.fork() == 0:
        lsn.close()
        os.dup2(conn.fileno(), 0)
        conn.close()
        os.execv(binary, [binary, "serve", "--inherited-fd", db])
        os._exit(127)
    conn.close()
    # Reap without blocking: a connection's process outlives this loop turn.
    try:
        while os.waitpid(-1, os.WNOHANG)[0]:
            pass
    except ChildProcessError:
        pass
