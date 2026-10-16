#!/usr/bin/env python3
"""Run Django's unmodified suite with its standard PostgreSQL backend.

Use a Python environment containing an editable Django checkout and its test
requirements. No labels means the full suite. Preserve setup failures as failures.
"""

import argparse
from contextlib import ExitStack
import json
import os
import re
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--django", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=3600)
    parser.add_argument(
        "--test-timeout",
        type=float,
        default=0,
        help="optional per-test deadline in seconds (Unix, errors on timeout)",
    )
    parser.add_argument(
        "--debug-sql",
        action="store_true",
        help="include Django's SQL trace for failed tests",
    )
    parser.add_argument(
        "--provisioned",
        action="store_true",
        help="provision separate test files instead of issuing CREATE DATABASE",
    )
    parser.add_argument("labels", nargs="*")
    parser.add_argument(
        "--portable-sql",
        action="store_true",
        help="use VALUES bulk inserts and omit extra LIKE optimization indexes (requires --provisioned)",
    )
    args = parser.parse_args()
    if args.portable_sql and not args.provisioned:
        parser.error("--portable-sql requires --provisioned")
    if args.test_timeout < 0:
        parser.error("--test-timeout must be nonnegative")
    checkout = args.django.resolve(strict=True)
    binary = args.binary.resolve(strict=True)
    args.work_dir.mkdir(parents=True, exist_ok=True)
    run = Path(tempfile.mkdtemp(prefix="django-", dir=args.work_dir.resolve()))
    revision = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=checkout, text=True
    ).strip()
    metadata = {
        "django_revision": revision,
        "binary": str(binary),
        "labels": args.labels,
        "backend": (
            "django_backend" if args.provisioned else "django.db.backends.postgresql"
        ),
        "adaptations": ["external database lifecycle"] if args.provisioned else [],
        "test_timeout": args.test_timeout,
    }
    if args.portable_sql:
        metadata["adaptations"].extend(
            [
                "omit redundant LIKE optimization indexes",
                "VALUES bulk inserts instead of UNNEST",
                "fixture sequence repair unnecessary for owned rowid generators",
                "DELETE-based flush with explicit FK cascade closure",
                "declare unsupported row locks, sequence reset, transactional DDL, DISTINCT ON and covering indexes",
                "full-column GROUP BY instead of primary-key functional dependency",
                "disable server-side cursors",
                "declare unsupported regex backreferences",
            ]
        )
    print(f"Artifacts: {run}", flush=True)
    print(
        f"Backend: {metadata['backend']}; adaptations: {metadata['adaptations']}",
        flush=True,
    )
    here = Path(__file__).resolve().parent
    with ExitStack() as stack:
        servers = []
        databases = {}
        for alias in ("default", "other"):
            sock = stack.enter_context(tempfile.TemporaryDirectory(prefix="dj-pg-"))
            config = run / f"{alias}.toml"
            config.write_text(
                "[database]\npath = "
                + json.dumps(str(run / f"{alias}.mpedb"))
                + '\nsize_mb = 512\ndurability = "commit"\n'
                + '[compat]\ndialect = "postgres"\n'
            )
            name = f"test_django_{alias}"
            databases[alias] = {
                "ENGINE": metadata["backend"],
                "NAME": name,
                "USER": "morten",
                "HOST": sock,
                "PORT": "5432",
                "OPTIONS": {"connect_timeout": 10},
                "TEST": {"NAME": name},
            }
            if args.portable_sql:
                databases[alias]["DISABLE_SERVER_SIDE_CURSORS"] = True
            server_log = stack.enter_context((run / f"{alias}-server.log").open("w"))
            server = subprocess.Popen(
                [
                    sys.executable,
                    str(here / "forkserve.py"),
                    sock,
                    str(binary),
                    str(config),
                ],
                stdout=server_log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            servers.append((server, sock))
            stack.callback(stop_server, server)
        (run / "mpedb_django_settings.py").write_text(
            "TEST_RUNNER = "
            + repr(
                "django_backend.runner.TimedDiscoverRunner"
                if args.test_timeout
                else "django.test.runner.DiscoverRunner"
            )
            + "\n"
            + 'SECRET_KEY = "django_tests_secret_key"\n'
            'PASSWORD_HASHERS = ["django.contrib.auth.hashers.MD5PasswordHasher"]\n'
            'DEFAULT_AUTO_FIELD = "django.db.models.AutoField"\n'
            'USE_TZ = False\nTIME_ZONE = "UTC"\nDATABASES = ' + repr(databases) + "\n"
        )
        for server, sock in servers:
            deadline = time.monotonic() + 10
            while not (Path(sock) / ".s.PGSQL.5432").exists():
                if server.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Server failed to start; see *-server.log")
                time.sleep(0.05)
        command = [
            sys.executable,
            "runtests.py",
            "--settings=mpedb_django_settings",
            "--parallel=1",
            "--noinput",
            "-v1",
            *args.labels,
        ]
        metadata["command"] = command
        if args.debug_sql:
            command.append("--debug-sql")
        (run / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
        with (run / "suite.log").open("w") as log:
            suite = subprocess.Popen(
                command,
                cwd=checkout / "tests",
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
                env={
                    **os.environ,
                    "MPEDB_DJANGO_PORTABLE_SQL": "1" if args.portable_sql else "0",
                    "MPEDB_DJANGO_TEST_TIMEOUT": str(args.test_timeout),
                    "PYTHONPATH": os.pathsep.join((str(run), str(checkout), str(here))),
                    "TMPDIR": str(run),
                },
            )
            try:
                code = suite.wait(timeout=args.timeout)
            except subprocess.TimeoutExpired:
                os.killpg(suite.pid, signal.SIGTERM)
                suite.wait(timeout=10)
                code = 124
    metadata["exit_code"] = code
    log_text = (run / "suite.log").read_text()
    metadata["reached_test_summary"] = any(
        line.startswith("Ran ") for line in log_text.splitlines()
    )
    found = re.search(r"^Found (\d+) test\(s\)\.", log_text, re.MULTILINE)
    ran = re.search(r"^Ran (\d+) tests? in", log_text, re.MULTILINE)
    metadata["discovered"] = int(found[1]) if found else None
    metadata["ran"] = int(ran[1]) if ran else 0
    metadata["failed_tests"] = sorted(
        set(re.findall(r"^(?:ERROR|FAIL): [^(]+ \(([^)]+)\)", log_text, re.MULTILINE))
    )
    summary = re.findall(r"^(?:OK|FAILED)(?: \([^\n]+\))?$", log_text, re.MULTILINE)
    metadata["summary"] = summary[-1] if summary else None
    (run / "failed_test_ids.txt").write_text(
        "".join(name + "\n" for name in metadata["failed_tests"])
    )
    (run / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print("\n".join(log_text.splitlines()[-25:]))
    print(f"Exit: {code}; test summary reached: {metadata['reached_test_summary']}")
    return code


def stop_server(server):
    try:
        os.killpg(server.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    server.wait(timeout=10)


if __name__ == "__main__":
    sys.exit(main())
