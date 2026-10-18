# ISO2GOD self-hosted worker

This is the optional VM development deployment for the native Xbox frontend. Regular users
run the PC application on their own computer and save their own Xbox FTP details; they do
not need this deployment or access to the developer's PC. Start with the [PC guide](../../README.md).

This package runs the existing Store implementation as a silent service in the isolated
`ISO2GOD-Worker` virtual machine. It does not share
Windows folders, the clipboard, USB devices, audio, remote display, or the Windows desktop.

## Current scope

The service supports authenticated Store search, public-host validation, a persistent
download queue, extraction/conversion, FTP delivery, verification, and failed-job retry.
The native Xbox client is in `../../../consoles/Xbox/ISO2GOD/xbox/store`; its menu contains Store and Exit. Connection
setup is automatic or asks once, and progress opens automatically after a selection.
Manual Import remains on the PC. Guide integration is retired.
This deployment pairs one console on the home LAN. It is not a public Internet service
or a multi-tenant cloud deployment; HTTP/FTP must not be exposed on the router.

The VM has 2 virtual CPUs and 8 GiB of RAM. The worker service is additionally capped at
2 CPU equivalents and 6 GiB of RAM and uses low CPU and disk-I/O priority.

## Service endpoint

The worker listens on TCP 36061 inside the VM. Windows forwards localhost for private
administration and its Ethernet address for the paired Xbox. The Windows firewall rule
`ISO2GOD-Xbox-Worker` permits only that Xbox address. SSH remains loopback-only.

Available API calls:

- `GET /health`
- `POST /v1/store/search`
- `POST /v1/store/inspect`
- `POST /v1/console` (actions: connection, search, options, drives, queue, activity,
  leave, notifications, ack_notification, jobs, retry, credentials). `jobs` remains available
  for diagnostics; the Xbox has no Downloads menu. `activity` restores progress
  automatically. `leave` acknowledges the dashboard transition for queued/running
  Digital 360 jobs. They pause before FTP until that acknowledgement, then wait
  for dashboard FTP. The legacy notification actions remain API-compatible, but
  Digital 360 does not load a popup helper.

All `/v1/` calls require `Authorization: Bearer <token>`. The service refuses to start with
a token shorter than 32 characters. Request bodies are limited to 1 MiB.
Console requests cannot supply arbitrary URLs or FTP destinations. Queue submissions
reference server-inspected listings and drives freshly enumerated from the paired Xbox.
Jobs run in independent child processes and private working folders, not in the Xbox app.
Leaving the Xbox app after queue confirmation does not cancel a job. Keep both devices on.
The Xbox keyboard requests missing FTP details; Microsoft account passwords are not used.

## Guest installation

Build the Linux x64 release inside the VM, place it at `/tmp/iso2god`, copy this directory
to the VM, and run `sudo ./install.sh /tmp/iso2god`. The installer creates an unprivileged
service account, generates a private API token, and enables the hardened systemd unit.

Do not publish SSH, the VirtualBox remote-display service, or the aria2 RPC port. Only the
ISO2GOD API is intended to receive requests.

## Verification

`python3 tests/worker_smoke.py /path/to/iso2god` starts a temporary worker with a synthetic
local catalogue and tests health, authentication rejection, input validation, search,
inspection, unknown routes, and request-size limits without contacting a real download host.

`python3 tests/worker_delivery_smoke.py /path/to/iso2god /path/to/rclone` checks the complete
synthetic download/extraction/FTP path, byte-for-byte delivery, broken-archive failure,
request restrictions, and persistence across worker restart. It uses no game downloads.

## Local deployment and startup

The Windows task `ISO2GOD Worker` runs `start-worker.ps1` at Riley's sign-in. It starts
VirtualBox headlessly and exits if the VM is already running. It does not start before
Windows sign-in. Inside Ubuntu, systemd starts/restarts the worker automatically.

The guest administrator accepts SSH keys only; root SSH login is disabled. Private
provisioning credentials live outside the repository under `D:\Projects\iso2god\.vm`.
The API token is stored at `/etc/iso2god-worker.env` (root-only) and in the paired Xbox's
`worker.ini`. Treat the latter as a secret; never include it in public release archives.
Saved FTP settings are copied to `/var/lib/iso2god-worker/console.json` (worker-only, 0600).
The API never returns the password. Each job reads this private configuration.

`harden-guest.sh` is specific to this VirtualBox guest. Its firewall blocks outgoing
private-network access except an explicit rule for the paired Xbox. Incoming API traffic
is limited to the NAT gateway and paired Xbox; SSH is allowed only from the NAT gateway.
DNS uses public resolvers 1.1.1.1 and 9.9.9.9 so private router DNS is unnecessary.
Guest access to Windows loopback is disabled. No router port forwarding or public endpoint
has been created. Re-pair if DHCP changes the PC or console IP address.
Networking reference: [Oracle VirtualBox networking documentation](https://docs.oracle.com/en/virtualization/virtualbox/7.2/user/networkingdetails.html).

This has no cloud subscription or usage-credit dependency. Your own PC must remain on;
electricity, disk space, and your internet connection are still used.

## Background FTP

The supplied FTPdll 0.3 uses port 7564, its own default login, and `fHdd`/`fUsbN` mount
names rather than Aurora's `Hdd1`/`UsbN`. The transfer layer maps the same physical drive
when changing endpoints, including verification/repair reads. Dashboard credentials are
preserved separately. Transfers wait/retry while a title transition makes FTP unavailable.
No custom Guide module is installed or bundled in the executable.
