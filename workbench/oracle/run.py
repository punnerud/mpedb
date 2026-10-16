#!/usr/bin/env python3
"""Run unmodified upstream Oracle tests; no MPEdb adapter or Oracle source."""

import argparse
import importlib.metadata
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("suite", choices=("django", "sqlalchemy"))
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--timeout", type=int, default=3600)
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("tests", nargs="*", help="Omit to run the full upstream suite")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    source = args.source.resolve()
    entry = source / ("tests/runtests.py" if args.suite == "django" else "test")
    if not entry.exists():
        parser.error(f"upstream test entry not found: {entry}")
    args.artifacts.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix=f"{args.suite}-", dir=args.artifacts))
    output.chmod(0o700)
    revision = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=source,
        text=True,
        capture_output=True,
        check=True,
    ).stdout.strip()
    metadata = {
        "suite": args.suite,
        "source": str(source),
        "revision": revision,
        "tests": args.tests or ["ALL"],
        "adaptations": [],
        "timeout_seconds": args.timeout,
        "status": "prepared",
        "versions": {
            name: importlib.metadata.version(name) for name in ("oracledb", args.suite)
        },
    }

    def save():
        (output / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")

    save()
    print(f"Artifacts: {output}", flush=True)
    if args.prepare_only:
        return 0
    required = ["ORACLE_DSN", "ORACLE_USER", "ORACLE_PASSWORD"]
    if args.suite == "django":
        required.append("ORACLE_TEST_PASSWORD")
    missing = [name for name in required if not os.environ.get(name)]
    if missing:
        metadata.update(status="setup_error", missing_environment=missing)
        save()
        print("Missing environment: " + ", ".join(missing), file=sys.stderr)
        return 2

    # Verify an actual Oracle connection before collecting tests. Do not record
    # the DSN, password, or arbitrary exception text in metadata.
    import oracledb

    try:
        with oracledb.connect(
            user=os.environ["ORACLE_USER"],
            password=os.environ["ORACLE_PASSWORD"],
            dsn=os.environ["ORACLE_DSN"],
            tcp_connect_timeout=10,
        ) as connection:
            metadata["oracle_version"] = connection.version
            metadata["thin_driver"] = connection.thin
    except oracledb.Error as error:
        detail = error.args[0]
        metadata.update(
            status="setup_error",
            error_code=getattr(detail, "full_code", type(error).__name__),
        )
        save()
        print(f"Oracle connection failed: {metadata['error_code']}", file=sys.stderr)
        return 2

    env = os.environ.copy()
    if args.suite == "django":
        env["PYTHONPATH"] = os.pathsep.join(
            [
                str(Path(__file__).resolve().parent),
                str(source),
                env.get("PYTHONPATH", ""),
            ]
        )
        command = [
            sys.executable,
            str(entry),
            "--settings=django_settings",
            "--parallel=1",
            "--noinput",
            *args.tests,
        ]
    else:
        # Build the URL inside the child; credentials never enter its OS argv.
        launcher = """
import os, sys, pytest
from sqlalchemy.engine import URL
url = URL.create('oracle+oracledb', username=os.environ['ORACLE_USER'],
    password=os.environ['ORACLE_PASSWORD'],
    query={'dsn': os.environ['ORACLE_DSN']})
raise SystemExit(pytest.main(['--dburi', url.render_as_string(hide_password=False),
    '--junitxml=' + os.environ['ORACLE_JUNIT'], *sys.argv[1:]]))
"""
        env["ORACLE_JUNIT"] = str(output / "junit.xml")
        command = [sys.executable, "-c", launcher, *(args.tests or ["test"])]
    metadata["status"] = "running"
    save()
    with (output / "suite.log").open("w") as log:
        process = subprocess.Popen(
            command,
            cwd=source,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        try:
            code = process.wait(timeout=args.timeout)
            metadata.update(status="finished", exit_code=code)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            metadata.update(status="incomplete", reason="timeout_or_interrupt")
            code = 124
    save()
    print(f"Status: {metadata['status']}; exit code: {code}")
    return code


if __name__ == "__main__":
    raise SystemExit(main())
