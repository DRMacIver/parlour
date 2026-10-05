#!/bin/sh
# Runs once, when Postgres starts on an empty data directory: makes the
# role Parlour connects as, and its database, which it owns. The role is
# not a superuser, so that the database's own checks, such as those that
# keep the break-glass log append-only (DESIGN.md §4), hold against it.
set -eu
psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname postgres \
    --set=password="$POSTGRES_PASSWORD" <<'SQL'
CREATE ROLE parlour LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD :'password';
CREATE DATABASE parlour OWNER parlour;
SQL
