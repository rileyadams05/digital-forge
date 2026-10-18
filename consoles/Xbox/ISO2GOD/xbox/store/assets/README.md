# Digital 360 artwork

These native Aurora assets were generated from the revised Front.png and
Back.png under docs/ICONS/Xbox 360. They contain the complete edge-to-edge images, with
the back on the left and front on the right of the 900x600 cover. Icon is 64x64.
Both are embedded in default.xex and installed by Digital 360 itself. Users do
not need to copy or select image files.

Conversion uses XboxUnity's official AuroraAssetEditor code and conversion DLL,
revision `5de15c860b861d2534f40a3634d831835e39b40a`:
https://github.com/XboxUnity/AuroraAssetEditor/tree/5de15c860b861d2534f40a3634d831835e39b40a

To regenerate, obtain `Classes/AuroraAsset.cs`, `Classes/AuroraAssetDll.cs`,
`AuroraAsset.dll`, and `msvcr100.dll` from the project's AuroraAssetEditor folder
into a local tools directory. Run store/convert-assets.ps1 with `-ToolDirectory`
using 32-bit Windows PowerShell. The external DLLs are build tools only and are
not included in the Xbox executable. The converter decodes each generated asset
to verify it and records the original image hashes. build.ps1 rejects stale assets
when either original image changes.
