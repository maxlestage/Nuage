#!/usr/bin/env bash
# Nuage DB — installs a server on a computer or a server, in one command:
#
#   curl -fsSL https://raw.githubusercontent.com/maxlestage/Nuage/master/install.sh | sudo bash
#
# Linux (Debian, Ubuntu, Fedora, Rocky, Alpine, Raspberry Pi OS..., x86_64 or ARM64) and
# macOS (Apple silicon or Intel). On Windows, install.ps1 does the same.
#
# It sets up: the `nuage` binary (/usr/local/bin), the data, the settings and secrets in a
# `nuage.env` file (generated once, kept on upgrades), the backups where you choose, and a
# service started at boot (systemd on Linux, launchd on macOS). On Linux it also adds swap
# when there is none, a `nuage` system user, and, with a domain name, HTTPS through Caddy.
# Run it again to upgrade, or to change where the backups go: data and secrets stay.
#
#                 Linux                        macOS
#   data          /var/lib/nuage               /usr/local/var/nuage
#   settings      /etc/nuage/nuage.env         /usr/local/etc/nuage/nuage.env
#   service       systemd: nuage               launchd: /Library/LaunchDaemons/io.github.maxlestage.nuage.plist
#   logs          journalctl -u nuage -f       /usr/local/var/log/nuage.log
#
# Backups (an empty data directory is restored from them at startup). Without one of these,
# the installer asks, when it runs in a terminal:
#   NUAGE_BACKUP_DIR=/mnt/backup/nuage    a folder, on ANOTHER disk: an external disk, a NAS
#                                         (macOS: /Volumes/MyDisk/nuage)
#   NUAGE_BACKUP_S3_BUCKET=my-bucket      S3-compatible storage (AWS, Cloudflare R2, Scaleway,
#     NUAGE_BACKUP_S3_REGION=eu-west-3    MinIO, Backblaze B2...), with
#     NUAGE_BACKUP_S3_ENDPOINT=https://…  NUAGE_BACKUP_S3_ACCESS_KEY_ID and
#                                         NUAGE_BACKUP_S3_SECRET_ACCESS_KEY (endpoint: not AWS)
#   NUAGE_BACKUP=none                     no backup (not recommended)
#
# Other options (environment variables, after `sudo`):
#   NUAGE_DOMAIN=db.example.com   HTTPS on this name, Linux (its DNS must point to this server)
#   NUAGE_PORT=8080               port of Nuage DB (without a domain, the one to open)
#   NUAGE_PG_PORT=5432            port of the PostgreSQL protocol: every language's PostgreSQL
#                                 driver connects there, its password a token (0: off)
#   NUAGE_SWAP_MB=2048            swap to create when there is none, Linux (0: none);
#                                 default: the memory size, between 1 and 4 GiB
#   NUAGE_VERSION=v0.3.0          a release (default: the latest)
#   NUAGE_BINARY=/path/to/nuage   install this binary instead of downloading one
#   NUAGE_FROM_SOURCE=1           build from source with cargo (installs Rust if needed)
#
#   curl -fsSL .../install.sh | sudo NUAGE_BACKUP_DIR=/Volumes/MyDisk/nuage bash
#
# Remove (the data, the backups and the settings stay):   ... | sudo bash -s uninstall
# Remove the data and the settings too (not the backups):   ... | sudo bash -s purge

set -euo pipefail

REPO="maxlestage/Nuage"
BIN=/usr/local/bin/nuage

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() {
	printf '\033[1;31merror:\033[0m %s\n' "$*" >&2
	exit 1
}
have() { command -v "$1" >/dev/null 2>&1; }
has_systemd() { [ "$OS" = Linux ] && [ -d /run/systemd/system ] && have systemctl; }

OS=$(uname -s)
case "$OS" in
Linux)
	DATA=/var/lib/nuage
	CONF_DIR=/etc/nuage
	UNIT=/etc/systemd/system/nuage.service
	;;
Darwin)
	DATA=/usr/local/var/nuage
	CONF_DIR=/usr/local/etc/nuage
	LABEL=io.github.maxlestage.nuage
	PLIST=/Library/LaunchDaemons/$LABEL.plist
	LOG=/usr/local/var/log/nuage.log
	;;
