#!/usr/bin/env python3
"""mpedb-redis against Redis, on the blob shape of ../blobasaur/run.py.

The client, the payloads and the phase runner are that file's. This one
adds a Redis arm, the resident memory and the bytes on disk after each
arm, and a second section driven by `redis-benchmark`, a C client, because
the Python client is slower than both servers on reads.

Variants, named for what the server does before it replies.

* redis memory        no persistence. A restart loses everything.
* redis aof-no        `appendonly yes`, `appendfsync no`. The command is
                      written to the log, not synced.
* redis aof-everysec  the same, synced once a second in the background.
* redis aof-always    synced before the reply.
* mpedb none          a mapped file, no log, no sync.
* mpedb async         a log record, no sync before the reply.
* mpedb wal           one log fdatasync before the reply.
* mpedb commit        data and meta flushed before the reply.
* mpedb nocache       `none` with `cache_mb = 0`. The control arm for the
                      string cache.
* mpedb behind        `none` with `write_behind_ms = 1000`. A plain SET is
                      answered from memory and written within a second. A
                      killed process loses that second.

Pairs: memory with none, aof-no and aof-everysec with async, aof-always
with wal and with commit. mpedb is never the weaker side: `none` survives
a killed process and `memory` does not. `behind` is paired with `memory`
only: it loses a second to a killed process, where `memory` loses
everything and every `aof` variant loses nothing.

    python3 -B run.py --mpedb <mpedb-redis> --redis <redis-server> \\
        --redis-benchmark <redis-benchmark> --disk <dir on the disk to test>
"""

import argparse
import importlib.util
import os
import shutil
import subprocess

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location(
    "blob", os.path.join(HERE, "..", "blobasaur", "run.py")
)
blob = importlib.util.module_from_spec(spec)
spec.loader.exec_module(blob)
PORT = blob.PORT
KEYS = 100_000
UNIQUE = 100_000_000

REDIS = {
    "memory": ("no", "no"),
    "aof-no": ("yes", "no"),
    "aof-everysec": ("yes", "everysec"),
    "aof-always": ("yes", "always"),
}

MPEDB = {
    "none": ("none", ""),
    "async": ("async", ""),
    "wal": ("wal", ""),
    "commit": ("commit", ""),
    "nocache": ("none", "cache_mb = 0"),
    "behind": ("none", "write_behind_ms = 1000"),
}


def start(system, binary, data, variant, log_path):
    os.makedirs(os.path.join(data, "db"), exist_ok=True)
    if system == "mpedb":
        durability, extra = MPEDB[variant]
        cfg_path = os.path.join(data, "serve.toml")
        with open(cfg_path, "w") as f:
            f.write(
                f"""listen = "127.0.0.1:{PORT}"
data_dir = "{data}/db"
shards = 1
durability = "{durability}"
size_mb = 512
max_connections = 64
cleanup_interval_ms = 0
{extra}
"""
            )
        return launch([binary, "serve", "--config", cfg_path], log_path)
    aof, fsync = REDIS[variant]
    cfg_path = os.path.join(data, "redis.conf")
    with open(cfg_path, "w") as f:
        f.write(
            f"""bind 127.0.0.1
port {PORT}
dir {data}/db
save ""
appendonly {aof}
appendfsync {fsync}
auto-aof-rewrite-percentage 0
daemonize no
"""
        )
    return launch([binary, cfg_path], log_path)


def launch(cmd, log_path):
    log = open(log_path, "wb")
    proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
    try:
        blob.wait_ping("127.0.0.1", PORT, proc, log_path)
    except Exception:
        blob.stop(proc)
        raise
    return proc


def rss_kb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    return 0


def disk_kb(path):
    out = subprocess.check_output(["du", "-sk", path])
    return int(out.split()[0])


def fresh(root, name):
    data = os.path.join(root, name)
    if os.path.exists(data):
        shutil.rmtree(data)
    return data


