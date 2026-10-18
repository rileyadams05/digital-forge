# Digital 360 Xbox Store

Native controller-driven Store frontend. Each user's PC runs the background service;
the PC menu contains Manual Import and Exit. Complete mandatory FTP setup on the PC and
it installs/updates `default.xex` and generates a private `worker.ini` for that user's LAN.
It starts the background service automatically. Reopen the PC application after restarting
the PC, and keep the PC awake while work runs. Allow TCP 36062 from your Xbox in the PC firewall.
No VM is required. The Xbox app is an application, not a DashLaunch plugin.

The installation path is `Hdd1:/Content/0000000000000000/ISO2GOD/`.
Scan in Aurora after first installation. Front/back artwork and icon assets are embedded.
Build with `build.ps1` and a locally installed Xbox SDK; the verified result is also copied
to `vendor/xbox/digital360.xex` for embedding in the PC release executable.

The pairing file contains `host`, `port` (36062 for the PC companion), and a private random
`token`. Never distribute a generated pairing file. The worker reads this user's saved FTP
settings and password store; it does not read or distribute the developer's credentials.

Install a background FTP plugin yourself to receive files while Digital 360 stays open. The companion
uses the existing port 7564 service but does not install plugins or edit DashLaunch.

The interface follows the PC terminal: dark background, monospaced text, orange banner
and selected row, with the Digital 360 name. The Xbox menu contains **Store** and
**Exit**. Manual Import is PC-only; there is no Downloads or FTP settings menu. Use
D-pad and A in place of the PC arrow keys and Enter, and B to return or exit.
Long messages wrap inside the TV safe area and lists page eight rows at a time.

The build embeds the original `docs/ICONS/Xbox 360/Front.png` and `Back.png` byte for
byte, Digital 360 title metadata (`D3600001`) with a 64x64 icon, a fitted front/back
cover, native Aurora cache assets, and the font directly into `default.xex`. The build extracts every resource
and compares its hash before declaring success. No loose artwork or font downloads
are needed. On launch, the app creates its font
cache, locates Aurora on the internal disk or USB (including DashLaunch's configured
path), and installs its own `GC` cover and `GL` icon assets into matching
`Data/GameData/D3600001_<ContentId>` cache directories. PNGs are also staged in the
import folder, but correct display no longer depends on Aurora importing them.
The old installed `FFED0707` app record was migrated to Digital 360 with a full
database backup and verification that every other game record remained unchanged.
Fresh installs should be scanned in Aurora before first launch. See assets/README.md
for reproducible cover conversion. The executable keeps its internal installation
folder for compatibility; its visible name is Digital 360.
Aurora owns the final dashboard artwork cache; it is separate from the XEX resource.

Search, choose a host listed on the source page, then choose a currently connected Xbox drive.
Only 1File/1Fichier is hidden because it requires sign-in. Other host choices are not
checked or hidden in advance; unavailable files fail when downloading.
Progress includes an orange percentage bar, transfer speed and the available time estimate.
MEGA public file/folder links run through the PC companion, including legacy shortened
links and multipart archives. No separate MEGA installation or account is required.
The companion decrypts and verifies each file before extraction; retry keeps verified
completed files and restarts the interrupted file.
Downloading, extraction and conversion start automatically. Progress
opens immediately; no separate Downloads screen or extra Start action is required.
Digital 360 stays on the progress screen during downloading, extraction, FTP upload
and verification. Return to main menu is offered after completion or failure; failed jobs
also offer Retry. The B button cannot leave an active install. FTP uses the installed
background service throughout; there is no dashboard acknowledgement or port switching.
Interrupted transfers reconnect to the same service and retain completed files.
Reopening Digital 360 restores progress. Extraction failures never advance to FTP.
The host PC and console must stay powered and connected.

There is no popup notification helper. The native system DLL and its loader calls
have been removed. Completion is shown when reopening Digital 360, with a reminder
to rescan the Xbox library. Failed jobs offer Retry and retain their prepared files.

Hardware validation on 2026-09-03 confirms the Digital 360 menu and actual front
cover in Aurora. A synthetic end-to-end delivery test verifies that upload remains
paused during the Exit handoff, waits through dashboard FTP downtime, then reconnects and
delivers byte-identical files. A broken archive fails before FTP.

`startup-status.txt` records flushed startup checkpoints and artwork/loader return
codes without credentials. The former cover-preview path was removed because it
called ATG SimpleShaders before initialization, dereferencing a null device pointer.
The original front/back images and dashboard icon remain embedded in the executable.

The existing background FTP service can provide initial drive discovery while the
Store is open; game delivery starts on it automatically, waits for the dashboard after exit and can then fail over
to the background endpoint during games. The retired
Guide and notification modules are not required.

The Xbox LAN socket option follows the [original Xbox networking example](https://github.com/mojobojo/PublicXboxStuff/blob/master/Applications/Xbox/AllocLogger/PluginDemo.cpp).
