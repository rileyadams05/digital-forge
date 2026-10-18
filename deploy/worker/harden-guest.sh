#!/usr/bin/env bash
# Run only inside the dedicated VirtualBox guest, never on the Windows host.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo 'Root is required.' >&2; exit 1; }
[[ $(systemd-detect-virt) == oracle ]] || { echo 'Expected the VirtualBox guest.' >&2; exit 1; }
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
# The dedicated VM uses enp0s3. Do not apply this to arbitrary guest layouts.
ip link show enp0s3 >/dev/null
install -m 600 "$script_dir/99-iso2god-worker.yaml" /etc/netplan/99-iso2god-worker.yaml
netplan generate
netplan apply
apt-get install -y --no-install-recommends ufw
ufw default deny incoming
ufw default allow outgoing
ufw allow from 10.0.2.2 to any port 22 proto tcp
ufw allow from 10.0.2.2 to any port 36061 proto tcp
# Pairing may add an explicit console-only exception; never open the whole LAN.
if [[ -n ${ISO2GOD_PAIRED_XBOX:-} ]]; then
    python3 -c 'import ipaddress,sys; a=ipaddress.ip_address(sys.argv[1]); assert a.version == 4 and a.is_private and not a.is_loopback' "$ISO2GOD_PAIRED_XBOX"
    ufw insert 1 allow out to "$ISO2GOD_PAIRED_XBOX"
    ufw allow from "$ISO2GOD_PAIRED_XBOX" to any port 36061 proto tcp
fi
ufw allow out to 10.0.2.3 port 53
ufw allow out to 10.0.2.2 port 67 proto udp
for subnet in 10.0.0.0/8 172.16.0.0/12 192.168.0.0/16 169.254.0.0/16 fc00::/7 fe80::/10; do
    ufw deny out to "$subnet"
done
ufw --force enable
ufw status verbose
