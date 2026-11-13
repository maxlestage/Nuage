#!/usr/bin/env bash
# Nuage DB — installs a server on Linux (Debian, Ubuntu, Fedora, Rocky, Alpine...,
# x86_64 or ARM64), in one command:
#
#   curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh | sudo bash
#
# It sets up: the `nuage` binary (/usr/local/bin), swap when the server has none, a `nuage`
# system user, the data in /var/lib/nuage, the settings and secrets in /etc/nuage/nuage.env
# (generated once, kept on upgrades), a systemd service started at boot, and, with a domain
# name, HTTPS through Caddy. Run it again to upgrade: data and secrets stay.
#
# Options (environment variables, after `sudo`):
#   NUAGE_DOMAIN=db.example.com   HTTPS on this name (its DNS must point to this server)
#   NUAGE_PORT=8080               port of Nuage DB (without a domain, the one to open)
#   NUAGE_SWAP_MB=2048            swap to create when there is none (0: none);
#                                 default: the memory size, between 1 and 4 GiB
#   NUAGE_VERSION=v0.3.0          a release (default: the latest)
#   NUAGE_BINARY=/path/to/nuage   install this binary instead of downloading one
#   NUAGE_FROM_SOURCE=1           build from source with cargo (installs Rust if needed)
#
#   curl -fsSL .../install.sh | sudo NUAGE_DOMAIN=db.example.com bash
#
# Remove (the data and /etc/nuage stay):   ... | sudo bash -s uninstall
# Remove everything, data included:        ... | sudo bash -s purge

set -euo pipefail

REPO="maxlestage/Nuage"
BIN=/usr/local/bin/nuage
DATA=/var/lib/nuage
CONF_DIR=/etc/nuage
ENV_FILE=$CONF_DIR/nuage.env
UNIT=/etc/systemd/system/nuage.service

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() {
	printf '\033[1;31merror:\033[0m %s\n' "$*" >&2
	exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }
has_systemd() { [ -d /run/systemd/system ] && have systemctl; }

[ "$(uname -s)" = Linux ] || die "this installer is for Linux servers (on macOS or Windows, download a binary from https://github.com/$REPO/releases)"
[ "$(id -u)" -eq 0 ] || die "run it as root: curl -fsSL https://raw.githubusercontent.com/$REPO/master/install.sh | sudo bash"

ACTION=${1:-install}

# --- uninstall ---------------------------------------------------------------------------
if [ "$ACTION" = uninstall ] || [ "$ACTION" = purge ]; then
	if has_systemd; then
		systemctl disable --now nuage 2>/dev/null || true
		rm -f "$UNIT"
		systemctl daemon-reload
	fi
	rm -f "$BIN"
	if [ "$ACTION" = purge ]; then
		rm -rf "$DATA" "$CONF_DIR"
		userdel nuage 2>/dev/null || true
		say "Nuage DB is removed, with its data and its key."
	else
		say "Nuage DB is removed. The data ($DATA) and the key ($ENV_FILE) are kept: reinstall to use them again."
	fi
	say "The swap (/swapfile) and Caddy, if the installer set them up, are left in place."
	exit 0
fi
[ "$ACTION" = install ] || die "unknown command \`$ACTION\` (install, uninstall or purge)"

PORT=${NUAGE_PORT:-8080}
DOMAIN=${NUAGE_DOMAIN:-}
case "$PORT" in '' | *[!0-9]*) die "NUAGE_PORT=$PORT is not a port number" ;; esac

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fetch() { # url file
	if have curl; then
		curl -fsSL --retry 3 -o "$2" "$1"
	elif have wget; then
		wget -q -O "$2" "$1"
	else
		return 1
	fi
}

install_packages() { # packages... (best effort)
	if have apt-get; then
		DEBIAN_FRONTEND=noninteractive apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$@"
	elif have dnf; then
		dnf install -y -q "$@"
	elif have yum; then
		yum install -y -q "$@"
	elif have apk; then
		apk add --no-cache -q "$@"
	elif have zypper; then
		zypper --non-interactive install "$@"
	elif have pacman; then
		pacman -Sy --noconfirm "$@"
	else
		return 1
	fi
}

have curl || have wget || install_packages curl ca-certificates || die "install curl or wget first"

