# AnyPS5 preparation tool

Digital Forge can invoke `pkg-to-anyps5` for a supported PS5 debug FIH package or an extracted native PS5 game folder. A Windows x64 executable built from the upstream source is available as `pkg-to-anyps5.exe` in this directory. Its source, Cargo manifest and GPL-3.0-or-later license are retained under `pkg-to-anyps5-source/`.

Upstream project and source revision used for this build:

https://github.com/awake-devel/pkg-to-anyps5

Commit: `2e9b12267840f9777f8d825335c4b4938160d9c4` (version 0.2.5).
The binary SHA-256 is recorded in `SHA256SUMS.txt`.

The application also accepts `DIGITAL_FORGE_PKG_TO_ANYPS5` or the packaged path `runtime/ps5/AnyPS5/pkg-to-anyps5.exe`. Keep the corresponding source and upstream GPL-3.0-or-later license with any copy you redistribute.

The tool must produce `source/` and `app0/`. Digital Forge validates the output structure and does not run AnyPS5's relinker. Retail encrypted packages are unsupported by this preparation route.
