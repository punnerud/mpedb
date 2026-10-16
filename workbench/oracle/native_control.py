#!/usr/bin/env python3
"""Control the explicitly configured, unofficial local Oracle test instance.

This does not install Oracle or change host users/groups. The runtime config
and init file belong to the external test directory, outside the repository.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("status", "start", "stop"))
    parser.add_argument("--root", required=True, type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    env = os.environ.copy()
    env.update(json.loads((root / "native-runtime.json").read_text()))
    home = Path(env["ORACLE_HOME"])

    def sql(text):
        return subprocess.run(
            [str(home / "bin/sqlplus"), "-L", "-s", "/ as sysdba"],
            input="whenever sqlerror exit failure\n" + text + "\nexit\n",
            env=env,
            text=True,
            capture_output=True,
            timeout=120,
        )

    def listener(action):
        return subprocess.run(
            [str(home / "bin/lsnrctl"), action],
            env=env,
            text=True,
            capture_output=True,
            timeout=30,
        )

    def show(result):
        print(result.stdout, end="")
        print(result.stderr, end="")

    status = sql("SELECT instance_name, status, version_full FROM v$instance;")
    if args.action == "status":
        show(status)
        if status.returncode:
            return status.returncode
        database = sql(
            "SELECT name, open_mode FROM v$database;\nSELECT name, open_mode FROM v$pdbs;"
        )
        show(database)
        network = listener("status")
        show(network)
        return database.returncode or network.returncode
    if args.action == "stop":
        if status.returncode == 0:
            result = sql("SHUTDOWN IMMEDIATE;")
            show(result)
            if result.returncode:
                return result.returncode
        if listener("status").returncode == 0:
            result = listener("stop")
            show(result)
            return result.returncode
        return 0
    if listener("status").returncode:
        result = listener("start")
        show(result)
        if result.returncode:
            return result.returncode
    if status.returncode:
        pfile = str(root / "native-private/initFREE.ora").replace("'", "''")
        startup = (
            "STARTUP;"
            if (home / "dbs/spfileFREE.ora").exists()
            else f"STARTUP PFILE='{pfile}';"
        )
        result = sql(startup + "\nALTER SYSTEM REGISTER;")
        show(result)
        return result.returncode
    show(status)
    print("Instance already running; no restart performed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
