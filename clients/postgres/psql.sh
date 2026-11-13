#!/bin/sh
# Nuage DB with psql, PostgreSQL's own client. The password is a token of an application.
#   PGPASSWORD=<token> sh psql.sh   (NUAGE_PG_HOST, NUAGE_PG_PORT: where Nuage DB listens)
set -eu
psql -X -v ON_ERROR_STOP=1 "host=${NUAGE_PG_HOST:-localhost} port=${NUAGE_PG_PORT:-5432} user=app dbname=nuage" <<'SQL'
CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL);
INSERT INTO notes (text) VALUES ('hello from psql');
SELECT id, text FROM notes ORDER BY id DESC LIMIT 5;
SQL
