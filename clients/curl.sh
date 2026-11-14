#!/bin/sh
# Nuage DB from the shell (curl). NUAGE_URL=https://db.example.com NUAGE_TOKEN=... sh curl.sh
# Without NUAGE_TOKEN, with the server's NUAGE_ADMIN_TOKEN: it makes its own application first.
set -eu
URL=${NUAGE_URL:-http://localhost:8080}

create_app() { # create_app name: a new application (a database of its own); prints its token
	curl -fsS "$URL/api/apps" -H "Authorization: Bearer $NUAGE_ADMIN_TOKEN" -H 'Content-Type: application/json' \
		-d "{\"name\": \"$1\"}" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p'
}

if [ -z "${NUAGE_TOKEN:-}" ] && [ -n "${NUAGE_ADMIN_TOKEN:-}" ]; then
	NUAGE_TOKEN=$(create_app notes-curl)
	echo "created the application notes-curl"
fi

sql() { # sql '{"sql": "...", "params": [...]}'
	curl -fsS "$URL/v1/sql" -H "Authorization: Bearer $NUAGE_TOKEN" -H 'Content-Type: application/json' -d "$1"
	echo
}

sql '{"sql": "CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)"}'
sql '{"sql": "INSERT INTO notes (text) VALUES (?)", "params": ["hello from curl"]}'
sql '{"sql": "SELECT id, text FROM notes ORDER BY id DESC LIMIT 5"}'
