#!/bin/sh
# Parlour's backups (DESIGN.md §10), run in a postgres:18 container by
# deploy/compose.yaml with the database's PG* variables set and the
# backups volume at /backups.
#
#   backup.sh pre-migration   one dump of the database, now, before Parlour
#                             starts (and runs any new migrations); keeps
#                             the newest BACKUP_KEEP_PRE_MIGRATION
#   backup.sh nightly         every day at BACKUP_AT (UTC): a dump of the
#                             database and an archive of /data/uploads;
#                             keeps BACKUP_KEEP_DAYS days of them
#   backup.sh now             one nightly backup, now
#
# Dumps are in pg_dump's custom format; docs/admin-guide.md says how to
# restore one. Each is written under a temporary name and renamed when
# complete, so a file without ".partial" is a whole backup.
set -eu

dir=/backups
mkdir -p "$dir/pre-migration" "$dir/nightly"

stamp() { date -u +%Y-%m-%dT%H%M%SZ; }

# Dumps the database to $1, whole or not at all.
dump() {
    pg_dump --format=custom --no-owner --file "$1.partial"
    mv "$1.partial" "$1"
}

# Keeps the newest $2 files in $1 matching $3, removing the rest.
keep_newest() {
    ls -1t "$1"/$3 2>/dev/null | tail -n +"$(($2 + 1))" | while read -r old; do
        rm -f "$old"
    done
}

nightly_once() {
    at=$(stamp)
    dump "$dir/nightly/parlour-$at.dump"
    if [ -d /data/uploads ]; then
        tar -C /data -czf "$dir/nightly/uploads-$at.tar.gz.partial" uploads
        mv "$dir/nightly/uploads-$at.tar.gz.partial" "$dir/nightly/uploads-$at.tar.gz"
    fi
    find "$dir/nightly" -type f -mtime +"${BACKUP_KEEP_DAYS:-14}" -delete
    rm -f "$dir"/nightly/*.partial
    echo "backup.sh: nightly backup $at done"
}

case "${1:-}" in
pre-migration)
    # A database Parlour has never started on has nothing to keep.
    if [ "$(psql -tAc "SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")" = "t" ]; then
        at=$(stamp)
        dump "$dir/pre-migration/parlour-$at.dump"
        keep_newest "$dir/pre-migration" "${BACKUP_KEEP_PRE_MIGRATION:-5}" 'parlour-*.dump'
        echo "backup.sh: dumped the database before starting Parlour ($at)"
    else
        echo "backup.sh: the database is empty, so there is nothing to dump yet"
    fi
    ;;
nightly)
    at_time="${BACKUP_AT:-03:30}"
    while :; do
        now=$(date -u +%s)
        next=$(date -u -d "$(date -u +%Y-%m-%d) $at_time" +%s)
        if [ "$next" -le "$now" ]; then
            next=$((next + 86400))
        fi
        sleep $((next - now))
        nightly_once || echo "backup.sh: the nightly backup failed" >&2
    done
    ;;
now)
    nightly_once
    ;;
*)
    echo "usage: backup.sh pre-migration | nightly | now" >&2
    exit 2
    ;;
esac
