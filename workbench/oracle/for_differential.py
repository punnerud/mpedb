#!/usr/bin/env python3
"""Compare native PL/SQL numeric FOR and PLS_INTEGER execution with Oracle."""

import argparse
from decimal import Decimal
import importlib.metadata
import json
import os
from pathlib import Path
import random
import subprocess

import oracledb


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--native-root", type=Path)
    parser.add_argument("--samples", type=int, default=250)
    parser.add_argument("--seed", type=int, default=20261017)
    args = parser.parse_args()
    if args.samples < 1:
        parser.error("--samples must be positive")
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
    bodies = []
    for reverse, bound in [("", "2147483647"), ("REVERSE ", "-2147483648")]:
        bodies.append(
            f"n NUMBER := 0; BEGIN FOR i IN {reverse}{bound}..{bound} LOOP n := i; CONTINUE; END LOOP; RETURN n; END;"
        )
    bodies += [
        "i NUMBER := 3; BEGIN FOR i IN i..i+2 LOOP NULL; END LOOP; RETURN i; END;",
        "BEGIN FOR i IN NULL..1 LOOP NULL; END LOOP; RETURN 0; END;",
        "BEGIN FOR i IN 1..NULL LOOP NULL; END LOOP; RETURN 0; END;",
        "BEGIN FOR i IN 2147483648..1 LOOP NULL; END LOOP; RETURN 0; END;",
        "BEGIN RETURN 50000 * 50000; END;",
        "v PLS_INTEGER := -2147483648; BEGIN RETURN - -v; END;",
        "BEGIN FOR i IN 2147483647..2147483647 LOOP RETURN i + 1.0; END LOOP; END;",
        "lo NUMBER := 1; hi NUMBER := 3; n NUMBER := 0; BEGIN FOR i IN lo..hi LOOP lo := 10; hi := 0; n := n*10+i; END LOOP; RETURN n; END;",
    ]
    values = [
        "-2147483648",
        "2147483647",
        "2147483648",
        "-2147483649",
        "-50000",
        "50000",
        "-2",
        "0",
        "3",
        "1.5",
        "-1.5",
        "2147483647.4",
        "2147483647.5",
        "NULL",
    ]
    for _ in range(args.samples):
        lower, upper = (str(Decimal(rng.randrange(-200, 201)) / 10) for _ in range(2))
        reverse = rng.choice(["", "REVERSE "])
        bodies.append(
            f"n NUMBER := 0; BEGIN FOR i IN {reverse}{lower}..{upper} LOOP n := n+i; END LOOP; RETURN n; END;"
        )
        skip, stop = rng.randrange(-20, 21), rng.randrange(-20, 21)
        bodies.append(
            f"n NUMBER := 0; BEGIN FOR i IN {reverse}{lower}..{upper} LOOP CONTINUE WHEN i={skip}; n := n+i; EXIT WHEN i={stop}; END LOOP; RETURN n; END;"
        )
        a, b = rng.choices(values, k=2)
        for op in ["+", "-", "*", "/"]:
            bodies.append(
                f"a PLS_INTEGER := {a}; b BINARY_INTEGER := {b}; BEGIN RETURN a {op} b; END;"
            )
        bodies.append(f"a PLS_INTEGER := {a}; BEGIN RETURN -a; END;")
    routines = (
        "\n".join("CREATE FUNCTION f RETURN NUMBER IS " + body for body in bodies)
        + "\n"
    )
    process = subprocess.run(
        [str(args.probe.resolve()), "--routine"],
        input=routines,
        text=True,
        capture_output=True,
        check=True,
    )
    native = process.stdout.splitlines()
    if len(native) != len(bodies):
        raise RuntimeError("probe returned an unexpected result count")
    mismatches = []
    with oracledb.connect(user=user, password=password, dsn=dsn) as connection:
        cursor = connection.cursor()
        output = cursor.var(oracledb.DB_TYPE_VARCHAR, size=32767)
        for body, actual in zip(bodies, native):
            code = None
            try:
                cursor.execute(
                    "DECLARE FUNCTION f RETURN NUMBER IS "
                    + body
                    + " BEGIN :out := TO_CHAR(f, 'TM9', 'NLS_NUMERIC_CHARACTERS=''.,'''); END;",
                    out=output,
                )
                value = output.getvalue()
                expected = "NULL" if value is None else "OK\t" + value
                equal = actual == expected
                if actual.startswith("OK\t") and expected.startswith("OK\t"):
                    equal = Decimal(actual[3:]) == Decimal(expected[3:])
            except oracledb.Error as error:
                code = error.args[0].code
                expected = str(error)
                equal = actual.startswith("ERR\t") and (
                    (code == 1426 and "ArithmeticOverflow" in actual)
                    or (code == 1476 and "DivisionByZero" in actual)
                    or (code == 6502 and "cannot be NULL" in actual)
                )
            if not equal:
                mismatches.append(
                    {
                        "body": body,
                        "oracle": expected,
                        "oracle_code": code,
                        "native": actual,
                    }
                )
        report = {
            "oracle_version": connection.version,
            "oracledb_version": importlib.metadata.version("oracledb"),
            "seed": args.seed,
            "cases": len(bodies),
            "mismatches": mismatches,
        }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"Cases: {len(bodies)}, mismatches: {len(mismatches)}; report: {args.output}")
    raise SystemExit(bool(mismatches))


if __name__ == "__main__":
    main()
