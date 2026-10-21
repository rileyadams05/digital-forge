Managed ShadPKG 2.1.0 - Windows x64

Official release: https://github.com/seregonwar/ShadPKG/releases/tag/v2.1.0
Asset: https://github.com/seregonwar/ShadPKG/releases/download/v2.1.0/shadpkg.exe
SHA256: 31d932ba5129ed50ecd4738dcf720b9c47881d1cb1ea41817651387446d90068
Upstream tag source: 0d97a08 (v2.1.0)
License: GNU Lesser General Public License (upstream LICENSE, version 2.1,
included unchanged). The upstream README labels it LGPL-2.0-or-later.
Corresponding tagged source and build instructions: source-v2.1.0.zip (included).
This is an unmodified upstream executable invoked as a separate process.

Digital Forge invokes: shadpkg.exe extract <package> <fresh-staging-output>
ShadPKG creates a Content-ID subfolder when SFO metadata is available.
The application checks title ID, content ID, category, exact version and actual
payload before publishing the prepared folder. Exit code 0 is not sufficient.
Original inputs and existing prepared folders are not overwritten.

Runtime dependencies: Windows 10/11 Universal CRT and the Microsoft Visual C++
x64 runtime (msvcp140.dll, msvcp140_atomic_wait.dll, vcruntime140.dll and
vcruntime140_1.dll). The Windows x64 release script copies these application-local
DLLs from the installed Visual Studio redistributable directory, not System32.
Microsoft runtime files retain Microsoft's license; they are not covered by the ShadPKG license.
Redistribution reference: https://learn.microsoft.com/cpp/windows/redistributing-visual-cpp-files

ShadPKG is used for extraction; OrbisPkgTool remains the metadata inspector,
GP4 generator, FPKG builder and package validator. No RIF generation is invoked.
The v2.1.0 source marks RIF-based extraction integration as TODO; passing a RIF
is not treated as a supported workaround for encrypted retail updates.
