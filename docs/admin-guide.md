# Running Parlour: the admin guide

This is for whoever runs a Parlour forum on a server of their own: how to
install it, upgrade it, back it up and restore it, and move it to a new
major version of Postgres. Everything here is in `deploy/` (DESIGN.md §10).

What runs, with `docker compose` from `deploy/`:

| Service | What it does |
|---|---|
| `parlour` | The forum: one static binary in a small image, built from `Dockerfile` at the repository's root |
| `postgres` | Postgres 18, the major version pinned |
| `caddy` | HTTPS for your domain, with certificates it fetches and renews itself |
| `pre-migration-dump` | A dump of the database each time the stack starts, before Parlour does (and before it migrates the database for a new version) |
| `backup` | Every night, a dump of the database and an archive of the uploaded images |

You need a server with Docker (with the compose plugin), a domain name
whose DNS points at it, ports 80 and 443 open, and an SMTP relay for email
(Parlour sends sign-up links and notifications).

## Installing

    git clone <parlour's repository> parlour
    cd parlour/deploy
    cp .env.example .env

Edit `.env`:

- `PARLOUR_DOMAIN`: the forum's domain, such as `forum.example.com`.
- `POSTGRES_PASSWORD`: a long random password, for example from
  `openssl rand -hex 32`. Parlour's connection to the database is made from
  it (`DATABASE_URL` in `compose.yaml`); nothing else needs it.
- The `PARLOUR_SMTP_*` settings and `PARLOUR_MAIL_FROM`, for your SMTP
  relay. You can also set these, and send a test email, in the admin
  settings later.

Then build and start everything:

    docker compose up -d --build

On its first start Parlour prints a one-time setup link. Find it with

    docker compose logs parlour

and open it in a browser. The setup wizard makes the admin account, names
the site, checks the email settings, creates the first spaces and the
default front page, and makes your first invites. The first spaces,
Announcements and General, are for members only; visitors who haven't
logged in see just a welcome, which you can change under Admin → Welcome,
terms and privacy.

## The database and its roles

Postgres's superuser is `postgres`. The first time Postgres starts on an
empty volume, `deploy/initdb/10-parlour.sh` makes the role Parlour
connects as, `parlour`, and its database, which it owns. That role is not
a superuser: keep it that way. Parlour runs its own migrations on start,
so its role owns its tables.

The break-glass log (DESIGN.md §8) is append-only. Nothing in Parlour
changes or removes an entry, `UPDATE`, `DELETE` and `TRUNCATE` on it are
revoked from Parlour's role, and triggers refuse them from any role. Since
that role owns the tables, someone with its password, or the superuser's,
could deliberately take the privileges back or set the triggers aside:
whoever runs the server can always do that, as they can read the whole
database. Parlour separates its moderators from that person; it can't
remove them.

## Upgrading Parlour

    cd parlour
    git pull
    cd deploy
    docker compose up -d --build

Bringing the stack up takes a dump of the database first
(`pre-migration-dump`), then starts the new version, which runs any new
migrations. The last five such dumps are kept (`BACKUP_KEEP_PRE_MIGRATION`)
in the `backups` volume, under `pre-migration/`. If an upgrade goes wrong,
go back to the version you had (`git checkout` it), restore the newest of
those dumps as **Restoring a backup** says, and bring the stack up again.

If Parlour finds an empty database where it has run before (a volume that
went missing, or a Postgres upgrade that left the data behind), it refuses
to start and says so, rather than offering a new setup link. Restore a
backup, or bring back the volume.

## Backups

Every night at `BACKUP_AT` (03:30 UTC unless you change it), the `backup`
service writes to the `backups` volume, under `nightly/`:

- `parlour-<time>.dump`: the whole database, in pg_dump's custom format;
- `uploads-<time>.tar.gz`: the uploaded images, and the file that ties
  them to this database.

It keeps `BACKUP_KEEP_DAYS` days of them (14 unless you change it). To take
one now:

    docker compose exec backup /bin/sh /backup.sh now

