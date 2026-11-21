#!/usr/bin/env python3
"""Head-to-head of mpedb-redis and Blobasaur on the blob shape from
https://zerodha.tech/blog/high-performance-distributed-blob-storage/

Their published table is a 16-core laptop, tmpfs, 100 000 keys, 64
connections and 1 600 MB of page cache. This runner does not reproduce
that setup and does not print those numbers next to ours. It measures
both servers with one client, on the disk you pass.

The value is 64 KiB of chart JSON tuned so `zstd -3` lands near 9x, which
is the compressibility the article states. A second, small value (2 KiB of
repeated JSON, under 200 B compressed) is the control for mpedb's
group-commit ring, which carries at most 824 parameter bytes.

Two commands, because the servers do not hash the same bytes:

* SET <key> <json> lands on shard(hash(key)) in both.
* HSET prefs <key> <json> is the article's command. Blobasaur hashes the
  field and spreads. mpedb-redis hashes the hash name (prefs) and keeps
  every field on one file.

Variants. Each one is named for what the server does before it replies.

* blobasaur normal     WAL, synchronous=NORMAL, batch_size=50. No sync
                       before the reply.
* blobasaur normal-b1  the same with batch_size=1. The control for the batch.
* blobasaur full       synchronous=FULL, batch_size=50. One WAL fsync per
                       batch before the reply.
* blobasaur full-b1    the same with batch_size=1.
* mpedb async          no sync before the reply.
* mpedb wal            one log fdatasync before the reply. The log-based
                       pair for `full`.
* mpedb commit         data and meta flushed before the reply. Two flushes.

`normal` pairs with `async`. `full` pairs with `wal` and with `commit`.
With `batch_timeout_ms = 0` Blobasaur's batch is whatever is queued, so
with N connections and one request in flight each it is at most N writes.

Method. Every variant runs --reps times and the order is rotated, so no
system always runs first. Workers connect, then wait on a barrier, and the
clock starts when the barrier opens. A phase runs for a fixed time or a
fixed number of operations, whichever comes first. Before and after each
repetition a probe writes 4 KiB and calls fdatasync 200 times on the same
disk: that is the price of a flush here, and the null control. If the
probe moves between repetitions, the machine moved, not the servers.

Usage:
  python3 benchmarks/blobasaur/run.py \\
      --mpedb /path/mpedb-redis --blobasaur /path/blobasaur \\
      --disk /mnt/HC_Volume_106406886/blob-bench
"""

import argparse
import os
import shutil
import socket
import statistics
import subprocess
import sys
import time

PAYLOAD_LEN = 64 * 1024
PORT = 16379


def payload():
    # 12 000 hex chars from a fixed xorshift, plus a repeated chart object.
    # Measured with zstd 1.5.5 at level 3: 7 166 B, 9.1x. Above the ring cap.
    unique_chars = 12000
    x = 0xC0FFEE123456789
    mask = (1 << 64) - 1
    hex_parts = []
    while sum(len(part) for part in hex_parts) < unique_chars:
        x = (x ^ ((x << 13) & mask)) & mask
        x = (x ^ (x >> 7)) & mask
        x = (x ^ ((x << 17) & mask)) & mask
        hex_parts.append(f"{x:016x}")
    noise = "".join(hex_parts)[:unique_chars]
    prefix = '{"layout":"chart","noise":"' + noise + '","draw":['
    chunks = []
    i = 0
    while True:
        piece = (
            '{"t":"line","c":"#1f77b4","p":[12,18,19,20,21,22,24,23],'
            '"w":1.50,"i":%d},'
        ) % i
        chunks.append(piece)
        raw = (prefix + "".join(chunks)).encode()
        if len(raw) >= PAYLOAD_LEN - 2:
            break
        i += 1
    raw = (prefix + "".join(chunks)).encode()
    raw = raw[: PAYLOAD_LEN - 2] + b"]}"
    if len(raw) != PAYLOAD_LEN:
        raise SystemExit(f"payload is {len(raw)} bytes, wanted {PAYLOAD_LEN}")
    return raw