# --- memory and swap ---------------------------------------------------------------------
mem_mb=$(awk '/^MemTotal:/ { print int($2 / 1024) }' /proc/meminfo)
swap_mb=$(awk '/^SwapTotal:/ { print int($2 / 1024) }' /proc/meminfo)
say "Memory: ${mem_mb} MiB, swap: ${swap_mb} MiB"

make_swap() {
	local want=${NUAGE_SWAP_MB:-}
	if [ -z "$want" ]; then
		want=$mem_mb
		[ "$want" -lt 1024 ] && want=1024
		[ "$want" -gt 4096 ] && want=4096
	fi
	case "$want" in *[!0-9]*) die "NUAGE_SWAP_MB=$want is not a number of MiB" ;; esac
	[ "$want" -eq 0 ] && return 0
	if [ "$swap_mb" -gt 0 ]; then
		say "Swap already set up: kept as it is"
		return 0
	fi
	if [ -e /swapfile ]; then
		warn "/swapfile exists but is not in use: left alone"
		return 0
	fi
	# Leave at least 2 GiB of disk to the data.
	local free_mb
	free_mb=$(df -Pm / | awk 'NR == 2 { print $4 }')
	if [ "$free_mb" -lt $((want + 2048)) ]; then
		want=$((free_mb - 2048))
		if [ "$want" -lt 256 ]; then
			warn "not enough disk for swap (${free_mb} MiB free): none created"
			return 0
		fi
	fi
	say "Creating ${want} MiB of swap in /swapfile"
	if [ "$(stat -f -c %T / 2>/dev/null)" = btrfs ]; then
		truncate -s 0 /swapfile && chattr +C /swapfile 2>/dev/null || true
	fi
	if ! { fallocate -l "${want}M" /swapfile 2>/dev/null || dd if=/dev/zero of=/swapfile bs=1M count="$want" status=none; } ||
		! chmod 600 /swapfile || ! mkswap /swapfile >/dev/null || ! swapon /swapfile 2>/dev/null; then
		swapoff /swapfile 2>/dev/null || true
		rm -f /swapfile
		warn "this system does not allow swap here (a container?): none created"
		return 0
	fi
	grep -q '^/swapfile ' /etc/fstab 2>/dev/null || echo '/swapfile none swap sw 0 0' >>/etc/fstab
	swap_mb=$want
}
make_swap
if [ "$swap_mb" -gt 0 ]; then
	# Swap only what is really idle: the memory goes to the database and its cache.
	echo 'vm.swappiness = 10' >/etc/sysctl.d/90-nuage.conf
	sysctl -q -w vm.swappiness=10 2>/dev/null || true
fi

# --- the binary --------------------------------------------------------------------------
target=""
case "$(uname -m)" in
x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
esac

from_source() {
	say "Building Nuage DB from source (a few minutes)"
	if ! have cargo; then
		[ -x "$HOME/.cargo/bin/cargo" ] || {
			have cc || install_packages gcc || install_packages build-base || die "install a C compiler (gcc) first"
			fetch https://sh.rustup.rs "$TMP/rustup.sh" || die "cannot download rustup"
			sh "$TMP/rustup.sh" -y -q --profile minimal >/dev/null
		}
		export PATH="$HOME/.cargo/bin:$PATH"
	fi
	local tag=()
	[ -n "${NUAGE_VERSION:-}" ] && tag=(--tag "$NUAGE_VERSION")
	cargo install --locked --quiet --git "https://github.com/$REPO" ${tag[@]+"${tag[@]}"} --root "$TMP/build" nuage ||
		die "the build failed"
	cp "$TMP/build/bin/nuage" "$TMP/nuage"
}

download() {
	[ -n "$target" ] || return 1
	local base
	if [ -n "${NUAGE_VERSION:-}" ]; then
		base="https://github.com/$REPO/releases/download/$NUAGE_VERSION"
	else
		base="https://github.com/$REPO/releases/latest/download"
	fi
	say "Downloading Nuage DB ($target)"
	fetch "$base/nuage-$target.tar.gz" "$TMP/nuage.tar.gz" || return 1
	fetch "$base/nuage-$target.tar.gz.sha256" "$TMP/nuage.sha256" || return 1
	local want got
	want=$(awk '{ print $1 }' "$TMP/nuage.sha256")
	got=$(sha256sum "$TMP/nuage.tar.gz" | awk '{ print $1 }')
	[ "$want" = "$got" ] || die "the download is corrupted (sha256 $got, expected $want)"
	tar -xzf "$TMP/nuage.tar.gz" -C "$TMP" nuage
}