*) die "this installer is for Linux and macOS (on Windows: install.ps1, see https://github.com/$REPO#install-on-any-computer)" ;;
esac
ENV_FILE=$CONF_DIR/nuage.env

[ "$(id -u)" -eq 0 ] || die "run it as root: curl -fsSL https://raw.githubusercontent.com/$REPO/master/install.sh | sudo bash"

# Who the service runs as: a `nuage` system user on Linux; on macOS, the person who ran sudo
# (the data and the backups on their disks are theirs), else root.
if [ "$OS" = Darwin ]; then
	RUN_AS=${SUDO_USER:-root}
	id "$RUN_AS" >/dev/null 2>&1 || RUN_AS=root
else
	RUN_AS=nuage
fi

ACTION=${1:-install}

# The value of KEY in the settings file (the last one), or nothing.
get_var() { [ -f "$ENV_FILE" ] && sed -n "s/^$1=//p" "$ENV_FILE" | tail -n1 || true; }
# Sets KEY=VALUE in the settings file: replaced where it is, else added at the end.
set_var() { # key value
	local tmp
	tmp=$(mktemp "$ENV_FILE.XXXXXX")
	awk -v k="$1" -v v="$2" 'BEGIN { done = 0 } $0 ~ "^" k "=" { if (!done) print k "=" v; done = 1; next } { print } END { if (!done) print k "=" v }' "$ENV_FILE" >"$tmp"
	cat "$tmp" >"$ENV_FILE"
	rm -f "$tmp"
}
# Removes the lines KEY=... of the settings file.
unset_var() { # key
	local tmp
	tmp=$(mktemp "$ENV_FILE.XXXXXX")
	grep -v "^$1=" "$ENV_FILE" >"$tmp" || true
	cat "$tmp" >"$ENV_FILE"
	rm -f "$tmp"
}

# --- uninstall ---------------------------------------------------------------------------
if [ "$ACTION" = uninstall ] || [ "$ACTION" = purge ]; then
	backup_dir=$(get_var NUAGE_BACKUP_DIR)
	if has_systemd; then
		systemctl disable --now nuage nuage-tls.timer 2>/dev/null || true
		rm -f "$UNIT" /etc/systemd/system/nuage-tls.service /etc/systemd/system/nuage-tls.timer
		systemctl daemon-reload
	elif [ "$OS" = Darwin ]; then
		launchctl bootout system "$PLIST" 2>/dev/null || launchctl unload "$PLIST" 2>/dev/null || true
		rm -f "$PLIST"
	fi
	rm -f /usr/local/lib/nuage/tls-sync
	rm -f "$BIN"
	if [ "$ACTION" = purge ]; then
		rm -rf "$DATA" "$CONF_DIR"
		[ "$OS" = Linux ] && { userdel nuage 2>/dev/null || true; }
		say "Nuage DB is removed, with its data and its key."
	else
		say "Nuage DB is removed. The data ($DATA) and the key ($ENV_FILE) are kept: reinstall to use them again."
	fi
	[ -n "$backup_dir" ] && say "The backups in $backup_dir are left in place (delete them yourself if you want)."
	[ "$OS" = Linux ] && say "The swap (/swapfile) and Caddy, if the installer set them up, are left in place."
	exit 0
fi
[ "$ACTION" = install ] || die "unknown command \`$ACTION\` (install, uninstall or purge)"

PORT=${NUAGE_PORT:-8080}
PG_PORT=${NUAGE_PG_PORT:-5432}
DOMAIN=${NUAGE_DOMAIN:-}
case "$PORT" in '' | *[!0-9]*) die "NUAGE_PORT=$PORT is not a port number" ;; esac
case "$PG_PORT" in '' | *[!0-9]*) die "NUAGE_PG_PORT=$PG_PORT is not a port number" ;; esac
if [ -n "$DOMAIN" ] && [ "$OS" != Linux ]; then
	warn "NUAGE_DOMAIN (HTTPS through Caddy) is for Linux servers: ignored here"
	DOMAIN=""
fi

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

sha256() { # file
	if have sha256sum; then sha256sum "$1" | awk '{ print $1 }'; else shasum -a 256 "$1" | awk '{ print $1 }'; fi
}

