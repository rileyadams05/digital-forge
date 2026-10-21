# Managed PS4 extraction

Digital Forge uses the unmodified Windows x64 ShadPKG 2.1.0 release for base,
update and DLC PKG extraction. OrbisPkgTool remains the metadata inspector,
GP4 generator, FPKG builder and package validator.

The executable is pinned to the official release SHA256:
`31d932ba5129ed50ecd4738dcf720b9c47881d1cb1ea41817651387446d90068`.
The managed directory includes the upstream license, tagged source archive and
runtime documentation. Windows releases include the app-local Visual C++ DLLs
from Visual Studio's redistributable directory.

## Workflow and validation

After manual firmware entry and game identification, Digital Forge queries the
existing update provider and presents the available compatible versions. Sony's
feed can contain only one version; this integration does not create historical
records. A selected update is downloaded or reverified in the existing cache,
then passed to ShadPKG before the physical-console/emulator selection.

Extraction uses a fresh staging directory. The source PKG is preserved and
existing prepared folders are not overwritten. Digital Forge checks the exact
title ID, content ID, category and application version in extracted metadata.
Base/update payloads must contain actual non-empty game files; package metadata
alone is rejected even when ShadPKG exits successfully. A positively identified
no-data add-on is treated separately. Cancellation, timeout, a crash or failed
validation removes the private partial extraction. Diagnostics are retained in
Digital Forge's operation log. Failure offers retry, explicit base-only or cancel.

Prepared base-game and DLC folders remain supported, including the existing
Image0/Sc0 layout. Base metadata supplies the game title; DLC metadata never
replaces it. Itemzflow dump validation and advisories remain available.

## Verified capabilities and remaining limitation

Synthetic integration tests create base, update and DLC packages with the
managed OrbisPkgTool, extract them with the real ShadPKG binary and check payload
and metadata. Existing base/update merge and DLC FPKG tests remain active.

A real application run against Need for Speed Payback EUR, CUSA05986, firmware
9.00, selected Sony update 01.10 (minimum firmware 5.55). The complete
7,810,449,408-byte cached download passed the existing integrity verification.
ShadPKG then crashed with Windows access violation `0xC0000005` using the system
Visual C++ runtime, and heap corruption `0xC0000374` using the packaged runtime.
Both failures occurred through Digital Forge's actual interface. No usable
decrypted update payload or real-game update FPKG was produced. The verified
download was retained. This integration does **not** fix retail-update decryption.

The upstream v2.1.0 source has unchecked PFS/PFSC parsing after key recovery;
the crash alone does not establish which cryptographic or parser assumption
failed. Its RIF-assisted extraction path is marked TODO, so passing a license
file is not represented as a tested workaround. No TLS or integrity check is
disabled to work around extraction failures.

Sources: [official release](https://github.com/seregonwar/ShadPKG/releases/tag/v2.1.0),
[extractor source](https://github.com/seregonwar/ShadPKG/blob/v2.1.0/core/file_format/pkg.cpp),
[CLI source](https://github.com/seregonwar/ShadPKG/blob/v2.1.0/main.cpp).
