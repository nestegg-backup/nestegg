#!/bin/sh
# Downloaded over the hub's pinned HTTPS connection. No secrets are logged.
set -eu
umask 077
hub=@HUB_URL@
pin=@HUB_PIN@
[ "$(id -u)" -eq 0 ] || { echo 'Run this script as root (sudo sh).' >&2; exit 1; }
[ "$#" -ge 1 ] || { echo 'Usage: join.sh CODE [--name BACKUP-ID] [--adopt]' >&2; exit 1; }
code=$1
shift
[ -r /etc/os-release ] || { echo 'Supported: Debian-family systems on amd64.' >&2; exit 1; }
. /etc/os-release
case " ${ID:-} ${ID_LIKE:-} " in
  *' debian '*|*' ubuntu '*|*' linuxmint '*) ;;
  *) echo 'This system is unsupported; stage 1 requires Debian, Ubuntu or Linux Mint on amd64.' >&2; exit 1 ;;
esac
[ "$(dpkg --print-architecture)" = amd64 ] || { echo 'This architecture is unsupported; amd64 is required.' >&2; exit 1; }
export DEBIAN_FRONTEND=noninteractive
if ! command -v curl >/dev/null 2>&1 || [ ! -f /etc/ssl/certs/ca-certificates.crt ]; then
    apt-get update
    apt-get install -y curl ca-certificates
fi
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
curl -fsSL -o "$tmp/proxmox.gpg" https://enterprise.proxmox.com/debian/proxmox-archive-keyring-trixie.gpg
printf '%s  %s\n' '136673be77aba35dcce385b28737689ad64fd785a797e57897589aed08db6e45' "$tmp/proxmox.gpg" | sha256sum -c -
install -m 644 "$tmp/proxmox.gpg" /usr/share/keyrings/proxmox-archive-keyring.gpg
printf '%s\n' 'Types: deb' 'URIs: http://download.proxmox.com/debian/pbs-client' \
  'Suites: trixie' 'Components: main' \
  'Signed-By: /usr/share/keyrings/proxmox-archive-keyring.gpg' > /etc/apt/sources.list.d/pbs-client.sources
apt-get update
apt-get install -y proxmox-backup-client-static
# No redirect following: a pinned hub cannot redirect a download elsewhere.
curl -fsS -k --pinnedpubkey "$pin" -o "$tmp/nestegg.deb" "$hub/pkg/nestegg.deb"
curl -fsS -k --pinnedpubkey "$pin" -o "$tmp/SHA256SUMS" "$hub/pkg/SHA256SUMS"
# Only the expected file may be checked; reject arbitrary paths in a manifest.
[ "$(wc -l < "$tmp/SHA256SUMS")" -eq 1 ] || { echo 'Invalid package checksum manifest.' >&2; exit 1; }
LC_ALL=C grep -Eq '^[0-9a-f]{64}  nestegg\.deb$' "$tmp/SHA256SUMS" || { echo 'Invalid package checksum manifest.' >&2; exit 1; }
(cd "$tmp" && sha256sum -c SHA256SUMS)
apt-get install -y "$tmp/nestegg.deb"
nestegg join "$hub" --pin "$pin" --code "$code" "$@"