install_packages() { # packages... (best effort, Linux)
	[ "$OS" = Linux ] || return 1
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

# --- memory and swap (Linux) -------------------------------------------------------------
if [ "$OS" = Linux ]; then
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
else
	say "Memory: $(($(sysctl -n hw.memsize) / 1048576)) MiB"
fi

# --- the binary --------------------------------------------------------------------------
target=""
case "$OS-$(uname -m)" in
Linux-x86_64 | Linux-amd64) target=x86_64-unknown-linux-musl ;;
Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
Darwin-arm64) target=aarch64-apple-darwin ;;
Darwin-x86_64) target=x86_64-apple-darwin ;;
esac

from_source() {
	say "Building Nuage DB from source (a few minutes)"
	if ! have cargo; then
		[ -x "$HOME/.cargo/bin/cargo" ] || {
			have cc || install_packages gcc || install_packages build-base || die "install a C compiler first (Linux: gcc; macOS: xcode-select --install)"
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
	got=$(sha256 "$TMP/nuage.tar.gz")
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
# A downloaded binary carries macOS's quarantine flag: a service could not start it.
[ "$OS" = Darwin ] && { xattr -d com.apple.quarantine "$TMP/nuage" 2>/dev/null || true; }
"$TMP/nuage" --version >/dev/null || die "this binary does not run here"
# The settings file must be readable by this version (`--env-file`, 0.4.2 and later).
"$TMP/nuage" --help 2>/dev/null | grep -q -- --env-file || [ "$OS" = Linux ] ||
	die "this binary is older than 0.4.2: on macOS the service needs \`nuage --env-file\` (NUAGE_VERSION=latest, or NUAGE_FROM_SOURCE=1)"
mkdir -p "$(dirname "$BIN")"
# Replaced in one step: the running server keeps its file until it restarts.
install -m 755 "$TMP/nuage" "$BIN.new" && mv -f "$BIN.new" "$BIN"
say "Installed $("$BIN" --version) in $BIN"

# --- user, data and settings -------------------------------------------------------------
if [ "$OS" = Linux ] && ! id nuage >/dev/null 2>&1; then
	nologin=$(command -v nologin || echo /bin/false)
	if have useradd; then
		useradd --system --home-dir "$DATA" --no-create-home --shell "$nologin" nuage
	else
		adduser -S -D -H -h "$DATA" -s "$nologin" nuage
	fi
fi
mkdir -p "$DATA" "$CONF_DIR"
chown "$RUN_AS" "$DATA"
chmod 700 "$DATA"

first_install=""
if [ ! -f "$ENV_FILE" ]; then
	first_install=1
	key=$("$BIN" genkey)
	token="nuage_$("$BIN" genkey | cut -c1-40)"
	password=$("$BIN" genkey | cut -c1-24)
	admin_token=$("$BIN" genkey)
	bind=0.0.0.0
	[ -n "$DOMAIN" ] && bind=127.0.0.1
	umask 077
	cat >"$ENV_FILE" <<EOF
# Nuage DB settings (read by the nuage service). Every variable: see the README.
# Keep a copy of NUAGE_ENCRYPTION_KEY somewhere safe, away from this computer: without it
# the data and its backups cannot be read, by anyone.
PORT=$PORT
NUAGE_BIND=$bind
# The PostgreSQL protocol: any PostgreSQL driver, user any name, password a token.
NUAGE_PG_PORT=$PG_PORT
NUAGE_PG_BIND=0.0.0.0
NUAGE_DATA_DIR=$DATA
NUAGE_NAME=nuage
NUAGE_ENCRYPTION_KEY=$key
NUAGE_TOKEN=$token
NUAGE_ADMIN_PASSWORD=$password
# The management API: \`nuage apps create <name>\` makes an application with it.
NUAGE_ADMIN_TOKEN=$admin_token
# Page cache of each application, in MiB; auto: an eighth of the memory (8 to 512 MiB).
NUAGE_CACHE_MB=auto
NUAGE_PUBLIC_STATUS=false
# Backups: run the installer again with NUAGE_BACKUP_DIR=... or NUAGE_BACKUP_S3_*=... to change.
EOF
	umask 022
	if [ "$PG_PORT" = 0 ]; then unset_var NUAGE_PG_PORT; fi
else
	say "Keeping the settings and secrets of $ENV_FILE"
	PORT=$(get_var PORT)
	PORT=${PORT:-8080}
	# Upgrades get the PostgreSQL protocol too, unless the file already says something of it.
	if ! grep -q 'NUAGE_PG_PORT=' "$ENV_FILE" && [ "$PG_PORT" != 0 ]; then
		{
			echo "# The PostgreSQL protocol: any PostgreSQL driver, password a token."
			echo "NUAGE_PG_PORT=$PG_PORT"
			echo "NUAGE_PG_BIND=0.0.0.0"
		} >>"$ENV_FILE"
	fi
	PG_PORT=$(get_var NUAGE_PG_PORT)
	PG_PORT=${PG_PORT:-0}
fi
if [ -n "$DOMAIN" ]; then
	grep -q '^SITE_URL=' "$ENV_FILE" || echo "SITE_URL=https://$DOMAIN" >>"$ENV_FILE"
	set_var NUAGE_BIND 127.0.0.1
fi

# --- backups -----------------------------------------------------------------------------
# The device a path is on (its closest existing folder).
device_of() {
	local p=$1
	while [ ! -e "$p" ]; do p=$(dirname "$p"); done
	df -P "$p" 2>/dev/null | awk 'NR == 2 { print $1 }'
}

# Can we ask? A terminal, even when the script itself comes through a pipe (curl | bash).
interactive() { [ -z "${CI:-}" ] && [ -t 1 ] && { : </dev/tty; } 2>/dev/null; }
ask() { # question default -> answer
	local answer
	printf '%s ' "$1" >/dev/tty
	IFS= read -r answer </dev/tty || answer=""
	printf '%s' "${answer:-$2}"
}
ask_secret() { # question -> answer
	local answer
	printf '%s ' "$1" >/dev/tty
	IFS= read -rs answer </dev/tty || answer=""
	printf '\n' >/dev/tty
	printf '%s' "$answer"
}

backup_to_dir() { # folder
	local dir=$1
	case "$dir" in /*) ;; *) die "NUAGE_BACKUP_DIR=$dir: give a full path (/mnt/backup/nuage, /Volumes/MyDisk/nuage)" ;; esac
	# Only the last folder is made: a disk that is not plugged in must not get its folder
	# made on this computer's own disk instead.
	[ -d "$(dirname "$dir")" ] || die "$(dirname "$dir") does not exist: is the disk plugged in? (only the last folder of NUAGE_BACKUP_DIR is created)"
	mkdir -p "$dir" || die "cannot create the backup folder $dir"
	chown "$RUN_AS" "$dir"
	chmod 700 "$dir"
	if [ "$(device_of "$dir")" = "$(device_of "$DATA")" ]; then
		warn "the backup folder $dir is on the same disk as the data ($DATA): if this disk fails, both are lost."
		warn "prefer another disk (an external disk, a NAS) or S3-compatible storage."
	fi
	for k in NUAGE_BACKUP_S3_BUCKET NUAGE_BACKUP_S3_REGION NUAGE_BACKUP_S3_ENDPOINT NUAGE_BACKUP_S3_PREFIX \
		NUAGE_BACKUP_S3_ACCESS_KEY_ID NUAGE_BACKUP_S3_SECRET_ACCESS_KEY; do unset_var "$k"; done
	set_var NUAGE_BACKUP_DIR "$dir"
	say "Backups go to the folder $dir"
}

backup_to_s3() { # bucket region endpoint prefix key secret
	[ -n "$5" ] && [ -n "$6" ] || die "S3 backups need NUAGE_BACKUP_S3_ACCESS_KEY_ID and NUAGE_BACKUP_S3_SECRET_ACCESS_KEY"
	unset_var NUAGE_BACKUP_DIR
	set_var NUAGE_BACKUP_S3_BUCKET "$1"
	set_var NUAGE_BACKUP_S3_REGION "${2:-us-east-1}"
	if [ -n "$3" ]; then set_var NUAGE_BACKUP_S3_ENDPOINT "$3"; else unset_var NUAGE_BACKUP_S3_ENDPOINT; fi
	set_var NUAGE_BACKUP_S3_PREFIX "${4:-nuage/}"
	set_var NUAGE_BACKUP_S3_ACCESS_KEY_ID "$5"
	set_var NUAGE_BACKUP_S3_SECRET_ACCESS_KEY "$6"
	say "Backups go to the bucket $1${3:+ ($3)}"
}

no_backup() {
	for k in NUAGE_BACKUP_DIR NUAGE_BACKUP_S3_BUCKET NUAGE_BACKUP_S3_REGION NUAGE_BACKUP_S3_ENDPOINT NUAGE_BACKUP_S3_PREFIX \
		NUAGE_BACKUP_S3_ACCESS_KEY_ID NUAGE_BACKUP_S3_SECRET_ACCESS_KEY; do unset_var "$k"; done
	warn "no backup: if this disk fails, the data is lost. Run the installer again with NUAGE_BACKUP_DIR=... to add one."
}

if [ -n "${NUAGE_BACKUP_DIR:-}" ]; then
	backup_to_dir "$NUAGE_BACKUP_DIR"
elif [ -n "${NUAGE_BACKUP_S3_BUCKET:-}" ]; then
	backup_to_s3 "$NUAGE_BACKUP_S3_BUCKET" "${NUAGE_BACKUP_S3_REGION:-}" "${NUAGE_BACKUP_S3_ENDPOINT:-}" \
		"${NUAGE_BACKUP_S3_PREFIX:-}" "${NUAGE_BACKUP_S3_ACCESS_KEY_ID:-}" "${NUAGE_BACKUP_S3_SECRET_ACCESS_KEY:-}"
elif [ "${NUAGE_BACKUP:-}" = none ]; then
	no_backup
elif [ -n "$(get_var NUAGE_BACKUP_DIR)$(get_var NUAGE_BACKUP_S3_BUCKET)" ]; then
	dir=$(get_var NUAGE_BACKUP_DIR)
	if [ -n "$dir" ]; then
		say "Backups: kept as they are, to the folder $dir"
		[ -d "$dir" ] || warn "the backup folder $dir is not there: is the disk plugged in?"
	else
		say "Backups: kept as they are, to the bucket $(get_var NUAGE_BACKUP_S3_BUCKET)"
	fi
elif interactive; then
	echo
	say "Where should the backups go? Nuage DB restores an empty data directory from them at startup."
	echo "    1) a folder on another disk: an external disk, a NAS"
	if [ "$OS" = Darwin ]; then echo "       (disks: $(ls /Volumes 2>/dev/null | tr '\n' ' '))"; fi
	echo "    2) S3-compatible storage: AWS S3, Cloudflare R2, Scaleway, Backblaze B2, MinIO..."
	echo "    3) none for now (not recommended)"
	choice=$(ask "Your choice [1/2/3]:" 1)
	case "$choice" in
	1)
		example=/mnt/backup/nuage
		[ "$OS" = Darwin ] && example=/Volumes/MyDisk/nuage
		dir=$(ask "Folder (full path, e.g. $example):" "")
		[ -n "$dir" ] || die "no folder given: run the installer again"
		backup_to_dir "$dir"
		;;
	2)
		bucket=$(ask "Bucket:" "")
		region=$(ask "Region [us-east-1]:" us-east-1)
		endpoint=$(ask "Endpoint (empty for AWS; e.g. https://<account>.r2.cloudflarestorage.com):" "")
		prefix=$(ask "Prefix [nuage/]:" nuage/)
		access=$(ask "Access key id:" "")
		secret=$(ask_secret "Secret access key (hidden):")
		[ -n "$bucket" ] || die "no bucket given: run the installer again"
		backup_to_s3 "$bucket" "$region" "$endpoint" "$prefix" "$access" "$secret"
		;;
	*) no_backup ;;
	esac
else
	no_backup
fi

if [ "$OS" = Linux ]; then
	chown root:nuage "$ENV_FILE"
	chmod 640 "$ENV_FILE"
else
	# The service runs as $RUN_AS and reads this file itself (--env-file): theirs only.
	chown "$RUN_AS" "$ENV_FILE"
	chmod 600 "$ENV_FILE"
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
# It writes its log and pages at once: give it time to stop cleanly (and send its last backup).
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
	backup_dir=$(get_var NUAGE_BACKUP_DIR)
	[ -n "$backup_dir" ] && sed -i "s|^ReadWritePaths=.*|ReadWritePaths=$DATA $backup_dir|" "$UNIT"
	case "$backup_dir" in /home/* | /root/*) sed -i 's/^ProtectHome=true/ProtectHome=false/' "$UNIT" ;; esac
	systemctl daemon-reload
	systemctl enable nuage >/dev/null 2>&1
	systemctl restart nuage
elif [ "$OS" = Darwin ]; then
	mkdir -p "$(dirname "$LOG")"
	touch "$LOG"
	chown "$RUN_AS" "$LOG"
	# A daemon (it starts with the Mac, before anyone logs in), run as $RUN_AS.
	cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key><string>$LABEL</string>
	<key>UserName</key><string>$RUN_AS</string>
	<key>ProgramArguments</key>
	<array><string>$BIN</string><string>--env-file</string><string>$ENV_FILE</string></array>
	<key>RunAtLoad</key><true/>
	<key>KeepAlive</key><true/>
	<key>ThrottleInterval</key><integer>5</integer>
	<key>ExitTimeOut</key><integer>30</integer>
	<key>WorkingDirectory</key><string>$DATA</string>
	<key>StandardOutPath</key><string>$LOG</string>
	<key>StandardErrorPath</key><string>$LOG</string>
</dict>
</plist>
EOF
	chown root:wheel "$PLIST"
	chmod 644 "$PLIST"
	launchctl bootout system "$PLIST" 2>/dev/null || true
	launchctl bootstrap system "$PLIST" 2>/dev/null || launchctl load -w "$PLIST"
	launchctl kickstart -k "system/$LABEL" 2>/dev/null || true
else
	warn "no systemd here: start the server yourself with"
	warn "  su -s /bin/sh nuage -c '$BIN --env-file $ENV_FILE'"
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

# --- HTTPS (Linux) -----------------------------------------------------------------------
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

# TLS for the PostgreSQL protocol, from Caddy's certificate of the domain: copied now (once
# Caddy has it) and every day by a timer, which restarts Nuage DB when it changed.
if [ -n "$DOMAIN" ] && [ "$PG_PORT" != 0 ] && has_systemd && have caddy; then
	mkdir -p /usr/local/lib/nuage "$CONF_DIR/tls"
	cat >/usr/local/lib/nuage/tls-sync <<EOF
#!/bin/sh
# Copies Caddy's certificate of $DOMAIN for Nuage DB's PostgreSQL protocol (NUAGE_TLS_*).
set -e
crt=\$(find /var/lib/caddy -name '$DOMAIN.crt' 2>/dev/null | head -n1)
[ -n "\$crt" ] || exit 0
key=\${crt%.crt}.key
if cmp -s "\$crt" $CONF_DIR/tls/cert.pem && cmp -s "\$key" $CONF_DIR/tls/key.pem; then exit 0; fi
install -m 640 -o root -g nuage "\$crt" $CONF_DIR/tls/cert.pem
install -m 640 -o root -g nuage "\$key" $CONF_DIR/tls/key.pem
if ! grep -q '^NUAGE_TLS_CERT=' $ENV_FILE; then
	echo 'NUAGE_TLS_CERT=$CONF_DIR/tls/cert.pem' >>$ENV_FILE
	echo 'NUAGE_TLS_KEY=$CONF_DIR/tls/key.pem' >>$ENV_FILE
fi
systemctl try-restart nuage
EOF
	chmod 755 /usr/local/lib/nuage/tls-sync
	cat >/etc/systemd/system/nuage-tls.service <<EOF
[Unit]
Description=Nuage DB: TLS certificate of the PostgreSQL protocol, from Caddy
[Service]
Type=oneshot
ExecStart=/usr/local/lib/nuage/tls-sync
EOF
	cat >/etc/systemd/system/nuage-tls.timer <<EOF
[Unit]
Description=Nuage DB: copy Caddy's renewed certificate every day
[Timer]
OnCalendar=daily
RandomizedDelaySec=1h
Persistent=true
[Install]
WantedBy=timers.target
EOF
	systemctl daemon-reload
	systemctl enable --now nuage-tls.timer >/dev/null 2>&1 || true
	# Caddy gets the certificate within a minute of its first start.
	say "Waiting for the certificate of $DOMAIN (TLS of the PostgreSQL protocol)"
	for _ in $(seq 1 60); do
		/usr/local/lib/nuage/tls-sync && [ -f "$CONF_DIR/tls/cert.pem" ] && break
		sleep 2
	done
	[ -f "$CONF_DIR/tls/cert.pem" ] || warn "no certificate yet: the timer adds TLS to the PostgreSQL protocol once Caddy has it"
fi

# Open the firewall, when one is on (Linux).
if [ "$OS" = Linux ]; then
	if have ufw && ufw status 2>/dev/null | grep -q 'Status: active'; then
		if [ -n "$DOMAIN" ]; then ufw allow 80/tcp >/dev/null && ufw allow 443/tcp >/dev/null; else ufw allow "$PORT/tcp" >/dev/null; fi
		[ "$PG_PORT" = 0 ] || ufw allow "$PG_PORT/tcp" >/dev/null
	elif have firewall-cmd && firewall-cmd --state >/dev/null 2>&1; then
		if [ -n "$DOMAIN" ]; then
			firewall-cmd -q --permanent --add-service=http --add-service=https
		else
			firewall-cmd -q --permanent --add-port="$PORT/tcp"
		fi
		[ "$PG_PORT" = 0 ] || firewall-cmd -q --permanent --add-port="$PG_PORT/tcp"
		firewall-cmd -q --reload
	fi
fi

# --- check -------------------------------------------------------------------------------
running=""
if has_systemd || [ "$OS" = Darwin ]; then
	for _ in $(seq 1 30); do
		if fetch "http://127.0.0.1:$PORT/api/health" "$TMP/health" 2>/dev/null; then
			running=1
			break
		fi
		sleep 1
	done
	if [ -z "$running" ]; then
		if [ "$OS" = Darwin ]; then die "the server does not answer: tail -n 50 $LOG"; else die "the server does not answer: journalctl -u nuage -n 50"; fi
	fi
fi

if [ -n "$DOMAIN" ]; then
	url="https://$DOMAIN"
else
	if [ "$OS" = Darwin ]; then
		ip=$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true)
	else
		ip=$(hostname -I 2>/dev/null | awk '{ print $1 }')
	fi
	url="http://${ip:-localhost}:$PORT"
fi
if [ -n "$(get_var NUAGE_BACKUP_DIR)" ]; then
	backups="the folder $(get_var NUAGE_BACKUP_DIR)"
elif [ -n "$(get_var NUAGE_BACKUP_S3_BUCKET)" ]; then
	backups="the bucket $(get_var NUAGE_BACKUP_S3_BUCKET)"
else
	backups="none (run again with NUAGE_BACKUP_DIR=... to add one)"
fi
echo
if [ -n "$running" ]; then say "Nuage DB is running: $url"; else say "Nuage DB is installed: $url (once started)"; fi
echo "    console:      $url/admin"
echo "    SQL:          POST $url/v1/sql   (Authorization: Bearer <token>)"
if [ "$PG_PORT" != 0 ]; then
	pg_host=${DOMAIN:-${ip:-localhost}}
	pg_ssl=""
	[ -f "$CONF_DIR/tls/cert.pem" ] && pg_ssl="?sslmode=require"
	echo "    PostgreSQL:   postgresql://app:<token>@$pg_host:$PG_PORT/nuage$pg_ssl   (any language's driver)"
fi
echo "    backups:      $backups"
echo "    settings:     $ENV_FILE"
echo "    data:         $DATA"
if [ "$OS" = Darwin ]; then
	echo "    logs:         tail -f $LOG"
else
	echo "    logs:         journalctl -u nuage -f"
fi
echo "    new app:      NUAGE_URL=$url NUAGE_ADMIN_TOKEN=<NUAGE_ADMIN_TOKEN of $ENV_FILE> nuage apps create <name>"
case "$OS-$(get_var NUAGE_BACKUP_DIR)" in
Darwin-/Volumes/*)
	echo
	warn "macOS asks before a service writes to an external disk: if the backups fail (tail $LOG),"
	warn "add $BIN in System Settings → Privacy & Security → Full Disk Access."
	;;
esac
if [ -n "$first_install" ]; then
	echo
	echo "    application token:  $token"
	echo "    console password:   $password"
	echo
	warn "copy $ENV_FILE (its NUAGE_ENCRYPTION_KEY) somewhere safe, away from this computer, now:"
	warn "without that key nobody can read the data or its backups."
fi
if [ -z "$DOMAIN" ] && [ "$OS" = Linux ]; then
	echo
	echo "    For HTTPS, point a domain name to this server and run again with"
	echo "    NUAGE_DOMAIN=db.example.com (until then, the console travels in clear)."
fi
