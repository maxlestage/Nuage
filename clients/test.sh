#!/usr/bin/env bash
# Runs every example of this folder against a fresh Nuage DB server, with each language
# found on this machine (the others are skipped; CI=1 makes a missing one an error).
#   clients/test.sh [path/to/nuage]     (default: target/debug/nuage, built if missing)
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-$here/../target/debug/nuage}
[ -x "$bin" ] || (cd "$here/.." && cargo build -q -p nuage --bin nuage)
bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
cd "$here"

dir=$(mktemp -d)
port=$((20000 + RANDOM % 20000))
export NUAGE_URL=http://127.0.0.1:$port NUAGE_TOKEN=test-token-$RANDOM
PORT=$port NUAGE_DATA_DIR=$dir/data NUAGE_TOKEN=$NUAGE_TOKEN NUAGE_ENCRYPTION_KEY=$("$bin" genkey) \
	"$bin" >"$dir/server.log" 2>&1 &
server=$!
trap 'kill $server 2>/dev/null; rm -rf "$dir"' EXIT
up=""
for _ in $(seq 1 100); do
	if curl -fs "$NUAGE_URL/api/health" >/dev/null 2>&1; then
		up=1
		break
	fi
	sleep 0.1
done
[ -n "$up" ] || { echo "the server did not start:"; cat "$dir/server.log"; exit 1; }

failed=0
check() { # name runtime command...
	local name=$1 runtime=$2
	shift 2
	if ! command -v "$runtime" >/dev/null; then
		if [ -n "${CI:-}" ]; then
			echo "FAIL $name: $runtime is missing"
			failed=1
		else
			echo "skip $name ($runtime not installed)"
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

check curl curl sh curl.sh
check javascript node node javascript.mjs
check typescript bun bun typescript.ts
check python python3 python3 python.py
check php php php php.php
check go go go run go.go
check ruby ruby ruby ruby.rb
check java java java Nuage.java
check csharp dotnet dotnet run csharp.cs
check perl perl perl perl.pl
check rust cargo cargo run -q --manifest-path rust/Cargo.toml
run_c() { cc c.c -lcurl -o "$dir/c" && "$dir/c"; }
check c cc run_c
run_kotlin() { kotlinc kotlin.kt -include-runtime -d "$dir/kotlin.jar" 2>/dev/null && java -jar "$dir/kotlin.jar"; }
check kotlin kotlinc run_kotlin
check swift swift swift swift.swift
check dart dart dart run dart.dart
check elixir elixir elixir elixir.exs

# An error comes back as {"error": {"kind", "message"}} with its status code.
code=$(curl -s -o "$dir/err" -w '%{http_code}' --max-time 10 "$NUAGE_URL/v1/sql" -H "Authorization: Bearer $NUAGE_TOKEN" -d '{"sql": "SELECT * FROM nowhere"}' || true)
if [ "$code" -ge 400 ] && grep -q '"kind"' "$dir/err"; then echo "ok   errors ($code)"; else echo "FAIL errors"; failed=1; fi
# The description of the API, for code generators.
if curl -fs "$NUAGE_URL/v1/openapi.json" | grep -q '"openapi"'; then echo "ok   openapi.json"; else echo "FAIL openapi.json"; failed=1; fi

[ $failed = 0 ] || { echo "server log:"; cat "$dir/server.log"; exit 1; }
