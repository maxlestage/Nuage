#!/usr/bin/env bash
# Runs every example of this folder against a fresh Nuage DB server speaking the PostgreSQL
# protocol (with TLS, from a certificate made here), with each driver found on this machine
# (the others are skipped; CI=1 makes a missing one an error). Install them first:
#   pip install -r requirements.txt; npm install; gem install pg;
#   curl -fsSLo postgresql.jar https://repo1.maven.org/maven2/org/postgresql/postgresql/42.7.4/postgresql-42.7.4.jar
#   clients/postgres/test.sh [path/to/nuage]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-$here/../../target/debug/nuage}
[ -x "$bin" ] || (cd "$here/../.." && cargo build -q -p nuage --bin nuage)
bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
cd "$here"

dir=$(mktemp -d)
http=$((20000 + RANDOM % 20000))
pg=$((http + 1))
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 1 -subj /CN=localhost \
	-addext subjectAltName=DNS:localhost,IP:127.0.0.1 -keyout "$dir/key.pem" -out "$dir/cert.pem" 2>/dev/null
export NUAGE_TOKEN=test-token-$RANDOM NUAGE_PG_HOST=127.0.0.1 NUAGE_PG_PORT=$pg PGPASSWORD
PGPASSWORD=$NUAGE_TOKEN
PORT=$http NUAGE_PG_PORT=$pg NUAGE_BIND=127.0.0.1 NUAGE_DATA_DIR=$dir/data NUAGE_ENCRYPTION_KEY=$("$bin" genkey) \
	NUAGE_TLS_CERT=$dir/cert.pem NUAGE_TLS_KEY=$dir/key.pem "$bin" >"$dir/server.log" 2>&1 &
server=$!
trap 'kill $server 2>/dev/null; rm -rf "$dir"' EXIT
up=""
for _ in $(seq 1 100); do
	if pg_isready -q -h 127.0.0.1 -p "$pg" 2>/dev/null || (exec 3<>"/dev/tcp/127.0.0.1/$pg") 2>/dev/null; then
		up=1
		break
	fi
	sleep 0.1
done
[ -n "$up" ] || { echo "the server did not start:"; cat "$dir/server.log"; exit 1; }

failed=0
check() { # name needed command...
	local name=$1 needed=$2
	shift 2
	if ! eval "$needed" >/dev/null 2>&1; then
		if [ -n "${CI:-}" ]; then
			echo "FAIL $name: not installed ($needed)"
			failed=1
		else
			echo "skip $name (not installed)"
		fi
		return
	fi
	local out
	if out=$("$@" 2>&1) && grep -q "hello from $name" <<<"$out"; then
		echo "ok   $name"
	else
		echo "FAIL $name"
		while IFS= read -r line; do echo "     $line"; done <<<"$out"
		failed=1
	fi
}

check psql "command -v psql" sh psql.sh
tls() { psql -X -tA "host=localhost port=$pg user=app dbname=nuage sslmode=verify-full sslrootcert=$dir/cert.pem" -c "SELECT 'hello from ' || 'tls'"; }
check tls "command -v psql" tls
check psycopg "python3 -c 'import psycopg'" python3 python_psycopg.py
check psycopg2 "python3 -c 'import psycopg2'" python3 python_psycopg2.py
check sqlalchemy "python3 -c 'import sqlalchemy, psycopg'" python3 python_sqlalchemy.py
check node-postgres "test -d node_modules/pg" node node_pg.mjs
check postgres.js "test -d node_modules/postgres" node node_postgres_js.mjs
pgx() { (cd go && go run .); }
check pgx "command -v go" pgx
database_sql() { (cd go_database_sql && go run .); }
check database/sql "command -v go" database_sql
check jdbc "test -f postgresql.jar && command -v java" java -cp postgresql.jar Jdbc.java
check pdo "php -m | grep -qi pdo_pgsql" php php_pdo.php
check "ruby pg" "ruby -e 'require \"pg\"'" ruby ruby_pg.rb
tokio_postgres() { (cd rust && cargo run -q); }
check tokio-postgres "command -v cargo" tokio_postgres
check npgsql "command -v dotnet" dotnet run csharp_npgsql.cs

[ $failed = 0 ] || { echo "server log:"; cat "$dir/server.log"; exit 1; }
