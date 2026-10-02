#!/usr/bin/env python3
"""Check that the server's build doesn't depend on what is in the database.

sqlx checks queries at compile time, either against the committed offline
data in server/.sqlx or, with SQLX_OFFLINE=false, against DATABASE_URL.
This script checks three situations, each on a scratch database:

1. The default build, with DATABASE_URL pointing at an empty database. This is
   what anyone following the README for the first time does, because the
   tables only appear when the server starts and runs its migrations.
2. Live query checking against a database with the migrations applied and no data.
3. Live query checking against a database filled by `parlour seed`.

Postgres infers whether columns can be null partly from query plans, and the
plans depend on the data. Queries must therefore state nullability explicitly
wherever the inference could differ, or (2) and (3) disagree.

Uses the dev Postgres from dev/compose.yaml, or the server given by
CHECK_POSTGRES (for example postgres://parlour:parlour@localhost:5432 in CI,
where psql must be installed). Exits non-zero on any failure.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SERVER = ROOT / "server"
COMPOSE = ["docker", "compose", "-f", str(ROOT / "dev" / "compose.yaml")]
SERVER_URL = os.environ.get("CHECK_POSTGRES", "postgres://parlour:parlour@localhost:54329")


def psql(sql: str, database: str = "parlour", stdin: str | None = None) -> None:
    if "CHECK_POSTGRES" in os.environ:
        if shutil.which("psql") is None:
            sys.exit("CHECK_POSTGRES is set but psql is not installed")
        args = ["psql", "-q", "-v", "ON_ERROR_STOP=1", f"{SERVER_URL}/{database}"]
    else:
        args = [*COMPOSE, "exec", "-T", "postgres", "psql", "-q", "-v", "ON_ERROR_STOP=1",
                "-U", "parlour", "-d", database]
    args += ["-f", "-"] if stdin is not None else ["-c", sql]
    subprocess.run(args, input=stdin, text=True, check=True)


def url(database: str) -> str:
    return f"{SERVER_URL}/{database}?sslmode=disable"


def base_env() -> dict[str, str]:
    return {k: v for k, v in os.environ.items() if k not in ("SQLX_OFFLINE", "DATABASE_URL")}


def cargo(args: list[str], env: dict[str, str]) -> bool:
    # Force the query macros to run again rather than reusing an earlier build.
    subprocess.run(["cargo", "clean", "-q", "-p", "parlour"], cwd=SERVER, env=env, check=True)
    return subprocess.run(["cargo", *args], cwd=SERVER, env=env).returncode == 0


def main() -> int:
    suffix = uuid.uuid4().hex[:8]
    empty, migrated, seeded = (f"parlour_check_{kind}_{suffix}" for kind in ("empty", "migrated", "seeded"))
    failures = []
    for name in (empty, migrated, seeded):
        psql(f"CREATE DATABASE {name}")
    try:
        env = base_env() | {"DATABASE_URL": url(empty)}
        if not cargo(["build", "--all-targets"], env):
            failures.append("the default build fails against an empty database")

        for migration in sorted((SERVER / "migrations").glob("*.sql")):
            psql("", database=migrated, stdin=migration.read_text())

        with tempfile.TemporaryDirectory() as uploads:
            env = base_env() | {"DATABASE_URL": url(seeded), "PARLOUR_UPLOADS_DIR": uploads}
            subprocess.run(["cargo", "run", "-q", "--", "seed"], cwd=SERVER, env=env, check=True)
        # Query plans depend on table statistics, which a long-lived database
        # gathers through autovacuum; gather them now.
        psql("ANALYZE", database=seeded)

        for name, label in ((migrated, "a migrated, empty"), (seeded, "a seeded")):
            env = base_env() | {"DATABASE_URL": url(name), "SQLX_OFFLINE": "false"}
            if not cargo(["check", "--all-targets"], env):
                failures.append(f"live query checking fails against {label} database")
    finally:
        for name in (empty, migrated, seeded):
            psql(f"DROP DATABASE IF EXISTS {name}")
    for failure in failures:
        print(f"FAIL: {failure}", file=sys.stderr)
    if not failures:
        print("OK: the build is independent of the database's contents")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