class Conn:
    def __init__(self, host, port):
        self.s = socket.create_connection((host, port), timeout=30)
        self.s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.buf = bytearray()

    def close(self):
        self.s.close()

    def _fill(self):
        chunk = self.s.recv(65536)
        if not chunk:
            raise EOFError("server closed the connection")
        self.buf += chunk

    def read_exact(self, n):
        while len(self.buf) < n:
            self._fill()
        out = bytes(self.buf[:n])
        del self.buf[:n]
        return out

    def read_line(self):
        while b"\r\n" not in self.buf:
            self._fill()
        i = self.buf.index(b"\r\n")
        line = bytes(self.buf[:i])
        del self.buf[: i + 2]
        return line

    def call(self, parts):
        frame = bytearray()
        frame += f"*{len(parts)}\r\n".encode()
        for part in parts:
            if isinstance(part, str):
                part = part.encode()
            frame += f"${len(part)}\r\n".encode()
            frame += part
            frame += b"\r\n"
        self.s.sendall(frame)
        line = self.read_line()
        if not line:
            raise RuntimeError("empty reply")
        kind = line[:1]
        rest = line[1:]
        if kind == b"+":
            return ("simple", rest)
        if kind == b"-":
            return ("err", rest)
        if kind == b":":
            return ("int", int(rest))
        if kind == b"$":
            n = int(rest)
            if n < 0:
                return ("bulk", None)
            data = self.read_exact(n)
            tail = self.read_exact(2)
            if tail != b"\r\n":
                raise RuntimeError("bulk framing broken")
            return ("bulk", data)
        raise RuntimeError(f"bad reply {line[:40]!r}")


def wait_ping(host, port, proc, log_path, seconds=30):
    deadline = time.time() + seconds
    last = ""
    while time.time() < deadline:
        if proc.poll() is not None:
            tail = open(log_path, "rb").read()[-2000:]
            raise RuntimeError(f"server exited {proc.returncode}: {tail!r}")
        try:
            c = Conn(host, port)
            kind, body = c.call([b"PING"])
            c.close()
            if kind == "simple" and body == b"PONG":
                return
            last = f"{kind} {body!r}"
        except OSError as e:
            last = str(e)
        time.sleep(0.05)
    raise RuntimeError(f"server did not answer PING: {last}")


def small_payload():
    raw = (b'{"t":"line","c":"#1f77b4","p":[12,18,19,20,21,22,24,23],"w":1.50},' * 40)[:2046]
    return raw + b"]}"


def percentile(samples, p):
    if not samples:
        return 0
    ordered = sorted(samples)
    idx = min(len(ordered) - 1, int(round((p / 100) * (len(ordered) - 1))))
    return ordered[idx]


def fsync_probe(root, n=200):
    """The price of one flush on this disk, in microseconds."""
    path = os.path.join(root, "fsync-probe.bin")
    block = os.urandom(4096)
    lat = []
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        for _ in range(n):
            t0 = time.perf_counter()
            os.write(fd, block)
            os.fdatasync(fd)
            lat.append((time.perf_counter() - t0) * 1e6)
    finally:
        os.close(fd)
        os.unlink(path)
    return percentile(lat, 50), percentile(lat, 99)


def one_client(worker, host, port, kind, value, limit, keys, seconds, barrier, out):
    """`keys` is how many keys this worker owns. A write phase creates up to
    `limit` of them. A read phase cycles over the `keys` that exist."""
    conn = Conn(host, port)
    lat = []
    try:
        barrier.wait()
        start = time.perf_counter()
        deadline = start + seconds
        i = 0
        while i < limit and time.perf_counter() < deadline:
            index = i if kind in ("set", "hset") else i % keys
            key = f"u{worker:02d}_{index:07d}"
            if kind == "set":
                parts = [b"SET", key, value]
            elif kind == "hset":
                parts = [b"HSET", b"prefs", key, value]
            elif kind == "get":
                parts = [b"GET", key]
            else:
                parts = [b"HGET", b"prefs", key]
            t0 = time.perf_counter()
            reply_kind, body = conn.call(parts)
            dt = (time.perf_counter() - t0) * 1e6
            if kind == "set":
                if reply_kind != "simple" or body != b"OK":
                    raise RuntimeError(f"{kind} {key}: {reply_kind} {body!r}")
            elif kind == "hset":
                if reply_kind != "int":
                    raise RuntimeError(f"{kind} {key}: {reply_kind} {body!r}")
            elif reply_kind != "bulk" or body != value:
                got = len(body) if isinstance(body, (bytes, bytearray)) else body
                raise RuntimeError(f"{kind} {key}: {reply_kind} len={got}")
            lat.append(dt)
            i += 1
        end = time.perf_counter()
    finally:
        conn.close()
    out.put((worker, start, end, lat))


