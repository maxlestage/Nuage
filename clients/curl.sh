#!/bin/sh
# Nuage DB from the shell (curl). NUAGE_URL=https://db.example.com NUAGE_TOKEN=... sh curl.sh
set -eu
URL=${NUAGE_URL:-http://localhost:8080}

sql() { # sql '{"sql": "...", "params": [...]}'
	curl -fsS "$URL/v1/sql" -H "Authorization: Bearer $NUAGE_TOKEN" -H 'Content-Type: application/json' -d "$1"
	echo
}

sql '{"sql": "CREATE TABLE IF NOT EXISTS notes (id SERIAL PRIMARY KEY, text TEXT NOT NULL)"}'
sql '{"sql": "INSERT INTO notes (text) VALUES (?)", "params": ["hello from curl"]}'
sql '{"sql": "SELECT id, text FROM notes ORDER BY id DESC LIMIT 5"}'
