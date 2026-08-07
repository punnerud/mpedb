# Deploying mpedb without a server

Everything here rests on one decision: **mpedb-pg does not open a listening
socket.** A connection arrives on a file descriptor the OS already accepted, one
process serves it, and the process exits. There is nothing resident between
connections to start, supervise, restart or leak.

That is DESIGN-SERVICE §1 Model A applied to a wire protocol instead of to a
queue, and it is what keeps the no-server contract intact while still letting
`psql`, `psycopg` and JDBC connect.

## The three pieces

| file | what it is |
|---|---|
| `mpedb-pg.socket` | the listening socket, owned by systemd |
| `mpedb-pg@.service` | the per-connection unit systemd spawns |
| `nginx-stream.conf` | remote access and TLS termination |
| `mpedb-cron.example` | scheduled work, projected onto the OS scheduler |

## systemd socket activation (the default)

`Accept=yes` is the important line. It makes systemd accept each connection
itself and spawn one instance of the template unit with that connection already
on stdin — which is exactly what `mpedb-pg serve --inherited-fd` expects.

```sh
sudo cp mpedb-pg.socket mpedb-pg@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now mpedb-pg.socket
psql -h /run/mpedb -d app
```

Nothing is running until the first connection. `systemctl status mpedb-pg.socket`
shows the socket listening; there is no `mpedb-pg.service` to look at.

**Why a unix socket and not TCP.** The peer of a unix socket is authenticated by
filesystem permissions, which is a stronger statement than a password in a config
file — and it is the same fence mpedb already relies on for the file itself
(DESIGN-MULTIDB's trust box: separate files are the hard, OS-enforced boundary).
`0660` plus a group is the whole access-control story.

## nginx, for remote access

nginx terminates TLS and proxies to the unix socket. A process spawned per
connection has no business holding a certificate, and `mpedb-pg` answers
PostgreSQL's `SSLRequest` with `N` so every client falls back cleanly to the
plaintext connection nginx has already secured on the outside.

```sh
sudo cp nginx-stream.conf /etc/nginx/streams-enabled/mpedb.conf
sudo nginx -t && sudo systemctl reload nginx
```

## Scheduled work

The DB is the schedule and the OS is the executor — `mpedb advise … --emit-cron`
already prints paste-ready crontab lines, and `mpedb-cron.example` shows the
shape. A timer fires, a process attaches, claims what is due, runs it, and exits.
The "is anything due?" check is a lock-free MVCC read, so overlapping ticks cost
nothing and claim disjoint work by construction.

## Verifying an install

```sh
# The socket exists and nothing is running yet.
systemctl status mpedb-pg.socket
pgrep -a mpedb-pg            # no output

# Connect. A process appears for the duration and then goes away.
psql -h /run/mpedb -d app -c '\d'
pgrep -a mpedb-pg            # no output again
```

If the last `pgrep` prints something, the connection was not closed — that is a
client holding it open, not a daemon.
