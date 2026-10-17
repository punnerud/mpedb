#!/usr/bin/env python3
"""Compare MPEdb PL/SQL VARCHAR2 and literal comparison against real Oracle."""

import argparse
import importlib.metadata
import json
import os
from pathlib import Path
import random
import subprocess

import oracledb


def literal(value):
    return "'" + value.replace("'", "''") + "'"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--native-root", type=Path)
    parser.add_argument("--pairs", type=int, default=250)
    parser.add_argument("--seed", type=int, default=20261017)
    args = parser.parse_args()
    if args.pairs < 1:
        parser.error("--pairs must be positive")
    if args.native_root:
        credentials = json.loads(
            (args.native_root / "native-private/credentials.json").read_text()
        )
        user, password, dsn = (
            "SQLA_TEST",
            credentials["sqlalchemy"],
            "127.0.0.1:1521/FREEPDB1",
        )
    else:
        user, password, dsn = (
            os.environ[name]
            for name in ("ORACLE_USER", "ORACLE_PASSWORD", "ORACLE_DSN")
        )
    rng = random.Random(args.seed)
    pairs = [("a", "a "), ("a\t", "a "), ("", ""), ("ø", "ø "), ("'", "'")]
    alphabet = ["a", "A", " ", "ø", "😀", "中", "'", "\t", "\x01"]
    for _ in range(args.pairs):
        pairs.append(
            tuple("".join(rng.choices(alphabet, k=rng.randrange(12))) for _ in range(2))
        )
    cases = []
    for a, b in pairs:
        a, b = literal(a), literal(b)
        for op in ["=", "<>", "<", "<=", ">", ">="]:
            cases.append(("BOOLEAN", f"BEGIN RETURN {a} {op} {b}; END;"))
            cases.append(
                ("BOOLEAN", f"v VARCHAR2 := {a}; BEGIN RETURN v {op} {b}; END;")
            )
        cases.append(("VARCHAR2", f"BEGIN RETURN {a} || {b}; END;"))
        for unit in ["BYTE", "CHAR"]:
            limit = rng.randrange(1, 9)
            cases.append(
                ("VARCHAR2", f"v VARCHAR2({limit} {unit}) := {a}; BEGIN RETURN v; END;")
            )
    source = (
        "\n".join(f"CREATE FUNCTION f RETURN {ty} IS {body}" for ty, body in cases)
        + "\n"
    )
    process = subprocess.run(
        [str(args.probe), "--routine"],
        input=source,
        text=True,
        capture_output=True,
        check=True,
    )
    native = process.stdout.splitlines()
    if len(native) != len(cases):
        raise RuntimeError(f"probe returned {len(native)} lines for {len(cases)} cases")
    mismatches = []
    with oracledb.connect(user=user, password=password, dsn=dsn) as connection:
        cursor = connection.cursor()
        cursor.execute(
            "SELECT parameter, value FROM nls_session_parameters WHERE parameter IN ('NLS_LENGTH_SEMANTICS', 'NLS_COMP', 'NLS_SORT') UNION ALL SELECT parameter, value FROM nls_database_parameters WHERE parameter = 'NLS_CHARACTERSET'"
        )
        nls = dict(cursor.fetchall())
        expected = {
            "NLS_LENGTH_SEMANTICS": "BYTE",
            "NLS_COMP": "BINARY",
            "NLS_SORT": "BINARY",
            "NLS_CHARACTERSET": "AL32UTF8",
        }
        if nls != expected:
            raise RuntimeError(
                f"reference environment differs from supported profile: {nls}"
            )
        outputs = {
            "BOOLEAN": cursor.var(oracledb.DB_TYPE_BOOLEAN),
            "VARCHAR2": cursor.var(oracledb.DB_TYPE_VARCHAR, size=32767),
        }
        for (ty, body), actual in zip(cases, native):
            output = outputs[ty]
            error = None
            try:
                cursor.execute(
                    f"DECLARE FUNCTION f RETURN {ty} IS {body} BEGIN :out := f; END;",
                    out=output,
                )
                value = output.getvalue()
                oracle = (
                    "NULL"
                    if value is None
                    else (
                        f"BOOL\t{str(value).lower()}"
                        if ty == "BOOLEAN"
                        else "TEXT\t" + value.encode("utf-8").hex()
                    )
                )
                equal = actual == oracle
            except oracledb.Error as exc:
                error = exc.args[0].code
                oracle = str(exc)
                equal = (
                    error == 6502
                    and actual.startswith("ERR\t")
                    and "VARCHAR2" in actual
                    and ("buffer too small" in actual or "exceeds 32767" in actual)
                )
            if not equal:
                mismatches.append(
                    {
                        "type": ty,
                        "body": body,
                        "oracle": oracle,
                        "oracle_error": error,
                        "native": actual,
                    }
                )
        report = {
            "oracle_version": connection.version,
            "oracledb_version": importlib.metadata.version("oracledb"),
            "nls": nls,
            "seed": args.seed,
            "cases": len(cases),
            "mismatches": mismatches,
        }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Cases: {len(cases)}, mismatches: {len(mismatches)}; report: {args.output}")
    raise SystemExit(bool(mismatches))


if __name__ == "__main__":
    main()
