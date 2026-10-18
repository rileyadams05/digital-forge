#!/usr/bin/env bash
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "Run this installer as root." >&2
  exit 1
fi

binary="${1:-/tmp/iso2god}"
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
if [[ ! -x "$binary" ]]; then
  echo "Linux ISO2GOD binary not found or not executable: $binary" >&2
  exit 1
fi

apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends aria2 ca-certificates p7zip-full

if ! id iso2god-worker >/dev/null 2>&1; then
  useradd --system --home-dir /var/lib/iso2god-worker --create-home --shell /usr/sbin/nologin iso2god-worker
fi

install -d -o root -g root -m 0755 /opt/iso2god-worker
install -d -o iso2god-worker -g iso2god-worker -m 0700 /var/lib/iso2god-worker
install -d -o iso2god-worker -g iso2god-worker -m 0700 /var/lib/iso2god-worker/config
install -o root -g root -m 0755 "$binary" /opt/iso2god-worker/iso2god
install -o root -g root -m 0644 "$script_dir/iso2god-worker.service" /etc/systemd/system/iso2god-worker.service

if [[ ! -f /etc/iso2god-worker.env ]]; then
  token="$(od -An -N48 -tx1 /dev/urandom | tr -d ' \n')"
  umask 077
  printf 'ISO2GOD_WORKER_TOKEN=%s\nISO2GOD_WORKER_BIND=0.0.0.0:36061\n' "$token" > /etc/iso2god-worker.env
fi

chmod 0600 /etc/iso2god-worker.env
if ! grep -q '^ISO2GOD_WORKER_CONSOLE=' /etc/iso2god-worker.env; then
  printf 'ISO2GOD_WORKER_CONSOLE=/var/lib/iso2god-worker/console.json\n' >> /etc/iso2god-worker.env
fi
systemctl daemon-reload
systemctl enable iso2god-worker.service
systemctl restart iso2god-worker.service
systemctl --no-pager --full status iso2god-worker.service
