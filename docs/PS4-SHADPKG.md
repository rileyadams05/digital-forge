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

## Crash investigation and capability check (22 October 2026)

Read-only inspection of this exact cached update established:

- Its metadata key (DK3) passes RSA PKCS#1 v1.5 decoding and its recorded digest.
- Its AES-unwrapped IMAGE_KEY fails RSA PKCS#1 v1.5 decoding with the fake-package
  key that ShadPKG uses. No valid filesystem key (EKPFS) was recovered.
- The standard zero passcode also fails the package's recorded key digest.

ShadPKG 2.1.0's `Crypto::RSA2048Decrypt` discards `DecodingResult` and copies the
output even after an invalid decode. Its `GetPFSCOffset` returns unsigned -1
when no PFSC header is found, then the caller uses that offset in an unchecked
`memcpy`. These are concrete memory-safety defects on the invalid-key path;
the actual package's failed key decode is independently confirmed. This was
not established by a debugger stack trace, so the precise faulting instruction
in the release binary is not asserted.

Digital Forge now reads only bounded key records before launching ShadPKG,
validates RSA padding, decoded lengths and stored key digests, and refuses to
start that parser when key recovery fails. The `orbis-pkg` crate supplies the
same published metadata and fake-package keysets. No private retail keys,
guessed passcodes or disabled integrity checks are introduced. This guard
prevents this invalid-key crash path; it does **not** decrypt the retail update.

Alternative backend evaluation:

- `Mohamed-Moslem-Allouch/ps4-pkg-unpacker`, commit
  `1fa6ecc0547ff2d24d7b5773b31a14866a924625`, was run against the same real PKG
  in an isolated research folder. It extracted 42 files, all under `sce_sys`,
  and no game payload. Its exit code was zero despite the missing payload.
  It explicitly requests a valid passcode for retail filesystem decryption.
  It was **not** integrated as a working replacement.
- `maxton/LibOrbisPkg`, commit `643477263b2644e0803e0f58b8726ea4e3f3b7d4`,
  was source-reviewed: `GetEkpfs` uses the same fake key, returning null when
  decryption fails; other paths require a matching passcode/key. Existing
  OrbisPkgTool had already produced metadata only on this update.
- Current ShadPKG source `7a02c1e56b40477b26a660884deda1f3ca8d2eab` still
  uses `FakeKeyset`, ignores the RSA decoding result and retains unchecked
  PFSC offset handling in its scan path. RIF-assisted extraction remains TODO.

No evaluated PC backend recovered the actual update filesystem with the
available inputs. A valid matching package key/passcode or an already decrypted
update is still required. The successful catalogue/download tests and synthetic
package tests must not be reported as successful real-update decryption.

## Itemzflow update path comparison

Inspection of `LightningMods/Itemzflow` (`main`, 22 October 2026) clarifies why
Itemzflow's console-side update flow differs from PC package extraction:

1. `itemzflow/source/http.cpp` (`Fetch_Update_Details`): queries `PATCH_API_URL + title_id`
   and parses `patches[]` entries (`version`, `size`, `endpoint`, optional `required_fw`).
   Public `include/secrets.h` redacts the private patch API endpoint (`ITS A SECRET`).
2. `itemzflow/source/GLES2_menu.cpp`: compares each patch's `required_fw` against
   `ps4_fw_version()` on the console; on PC, Digital Forge takes manual firmware input.
3. `itemzflow/source/GLES2_itemzflow.cpp`: explicitly rejects its retail update installer
   option for games flagged as FPKG, and passes selected retail patch endpoints to
   `installPatchPKG`.
4. `itemzflow/source/installpkg.cpp` (`installPatchPKG`): queues the retail PKG via PS4
   system services (`sceBgftServiceIntDebugDownloadRegisterPkg` and
   `sceBgftServiceDownloadStartTask`) for console-side installation. No PC filesystem
   decryption occurs in that path.
5. `itemzflow/source/sig_handler.cpp`: Itemzflow's separate console dumper (`GAME_PATCH`)
   operates on the PS4 after a patch is installed on compatible hardware. Digital Forge
   can package a complete console-produced patch dump folder on PC, but cannot reproduce
   console-side retail installation and dumping from the encrypted retail PKG alone.

Sources: [official release](https://github.com/seregonwar/ShadPKG/releases/tag/v2.1.0),
[extractor source](https://github.com/seregonwar/ShadPKG/blob/v2.1.0/core/file_format/pkg.cpp),
[CLI source](https://github.com/seregonwar/ShadPKG/blob/v2.1.0/main.cpp),
[Itemzflow source](https://github.com/LightningMods/Itemzflow).