if [ -n "${NUAGE_BINARY:-}" ]; then
	cp "$NUAGE_BINARY" "$TMP/nuage"
elif [ -n "${NUAGE_FROM_SOURCE:-}" ]; then
	from_source
elif ! download; then
	warn "no release binary for this system: building from source instead"
	from_source
fi
chmod 755 "$TMP/nuage"
"$TMP/nuage" --version >/dev/null || die "this binary does not run here"
# Replaced in one step: the running server keeps its file until it restarts.
install -m 755 "$TMP/nuage" "$BIN.new" && mv -f "$BIN.new" "$BIN"
say "Installed $("$BIN" --version) in $BIN"

# --- user, data and settings -------------------------------------------------------------
if ! id nuage >/dev/null 2>&1; then
	nologin=$(command -v nologin || echo /bin/false)
	if have useradd; then
		useradd --system --home-dir "$DATA" --no-create-home --shell "$nologin" nuage
	else
		adduser -S -D -H -h "$DATA" -s "$nologin" nuage
	fi
fi
mkdir -p "$DATA" "$CONF_DIR"
chown nuage "$DATA"
chmod 700 "$DATA"

first_install=""
if [ ! -f "$ENV_FILE" ]; then
	first_install=1
	key=$("$BIN" genkey)
	token="nuage_$("$BIN" genkey | cut -c1-40)"
	password=$("$BIN" genkey | cut -c1-24)
	bind=0.0.0.0
	[ -n "$DOMAIN" ] && bind=127.0.0.1
	umask 077
	cat >"$ENV_FILE" <<EOF
# Nuage DB settings (read by the nuage service). Every variable: see the README.
# Keep a copy of NUAGE_ENCRYPTION_KEY somewhere safe: without it the data and its
# backups cannot be read, by anyone.
PORT=$PORT
NUAGE_BIND=$bind
NUAGE_DATA_DIR=$DATA
NUAGE_NAME=nuage
NUAGE_ENCRYPTION_KEY=$key
NUAGE_TOKEN=$token
NUAGE_ADMIN_PASSWORD=$password
# Page cache of each application, in MiB; auto: an eighth of the memory (8 to 512 MiB).
NUAGE_CACHE_MB=auto
NUAGE_PUBLIC_STATUS=false
# Backups (recommended), to a folder on another disk or to S3-compatible storage:
# NUAGE_BACKUP_DIR=/mnt/backup/nuage
# NUAGE_BACKUP_S3_BUCKET=...
EOF
	umask 022
else
	say "Keeping the settings and secrets of $ENV_FILE"
	PORT=$(sed -n 's/^PORT=//p' "$ENV_FILE" | tail -n1)
	PORT=${PORT:-8080}
fi
chown root:nuage "$ENV_FILE"
chmod 640 "$ENV_FILE"
if [ -n "$DOMAIN" ]; then
	grep -q '^SITE_URL=' "$ENV_FILE" || echo "SITE_URL=https://$DOMAIN" >>"$ENV_FILE"
	grep -q '^NUAGE_BIND=' "$ENV_FILE" && sed -i 's/^NUAGE_BIND=.*/NUAGE_BIND=127.0.0.1/' "$ENV_FILE"
fi

# --- the service -------------------------------------------------------------------------
if has_systemd; then
	cat >"$UNIT" <<EOF
[Unit]
Description=Nuage DB
Documentation=https://github.com/$REPO
After=network-online.target
Wants=network-online.target

[Service]
User=nuage
Group=nuage
EnvironmentFile=$ENV_FILE
ExecStart=$BIN
Restart=always
RestartSec=2
# It writes its log and pages at once: give it time to stop cleanly.
TimeoutStopSec=30
LimitNOFILE=65536
# The kernel kills other processes before it when memory runs out.
OOMScoreAdjust=-500
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=$DATA
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
CapabilityBoundingSet=
AmbientCapabilities=

[Install]
WantedBy=multi-user.target
EOF
	# Backups to a folder outside the data directory need to be writable too.
	backup_dir=$(sed -n 's/^NUAGE_BACKUP_DIR=//p' "$ENV_FILE" | tail -n1)
	[ -n "$backup_dir" ] && sed -i "s|^ReadWritePaths=.*|ReadWritePaths=$DATA $backup_dir|" "$UNIT"
	systemctl daemon-reload
	systemctl enable nuage >/dev/null 2>&1
	systemctl restart nuage