def run_phase(host, port, kind, value, conns, seconds, limit, keys):
    """`limit` and `keys` are per worker lists or ints."""
    from multiprocessing import Barrier, Process, Queue

    barrier = Barrier(conns)
    out = Queue()
    procs = []
    for c in range(conns):
        proc = Process(
            target=one_client,
            args=(c, host, port, kind, value, limit[c], keys[c], seconds, barrier, out),
        )
        proc.start()
        procs.append(proc)
    results = []
    try:
        for _ in procs:
            results.append(out.get(timeout=seconds + 120))
    except Exception:
        for proc in procs:
            proc.kill()
        raise RuntimeError(f"{kind}: a worker died before reporting")
    for proc in procs:
        proc.join()
    # perf_counter is CLOCK_MONOTONIC, one clock for every process.
    start = min(r[1] for r in results)
    end = max(r[2] for r in results)
    lats = [x for r in results for x in r[3]]
    done = [0] * conns
    for worker, _s, _e, lat in results:
        done[worker] = len(lat)
    wall = end - start
    return {
        "ops": len(lats),
        "wall_s": wall,
        "ops_s": len(lats) / wall if wall else 0,
        "p50_us": percentile(lats, 50),
        "p99_us": percentile(lats, 99),
        "done": done,
    }


def stop(proc):
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=10)


BLOBASAUR = {
    # variant: (synchronous, batch_size)
    "normal": ("NORMAL", 50),
    "normal-b1": ("NORMAL", 1),
    "full": ("FULL", 50),
    "full-b1": ("FULL", 1),
}
MPEDB = ("async", "wal", "commit")


def start_server(system, binary, data, shards, variant, log_path):
    os.makedirs(data, exist_ok=True)
    if system == "mpedb":
        cfg_path = os.path.join(data, "serve.toml")
        with open(cfg_path, "w") as f:
            f.write(
                f"""listen = "127.0.0.1:{PORT}"
data_dir = "{data}/db"
shards = {shards}
durability = "{variant}"
size_mb = 512
zstd_level = 3
max_value_mb = 16
max_connections = 64
cleanup_interval_ms = 0
cleanup_chunk = 200
"""
            )
        os.makedirs(os.path.join(data, "db"), exist_ok=True)
        cmd = [binary, "serve", "--config", cfg_path]
    else:
        sync, batch = BLOBASAUR[variant]
        cfg_path = os.path.join(data, "blob.toml")
        with open(cfg_path, "w") as f:
            f.write(
                f"""data_dir = "{data}/db"
num_shards = {shards}
addr = "127.0.0.1:{PORT}"
async_write = false
batch_size = {batch}
batch_timeout_ms = 0
cleanup_interval_secs = 86400
max_request_size_mb = 16

[storage_compression]
enabled = true
algorithm = "zstd"
level = 3

[sqlite]
cache_size_mb = 64
synchronous = "{sync}"
max_connections = 16
busy_timeout_ms = 30000
mmap_size = 0
"""
            )
        cmd = [binary, "--config", cfg_path]
    log = open(log_path, "wb")
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    try:
        wait_ping("127.0.0.1", PORT, proc, log_path)
    except Exception:
        stop(proc)
        raise
    return proc


