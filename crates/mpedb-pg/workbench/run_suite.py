#!/usr/bin/env python3
"""Run the stock SQLAlchemy suite against a fresh mpedb database.

Execute with the Python from the workbench venv. Each run retains its database,
server log, pytest log and JUnit report in a unique directory below --work-dir.
Use a real filesystem with enough space, rather than /dev/shm.
"""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlencode
import xml.etree.ElementTree as ET


def results(path):
    return {
        (case.get("classname"), case.get("name")): next(
            (state for state in ("failure", "error", "skipped")
             if case.find(state) is not None), "passed"
        )
        for case in ET.parse(path).iter("testcase")
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--compare", type=Path, help="previous JUnit report")
    parser.add_argument("--stock-suite", action="store_true",
                        help="run the generic suite without PostgreSQL expectation adaptations")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    args.work_dir.mkdir(parents=True, exist_ok=True)
    run_dir = Path(tempfile.mkdtemp(prefix="satest-", dir=args.work_dir.resolve()))
    (run_dir / "run.json").write_text(json.dumps({
        "binary": str(binary),
        "suite": "stock" if args.stock_suite else "postgresql-expectations",
        "adapted_tests": [] if args.stock_suite else [
            "DistinctOnTest.test_distinct_on",
            "ComponentReflectionTest.test_get_temp_table_indexes",
        ],
        "additional_tests": [] if args.stock_suite else ["EmulatedEnumTest"],
    }, indent=2) + "\n")
    # Keep the socket path short even if the artifact directory is deep.
    with tempfile.TemporaryDirectory(prefix="mpedb-sock-") as sock_dir:
        config = run_dir / "db.toml"
        config.write_text(
            "[database]\npath = " + json.dumps(str(run_dir / "app.mpedb"))
            + '\nsize_mb = 128\ndurability = "commit"\n'
            + '[compat]\ndialect = "postgres"\n'
        )
        here = Path(__file__).resolve().parent
        report = run_dir / "results.xml"
        print(f"Artifacts: {run_dir}", flush=True)
        print("Suite: " + ("stock" if args.stock_suite else "PostgreSQL expectations (2 adapted tests)"), flush=True)
        with (run_dir / "server.log").open("w") as server_log:
            server = subprocess.Popen(
                [sys.executable, str(here / "forkserve.py"), sock_dir,
                 str(binary), str(config)],
                stdout=server_log, stderr=subprocess.STDOUT, start_new_session=True,
            )
            try:
                deadline = time.monotonic() + 10
                while not (Path(sock_dir) / ".s.PGSQL.5432").exists():
                    if server.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(f"Server did not start; see {run_dir / 'server.log'}")
                    time.sleep(0.05)
                url = "postgresql+psycopg2://morten@/app?" + urlencode({"host": sock_dir})
                with (run_dir / "pytest.log").open("w") as log:
                    completed = subprocess.run(
                        [sys.executable, "-m", "pytest", "-q", "--dburi", url,
                         "--timeout=20", "--timeout-method=thread",
                         f"--junitxml={report}"],
                        cwd=here / "satest", stdout=log, stderr=subprocess.STDOUT,
                        env={**os.environ, "MPEDB_STOCK_SUITE": "1" if args.stock_suite else "0"},
                    )
            finally:
                # Also stop any per-connection children left by a timed-out test.
                try:
                    os.killpg(server.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                server.wait(timeout=10)
        if report.exists():
            current = results(report)
            print(dict(Counter(current.values())))
            if args.compare:
                previous = results(args.compare)
                for label, names in (
                    ("Fixed", [k for k, v in current.items()
                               if v == "passed" and previous.get(k) in ("failure", "error")]),
                    ("Regressed", [k for k, v in previous.items()
                                   if v == "passed" and current.get(k) != "passed"]),
                ):
                    print(f"{label}: {len(names)}")
                    for cls, name in sorted(names):
                        print(f"  {cls}::{name}")
        return completed.returncode


if __name__ == "__main__":
    sys.exit(main())
