#!/usr/bin/env bash
# CI: install.sh end to end on Linux (systemd) and macOS (launchd), with a backup folder.
#   .github/install-test.sh <nuage binary> <backup folder>
# It installs the binary as a service, checks that it answers, that its first backup lands in
# the folder, that `nuage apps create` makes an application, that a run without options keeps
# the backup setting, then uninstalls, checks the backups stay, and purges.
set -euo pipefail

binary=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
backup=$2
port=8091
if [ "$(uname -s)" = Darwin ]; then env_file=/usr/local/etc/nuage/nuage.env; else env_file=/etc/nuage/nuage.env; fi

step() { printf '\n\033[1;36m### %s\033[0m\n' "$*"; }
fail() {
	printf '\033[1;31mFAILED: %s\033[0m\n' "$*" >&2
	if [ "$(uname -s)" = Darwin ]; then sudo tail -n 80 /usr/local/var/log/nuage.log || true; else sudo journalctl -u nuage -n 80 --no-pager || true; fi
	exit 1
}
health() { curl -fsS "http://127.0.0.1:$port/api/health" >/dev/null 2>&1; }

step "install, backups to $backup"
sudo env CI=1 NUAGE_BINARY="$binary" NUAGE_SWAP_MB=0 NUAGE_PORT=$port NUAGE_PG_PORT=0 NUAGE_BACKUP_DIR="$backup" bash install.sh
health || fail "the service does not answer"
sudo grep -q "^NUAGE_BACKUP_DIR=$backup\$" "$env_file" || fail "NUAGE_BACKUP_DIR is not in $env_file"

step "the first backup lands in the folder"
for _ in $(seq 1 90); do
	sudo test -f "$backup/manifest.json" && break
	sleep 1
done
sudo test -f "$backup/manifest.json" || fail "no backup in $backup after 90 s"
sudo ls -la "$backup"

step "nuage apps create, with the management token the installer generated"
admin=$(sudo sed -n 's/^NUAGE_ADMIN_TOKEN=//p' "$env_file")
[ -n "$admin" ] || fail "no NUAGE_ADMIN_TOKEN in $env_file"
lines=$(NUAGE_URL=http://127.0.0.1:$port NUAGE_ADMIN_TOKEN=$admin "$binary" apps create essai "CI")
token=$(printf '%s\n' "$lines" | sed -n 's/^NUAGE_TOKEN=//p')
[ -n "$token" ] || fail "no token: $lines"
"$binary" check "http://127.0.0.1:$port" "$token" || fail "the new token does not connect"

step "nuage apps create on the server's computer, with nothing else: the settings file says it all"
lines=$(sudo env -u NUAGE_URL -u NUAGE_ADMIN_TOKEN "$binary" apps create sans-reglage)
printf '%s\n' "$lines" | grep -q '^NUAGE_TOKEN=nuage_' || fail "no token without settings: $lines"
printf '%s\n' "$lines" | grep -q "^NUAGE_URL=http://127.0.0.1:$port\$" || fail "not the local server: $lines"

step "run again without options: the backup setting stays, the service restarts"
sudo env CI=1 NUAGE_BINARY="$binary" NUAGE_SWAP_MB=0 bash install.sh | tee "$RUNNER_TEMP/again.txt"
grep -q "Backups: kept as they are" "$RUNNER_TEMP/again.txt" || fail "the backup setting was not kept"
health || fail "the service does not answer after the upgrade"
NUAGE_URL=http://127.0.0.1:$port NUAGE_ADMIN_TOKEN=$admin "$binary" apps list | grep -q '^essai' || fail "the application is gone"

step "uninstall: the service stops, the backups and the settings stay"
sudo bash install.sh uninstall
sleep 2
! health || fail "still answering after uninstall"
sudo test -f "$backup/manifest.json" || fail "the backups were removed"
sudo test -f "$env_file" || fail "the settings were removed"

step "purge: the data and the settings go, the backups stay"
sudo bash install.sh purge
! sudo test -e "$env_file" || fail "the settings are still there"
sudo test -f "$backup/manifest.json" || fail "the backups were removed"
printf '\n\033[1;32mall good\033[0m\n'