else
	warn "no systemd here: start the server yourself with"
	warn "  su -s /bin/sh nuage -c 'set -a; . $ENV_FILE; exec $BIN'"
fi

# Caddy's own package repository, for Debian and Ubuntu versions without Caddy.
caddy_apt_repo() {
	have apt-get || return 1
	install_packages gnupg debian-keyring debian-archive-keyring apt-transport-https || return 1
	fetch https://dl.cloudsmith.io/public/caddy/stable/gpg.key "$TMP/caddy.key" || return 1
	gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg "$TMP/caddy.key" || return 1
	fetch https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt /etc/apt/sources.list.d/caddy-stable.list || return 1
	install_packages caddy
}

# --- HTTPS -------------------------------------------------------------------------------
if [ -n "$DOMAIN" ]; then
	if ! have caddy; then
		say "Installing Caddy (HTTPS, with a free Let's Encrypt certificate)"
		install_packages caddy || caddy_apt_repo || warn "Caddy could not be installed: put an HTTPS proxy in front of 127.0.0.1:$PORT yourself"
	fi
	if have caddy; then
		mkdir -p /etc/caddy
		block="$DOMAIN {
	encode zstd gzip
	reverse_proxy 127.0.0.1:$PORT
}"
		if [ ! -s /etc/caddy/Caddyfile ] || grep -q 'easy way to configure' /etc/caddy/Caddyfile; then
			printf '%s\n' "$block" >/etc/caddy/Caddyfile
		elif ! grep -q "^$DOMAIN " /etc/caddy/Caddyfile; then
			printf '\n%s\n' "$block" >>/etc/caddy/Caddyfile
		fi
		if has_systemd; then
			systemctl enable caddy >/dev/null 2>&1 || true
			systemctl reload caddy 2>/dev/null || systemctl restart caddy || warn "Caddy did not start: journalctl -u caddy"
		fi
	fi
fi

# Open the firewall, when one is on.
if have ufw && ufw status 2>/dev/null | grep -q 'Status: active'; then
	if [ -n "$DOMAIN" ]; then ufw allow 80/tcp >/dev/null && ufw allow 443/tcp >/dev/null; else ufw allow "$PORT/tcp" >/dev/null; fi
elif have firewall-cmd && firewall-cmd --state >/dev/null 2>&1; then
	if [ -n "$DOMAIN" ]; then
		firewall-cmd -q --permanent --add-service=http --add-service=https
	else
		firewall-cmd -q --permanent --add-port="$PORT/tcp"
	fi
	firewall-cmd -q --reload
fi

# --- check -------------------------------------------------------------------------------
if has_systemd; then
	ok=""
	for _ in $(seq 1 30); do
		if fetch "http://127.0.0.1:$PORT/api/health" "$TMP/health" 2>/dev/null; then
			ok=1
			break
		fi
		sleep 1
	done
	[ -n "$ok" ] || die "the server does not answer: journalctl -u nuage -n 50"
fi

if [ -n "$DOMAIN" ]; then
	url="https://$DOMAIN"
else
	ip=$(hostname -I 2>/dev/null | awk '{ print $1 }')
	url="http://${ip:-this-server}:$PORT"
fi
echo
if has_systemd; then say "Nuage DB is running: $url"; else say "Nuage DB is installed: $url (once started)"; fi
echo "    console:      $url/admin"
echo "    SQL:          POST $url/v1/sql   (Authorization: Bearer <token>)"
echo "    settings:     $ENV_FILE"
echo "    data:         $DATA"
echo "    logs:         journalctl -u nuage -f"
if [ -n "$first_install" ]; then
	echo
	echo "    application token:  $token"
	echo "    console password:   $password"
	echo
	warn "copy $ENV_FILE (its NUAGE_ENCRYPTION_KEY) somewhere safe now:"
	warn "without that key nobody can read the data or its backups."
fi
if [ -z "$DOMAIN" ]; then
	echo
	echo "    For HTTPS, point a domain name to this server and run again with"
	echo "    NUAGE_DOMAIN=db.example.com (until then, the console travels in clear)."
fi