def arm(root, rep, system, binary, variant, payload_name, value, args):
    name = f"{system}-{variant}-{payload_name}"
    data = fresh(root, name)
    proc = start(system, binary, data, variant, os.path.join(root, f"{name}.r{rep}.log"))
    try:
        warm = blob.Conn("127.0.0.1", PORT)
        warm.call([b"SET", b"warm", value])
        warm.close()
        per = [args.max_ops // args.conns] * args.conns
        wrote = blob.run_phase(
            "127.0.0.1", PORT, "set", value, args.conns, args.write_secs, per, per
        )
        if min(wrote["done"]) == 0:
            raise RuntimeError(f"{name}: a worker wrote nothing")
        many = [10**9] * args.conns
        read = blob.run_phase(
            "127.0.0.1", PORT, "get", value, args.conns, args.read_secs, many, wrote["done"]
        )
        rss, disk = rss_kb(proc.pid), disk_kb(data)
        print(
            f"r{rep} {name:30s} write {wrote['ops_s']:8.0f}/s p50 {wrote['p50_us']/1000:7.2f} ms"
            f"   read {read['ops_s']:8.0f}/s p50 {read['p50_us']/1000:6.2f} ms"
            f"   keys {wrote['ops']:6d} rss {rss/1024:6.0f} MiB disk {disk/1024:6.0f} MiB",
            flush=True,
        )
        return wrote, read, rss, disk
    finally:
        blob.stop(proc)
        shutil.rmtree(data, ignore_errors=True)


def c_client(root, rep, system, binary, variant, bench, pipeline, size, keys, args):
    """SET then GET with redis-benchmark. Returns {test: (rps, p50_ms)}."""
    name = f"c-{system}-{variant}-P{pipeline}-k{keys}"
    data = fresh(root, name)
    proc = start(system, binary, data, variant, os.path.join(root, f"{name}.r{rep}.log"))
    try:
        n = args.c_requests * (4 if pipeline > 1 else 1)
        if variant in ("aof-always", "wal", "commit"):
            n = max(n // 10, 2000)
        out = subprocess.check_output(
            [
                bench, "-h", "127.0.0.1", "-p", str(PORT), "-c", str(args.conns),
                "-n", str(n), "-d", str(size), "-r", str(keys), "-P", str(pipeline),
                "-t", "set,get", "--csv",
            ],
            stderr=subprocess.DEVNULL,
        ).decode()
        got = {}
        for line in out.splitlines():
            cells = [c.strip('"') for c in line.split(",")]
            if cells[0] in ("SET", "GET"):
                got[cells[0]] = (float(cells[1]), float(cells[4]))
        if set(got) != {"SET", "GET"}:
            raise RuntimeError(f"{name}: redis-benchmark printed\n{out}")
        print(
            f"r{rep} {name:30s} SET {got['SET'][0]:9.0f}/s p50 {got['SET'][1]:6.3f} ms"
            f"   GET {got['GET'][0]:9.0f}/s p50 {got['GET'][1]:6.3f} ms",
            flush=True,
        )
        return got
    finally:
        blob.stop(proc)
        shutil.rmtree(data, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mpedb", required=True)
    parser.add_argument("--redis", required=True)
    parser.add_argument("--redis-benchmark", required=True)
    parser.add_argument("--disk", required=True)
    parser.add_argument("--reps", type=int, default=3)
    parser.add_argument("--conns", type=int, default=8)
    parser.add_argument("--write-secs", type=float, default=10.0)
    parser.add_argument("--read-secs", type=float, default=5.0)
    # Redis keeps the 64 KiB value uncompressed in memory. 6 000 of them is
    # 375 MiB, which this harness's 3.7 GiB box can hold without swapping.
    parser.add_argument("--max-ops", type=int, default=6000)
    parser.add_argument("--c-requests", type=int, default=100000)
    parser.add_argument("--only", default="")
    args = parser.parse_args()
    root = args.disk
    os.makedirs(root, exist_ok=True)
    values = {"article": blob.payload(), "small": blob.small_payload()}
    r, m = ("redis", args.redis), ("mpedb", args.mpedb)
    plan = []
    for rv, mv in (("memory", "none"), ("aof-no", "async"), ("aof-always", "wal")):
        plan.append((*r, rv, "article"))
        plan.append((*m, mv, "article"))
    plan.append((*r, "aof-everysec", "article"))
    plan.append((*m, "commit", "article"))
    plan.append((*m, "behind", "article"))
    for rv, mv in (("aof-no", "async"), ("aof-always", "wal")):
        plan.append((*r, rv, "small"))
        plan.append((*m, mv, "small"))
    plan = [p for p in plan if args.only in f"{p[0]}-{p[2]}-{p[3]}"]
    cplan = []
    for pipeline in (1, 16):
        for rv, mv in (("memory", "none"), ("aof-no", "async"), ("aof-always", "wal")):
            cplan.append((*r, rv, pipeline, 64, KEYS))
            cplan.append((*m, mv, pipeline, 64, KEYS))
        cplan.append((*m, "nocache", pipeline, 64, KEYS))
        cplan.append((*m, "behind", pipeline, 64, KEYS))
    # Keys that do not repeat, so a held SET is never replaced before it is
    # written.
    cplan.append((*r, "memory", 16, 64, UNIQUE))
    cplan.append((*m, "none", 16, 64, UNIQUE))
    cplan.append((*m, "behind", 16, 64, UNIQUE))
    cplan = [p for p in cplan if args.only in f"c-{p[0]}-{p[2]}-P{p[3]}"]

    tsv = os.path.join(root, "results.tsv")
    ctsv = os.path.join(root, "c-client.tsv")
    probes = os.path.join(root, "fsync-probe.tsv")
    with open(tsv, "w") as f:
        f.write(
            "rep\tsystem\tvariant\tpayload\tphase\tops\twall_s\tops_s\tp50_us\tp99_us\trss_kb\tdisk_kb\n"
        )
    with open(ctsv, "w") as f:
        f.write("rep\tsystem\tvariant\tpipeline\tvalue_bytes\tkeys\ttest\trps\tp50_ms\n")
    with open(probes, "w") as f:
        f.write("when\tp50_us\tp99_us\n")

    def probe(when):
        p50, p99 = blob.fsync_probe(root)
        print(f"fsync probe {when}: p50 {p50/1000:.2f} ms  p99 {p99/1000:.2f} ms", flush=True)
        with open(probes, "a") as f:
            f.write(f"{when}\t{p50:.0f}\t{p99:.0f}\n")

    probe("before-r1")
    for rep in range(1, args.reps + 1):
        shift = (rep - 1) * (len(plan) // max(args.reps, 1))
        for system, binary, variant, payload_name in plan[shift:] + plan[:shift]:
            wrote, read, rss, disk = arm(
                root, rep, system, binary, variant, payload_name, values[payload_name], args
            )
            with open(tsv, "a") as f:
                for phase, row in (("write", wrote), ("read", read)):
                    f.write(
                        f"{rep}\t{system}\t{variant}\t{payload_name}\t{phase}\t"
                        f"{row['ops']}\t{row['wall_s']:.4f}\t{row['ops_s']:.1f}\t"
                        f"{row['p50_us']:.0f}\t{row['p99_us']:.0f}\t{rss}\t{disk}\n"
                    )
        shift = (rep - 1) * (len(cplan) // max(args.reps, 1))
        for system, binary, variant, pipeline, size, keys in cplan[shift:] + cplan[:shift]:
            got = c_client(
                root, rep, system, binary, variant, args.redis_benchmark, pipeline, size, keys, args
            )
            with open(ctsv, "a") as f:
                for test, (rps, p50) in got.items():
                    f.write(
                        f"{rep}\t{system}\t{variant}\t{pipeline}\t{size}\t{keys}\t{test}\t{rps:.1f}\t{p50:.3f}\n"
                    )
        probe(f"after-r{rep}")
    print(f"\nwrote {tsv}, {ctsv} and {probes}", flush=True)


if __name__ == "__main__":
    main()