def arm(root, rep, system, binary, variant, shards, command, payload_name, value, args):
    name = f"{system}-{variant}-s{shards}-{command}-{payload_name}"
    data = os.path.join(root, name)
    if os.path.exists(data):
        shutil.rmtree(data)
    log_path = os.path.join(root, f"{name}.r{rep}.log")
    proc = start_server(system, binary, data, shards, variant, log_path)
    try:
        # One untimed write so a first-use table create stays out of the clock.
        warm = Conn("127.0.0.1", PORT)
        if command == "set":
            warm.call([b"SET", b"warm", value])
        else:
            warm.call([b"HSET", b"prefs", b"warm", value])
        warm.close()
        write_kind = "set" if command == "set" else "hset"
        read_kind = "get" if command == "set" else "hget"
        per = [args.max_ops // args.conns] * args.conns
        wrote = run_phase(
            "127.0.0.1", PORT, write_kind, value, args.conns, args.write_secs, per, per
        )
        if min(wrote["done"]) == 0:
            raise RuntimeError(f"{name}: a worker wrote nothing")
        many = [10**9] * args.conns
        read = run_phase(
            "127.0.0.1", PORT, read_kind, value, args.conns, args.read_secs, many, wrote["done"]
        )
        print(
            f"r{rep} {name:44s} write {wrote['ops_s']:8.0f}/s p50 {wrote['p50_us']/1000:7.2f} ms"
            f"   read {read['ops_s']:8.0f}/s p50 {read['p50_us']/1000:6.2f} ms",
            flush=True,
        )
        return wrote, read
    finally:
        stop(proc)
        shutil.rmtree(data, ignore_errors=True)


def compressed_len(value, root, name):
    path = os.path.join(root, name)
    os.makedirs(root, exist_ok=True)
    with open(path, "wb") as f:
        f.write(value)
    out = subprocess.check_output(["zstd", "-3", "-c", path])
    return len(out)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mpedb", required=True)
    parser.add_argument("--blobasaur", required=True)
    parser.add_argument("--disk", required=True)
    parser.add_argument("--reps", type=int, default=3)
    parser.add_argument("--conns", type=int, default=8)
    parser.add_argument("--write-secs", type=float, default=10.0)
    parser.add_argument("--read-secs", type=float, default=5.0)
    parser.add_argument("--max-ops", type=int, default=20000)
    parser.add_argument("--shards", default="1,2")
    parser.add_argument("--only", default="", help="substring filter on the arm name")
    args = parser.parse_args()
    root = args.disk
    os.makedirs(root, exist_ok=True)
    article = payload()
    small = small_payload()
    zlen = compressed_len(article, root, "payload.json")
    slen = compressed_len(small, root, "payload-small.json")
    print(
        f"article payload {len(article)} B, zstd -3 -> {zlen} B ({len(article) / zlen:.1f}x)\n"
        f"small payload {len(small)} B, zstd -3 -> {slen} B\n"
        f"conns {args.conns}, write {args.write_secs}s or {args.max_ops} ops, read {args.read_secs}s",
        flush=True,
    )
    if zlen <= 824 or slen >= 700:
        raise SystemExit("payloads are on the wrong side of the 824-byte ring cap")
    shards = [int(s) for s in args.shards.split(",") if s]
    values = {"article": article, "small": small}
    b, m = ("blobasaur", args.blobasaur), ("mpedb", args.mpedb)
    plan = []
    for n in shards:
        # The engine comparison, alternating systems.
        for bv, mv in (("normal", "async"), ("full", "wal"), ("full-b1", "commit")):
            plan.append((*b, bv, n, "set", "article"))
            plan.append((*m, mv, n, "set", "article"))
        plan.append((*b, "normal-b1", n, "set", "article"))
        # The routing control.
        plan.append((*b, "normal", n, "hset", "article"))
        plan.append((*m, "async", n, "hset", "article"))
    # The ring control: a value that fits the group-commit slot.
    plan.append((*b, "full", 1, "set", "small"))
    plan.append((*m, "wal", 1, "set", "small"))
    plan.append((*m, "commit", 1, "set", "small"))
    plan = [p for p in plan if args.only in f"{p[0]}-{p[2]}-s{p[3]}-{p[4]}-{p[5]}"]
    tsv = os.path.join(root, "results.tsv")
    probes = os.path.join(root, "fsync-probe.tsv")
    with open(tsv, "w") as f:
        f.write(
            "rep\tsystem\tvariant\tshards\tcommand\tpayload\tphase\tops\twall_s\tops_s\tp50_us\tp99_us\n"
        )
    with open(probes, "w") as f:
        f.write("when\tp50_us\tp99_us\n")

    def probe(when):
        p50, p99 = fsync_probe(root)
        print(f"fsync probe {when}: p50 {p50/1000:.2f} ms  p99 {p99/1000:.2f} ms", flush=True)
        with open(probes, "a") as f:
            f.write(f"{when}\t{p50:.0f}\t{p99:.0f}\n")

    probe("before-r1")
    for rep in range(1, args.reps + 1):
        # Rotate, so the arm that ran first in one repetition does not in
        # the next.
        shift = (rep - 1) * (len(plan) // max(args.reps, 1))
        order = plan[shift:] + plan[:shift]
        for system, binary, variant, n, command, payload_name in order:
            wrote, read = arm(
                root, rep, system, binary, variant, n, command, payload_name,
                values[payload_name], args,
            )
            with open(tsv, "a") as f:
                for phase, row in (("write", wrote), ("read", read)):
                    f.write(
                        f"{rep}\t{system}\t{variant}\t{n}\t{command}\t{payload_name}\t{phase}\t"
                        f"{row['ops']}\t{row['wall_s']:.4f}\t{row['ops_s']:.1f}\t"
                        f"{row['p50_us']:.0f}\t{row['p99_us']:.0f}\n"
                    )
        probe(f"after-r{rep}")
    print(f"\nwrote {tsv} and {probes}", flush=True)


if __name__ == "__main__":
    main()
