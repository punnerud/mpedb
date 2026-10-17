#!/usr/bin/env python3
"""Compare native PL/SQL NUMBER arithmetic with an actual Oracle server."""

import argparse
import decimal
import importlib.metadata
import json
import os
from pathlib import Path
import random
import subprocess

import oracledb


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--native-root", type=Path)
    parser.add_argument("--pairs", type=int, default=500)
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
    fixture = json.loads(
        (Path(__file__).parent / "reference/number-boundaries.json").read_text()
    )
    expressions = [row["expression"] for row in fixture["results"]]

    def literal():
        if rng.randrange(20) == 0:
            return "0"
        digits = str(rng.randrange(1, 10)) + "".join(
            str(rng.randrange(10)) for _ in range(rng.randrange(1, 44))
        )
        # Value exponent is -130..125. Exclude out-of-range source literals
        # here: their SQL/PLSQL parsing error codes are a separate concern.
        return (
            ("-" if rng.randrange(2) else "")
            + "0."
            + digits
            + "e"
            + str(rng.randrange(-129, 127))
        )

    for _ in range(args.pairs):
        a, b = literal(), literal()
        expressions.extend(f"({a}) {op} ({b})" for op in ("+", "-", "*", "/"))
    process = subprocess.run(
        [str(args.probe.resolve())],
        input="\n".join(expressions) + "\n",
        text=True,
        capture_output=True,
        check=True,
    )
    native = process.stdout.splitlines()
    if len(native) != len(expressions):
        raise RuntimeError("native probe returned an unexpected result count")
    oracledb.defaults.fetch_decimals = True
    mismatches = []
    with oracledb.connect(
        user=user, password=password, dsn=dsn, tcp_connect_timeout=10
    ) as connection:
        with connection.cursor() as cursor:
            for expression, actual in zip(expressions, native):
                try:
                    cursor.execute("SELECT " + expression + " FROM dual")
                    value = cursor.fetchone()[0]
                    expected = "NULL" if value is None else "OK\t" + str(value)
                except oracledb.Error as error:
                    expected = "ERR\t" + error.args[0].full_code
                equal = actual == expected
                if actual.startswith("OK\t") and expected.startswith("OK\t"):
                    equal = decimal.Decimal(actual[3:]) == decimal.Decimal(expected[3:])
                elif actual.startswith("ERR\t") and expected.startswith("ERR\t"):
                    equal = (
                        "ArithmeticOverflow" in actual and expected == "ERR\tORA-01426"
                    ) or ("DivisionByZero" in actual and expected == "ERR\tORA-01476")
                if not equal:
                    mismatches.append(
                        {"expression": expression, "oracle": expected, "mpedb": actual}
                    )
        result = {
            "oracle_version": connection.version,
            "driver": importlib.metadata.version("oracledb"),
            "seed": args.seed,
            "pairs": args.pairs,
            "cases": len(expressions),
            "scope": "arithmetic with valid finite literals plus measured boundary corpus",
            "mismatches": mismatches,
        }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(
        f"Cases: {len(expressions)}; mismatches: {len(mismatches)}; report: {args.output}"
    )
    return int(bool(mismatches))


if __name__ == "__main__":
    raise SystemExit(main())