The backups are on the same server as the forum. Copy them somewhere else
too, for example every night after 04:00:

    docker run --rm -v parlour_backups:/backups:ro -v "$PWD/offsite:/out" \
        alpine sh -c 'cp -a /backups/nightly/. /out/'

and then send `offsite/` wherever you keep backups.

## Restoring a backup

Restore the database and the uploads from the same night: the uploads hold
the forum's id, which Parlour checks against the database's when it
starts. In `deploy/`:

1. Stop Parlour and the nightly backups:

       docker compose stop parlour backup

2. Empty the database:

       docker compose exec postgres psql -U postgres \
           -c "DROP DATABASE parlour WITH (FORCE)" \
           -c "CREATE DATABASE parlour OWNER parlour"

3. Restore the dump (name the file you want; `ls` the volume to see them):

       docker compose run --rm --no-deps --entrypoint ls backup -l /backups/nightly
       docker compose run --rm --no-deps --entrypoint pg_restore backup \
           --no-owner --exit-on-error -d parlour /backups/nightly/parlour-<time>.dump

   For a dump taken before an upgrade, use
   `/backups/pre-migration/parlour-<time>.dump` instead, and skip step 4:
   the uploads haven't changed.

4. Restore the uploads from the same night:

       docker run --rm -v parlour_uploads:/data/uploads -v parlour_backups:/backups:ro \
           alpine sh -c 'find /data/uploads -mindepth 1 -delete \
               && tar -C /data -xzf /backups/nightly/uploads-<time>.tar.gz'

5. Start everything again:

       docker compose up -d

## Upgrading Postgres to a new major version

A new major version of Postgres can't read the old one's data directory,
so the data is moved by dumping it and restoring it into the new version.
The image's major version is pinned in `compose.yaml` (`postgres:18`) so
that this never happens by surprise. For, say, Postgres 19, in `deploy/`:

1. Stop Parlour and take a backup now:

       docker compose stop parlour backup
       docker compose run --rm --no-deps --entrypoint /bin/sh backup /backup.sh now

   Note the time in the new files' names (`ls` the volume as in
   **Restoring a backup**).

2. Stop Postgres:

       docker compose stop postgres

   In `compose.yaml`, change every `postgres:18` to `postgres:19`, and the
   `postgres` service's volume to a new one, so that the old data stays
   where it is until you are done:

       volumes:
         - postgres-19:/var/lib/postgresql
         - ./initdb:/docker-entrypoint-initdb.d:ro

   and add `postgres-19:` under `volumes:` at the end.

3. Start the new Postgres, which makes the `parlour` role and database
   afresh:

       docker compose up -d postgres

4. Restore the dump into it, as step 3 of **Restoring a backup** says
   (the database is empty already, so skip step 2). The uploads stay as
   they are.

5. Start everything:

       docker compose up -d

Once the forum is working, remove the old volume:
`docker volume rm parlour_postgres`.

## Configuration

Parlour reads these from the environment (`compose.yaml` sets them from
`.env`); everything else is set in the admin pages.

| Variable | |
|---|---|
| `PARLOUR_DOMAIN` | The forum's domain; its address is `https://` and the domain |
| `POSTGRES_PASSWORD` | The database's password, for both the superuser and Parlour's role |
| `PARLOUR_SMTP_HOST`, `PARLOUR_SMTP_PORT`, `PARLOUR_SMTP_SECURITY` (`none`, `starttls`, `tls`), `PARLOUR_SMTP_USERNAME`, `PARLOUR_SMTP_PASSWORD`, `PARLOUR_MAIL_FROM` | The SMTP relay, unless one is saved in the admin settings |
| `BACKUP_AT`, `BACKUP_KEEP_DAYS`, `BACKUP_KEEP_PRE_MIGRATION` | When the nightly backup runs (UTC), how many days of them to keep, and how many dumps from before each start |

`parlour --help` lists the server's own variables, which `compose.yaml`
sets for you.
