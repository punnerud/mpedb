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
    parser.add_argument(
        "--runtime-specials",
        action="store_true",
        help="compare the measured runtime special-value matrix",
    )
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
    bodies = [f"BEGIN RETURN {expression}; END;" for expression in expressions]
    if args.runtime_specials:
        matrix = json.loads(
            (Path(__file__).parent / "reference/plsql-number-specials.json").read_text()
        )
        expressions = [row["expression"] for row in matrix["results"]] + ["-p", "-m"]
        bodies = [
            f"a NUMBER := 1e125; p NUMBER; m NUMBER; BEGIN p := a*a; m := -p; RETURN {expression}; END;"
            for expression in expressions
        ]
        for condition in [
            "p = p",
            "p = m",
            "p > 1e125",
            "m < -1e125",
            "p < m",
            "p IS NULL",
        ]:
            expressions.append(condition)
            bodies.append(
                f"a NUMBER := 1e125; p NUMBER; m NUMBER; BEGIN p := a*a; m := -p; IF {condition} THEN RETURN 1; ELSE RETURN 0; END IF; END;"
            )
        for statement in ["i := p;", "FOR j IN p..p LOOP NULL; END LOOP;"]:
            expressions.append(statement)
            bodies.append(
                f"a NUMBER := 1e125; p NUMBER; i PLS_INTEGER; BEGIN p := a*a; {statement} RETURN 0; END;"
            )
    process = subprocess.run(
        [str(args.probe.resolve()), "--routine"],
        input="\n".join(f"CREATE FUNCTION f RETURN NUMBER IS {body}" for body in bodies)
        + "\n",
        text=True,
        capture_output=True,
        check=True,
    )
    native = process.stdout.splitlines()
    if len(native) != len(expressions):
        raise RuntimeError("native probe returned an unexpected result count")
    mismatches = []
    with oracledb.connect(
        user=user, password=password, dsn=dsn, tcp_connect_timeout=10
    ) as connection:
        with connection.cursor() as cursor:
            output = cursor.var(oracledb.DB_TYPE_VARCHAR, size=32767)
            image = cursor.var(oracledb.DB_TYPE_VARCHAR, size=128)
            for expression, body, actual in zip(expressions, bodies, native):
                try:
                    # Compare the same PL/SQL context as the native compiler:
                    # integer-literal arithmetic can overflow in PL/SQL even
                    # when the corresponding Oracle SQL expression succeeds.
                    cursor.execute(
                        "DECLARE n NUMBER; FUNCTION f RETURN NUMBER IS "
                        + body
                        + " BEGIN n := f; SELECT DUMP(n,16) INTO :image FROM dual; :out := TO_CHAR(n, 'TM9', "
                        "'NLS_NUMERIC_CHARACTERS=''.,'''); END;",
                        out=output,
                        image=image,
                    )
                    value = output.getvalue()
                    # TO_CHAR displays both NUMBER specials as ####### and
                    # some drivers decode them as +/-1e126. Use the public
                    # DUMP representation to retain their actual identity.
                    if image.getvalue() == "Typ=2 Len=2: ff,65":
                        value = "Infinity"
                    elif image.getvalue() == "Typ=2 Len=1: 0":
                        value = "-Infinity"
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
            "pairs": None if args.runtime_specials else args.pairs,
            "cases": len(expressions),
            "scope": (
                "runtime special-value matrix"
                if args.runtime_specials
                else "arithmetic with valid finite literals plus measured boundary corpus"
            ),
            "context": "anonymous PL/SQL functions returning NUMBER",
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
