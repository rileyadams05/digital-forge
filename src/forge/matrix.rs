use serde::Serialize;

use super::format::{SourceFormat, TargetFormat};
use super::platform::Platform;
use super::profile::TargetProfile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SupportStatus {
    Supported,
    PassThrough,
    ValidationOnly,
    Unsupported,
}

impl SupportStatus {
    pub fn selectable(self) -> bool {
        matches!(self, Self::Supported | Self::PassThrough)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FormatConversionRule {
    pub platform: Platform,
    pub source_format: SourceFormat,
    pub target_format: TargetFormat,
    pub backend_id: &'static str,
    pub operation: &'static str,
    pub support_status: SupportStatus,
    pub recommended: bool,
    pub lossless_or_transforming: &'static str,
    pub requires_external_tool: bool,
    pub requires_key_or_license_input: bool,
    pub notes: &'static str,
    pub recommended_for: &'static [TargetProfile],
    pub is_passthrough: bool,
    pub requirements: Option<&'static str>,
}

macro_rules! rule {
    ($platform:ident, $source:ident => $target:ident, $backend:literal, $operation:literal,
     $status:ident, $recommended:expr, $kind:literal, $external:expr, $key:expr, $notes:literal) => {
        FormatConversionRule {
            platform: Platform::$platform,
            source_format: SourceFormat::$source,
            target_format: TargetFormat::$target,
            backend_id: $backend,
            operation: $operation,
            support_status: SupportStatus::$status,
            recommended: $recommended,
            lossless_or_transforming: $kind,
            requires_external_tool: $external,
            requires_key_or_license_input: $key,
            notes: $notes,
            recommended_for: &[],
            is_passthrough: matches!(SupportStatus::$status, SupportStatus::PassThrough),
            requirements: if $notes.is_empty() {
                None
            } else {
                Some($notes)
            },
        }
    };
}

macro_rules! pass {
    ($platform:ident, $source:ident) => {
        rule!($platform, $source => Passthrough, "native_passthrough", "validate and copy",
            PassThrough, true, "lossless", false, false, "Native console format")
    };
}

pub struct FormatMatrix;

impl FormatMatrix {
    pub fn get_rules_for_source(
        platform: Platform,
        source: SourceFormat,
    ) -> Vec<&'static FormatConversionRule> {
        ALL_RULES
            .iter()
            .filter(|rule| {
                rule.platform == platform
                    && rule.source_format == source
                    && rule.support_status.selectable()
            })
            .collect()
    }

    pub fn find_rule(
        platform: Platform,
        source: SourceFormat,
        target: TargetFormat,
    ) -> Option<&'static FormatConversionRule> {
        ALL_RULES.iter().find(|rule| {
            rule.platform == platform
                && rule.source_format == source
                && rule.target_format == target
        })
    }

    pub fn is_recommended(rule: &FormatConversionRule, _profile: TargetProfile) -> bool {
        rule.recommended
    }
}

pub static ALL_RULES: &[FormatConversionRule] = &[
    rule!(PlayStation1, BinCue => Pbp, "ps1_psxpackager", "PSXPackager/Popstation pack", Supported, true, "transforming", true, false, "CUE and referenced tracks are validated"),
    rule!(PlayStation1, Pbp => BinCue, "ps1_psxpackager", "PSXPackager/Popstation extract", Supported, true, "transforming", true, false, "PS1 Classics DATA.PSAR PBP only"),
    rule!(PlayStation1, BinCue => Chd, "ps1_chdman", "chdman createcd", Supported, false, "lossless", true, false, "Preserves the CUE track model"),
    rule!(PlayStation1, Chd => BinCue, "ps1_chdman", "chdman extractcd", Supported, false, "lossless", true, false, "Preserves every chdman output"),
    rule!(PlayStation1, BinCue => BinCue, "passthrough_ps1", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(PlayStation1, Pbp => Pbp, "passthrough_ps1", "validate and copy", PassThrough, false, "lossless", false, false, "PS1 Classics PBP only"),
    rule!(PlayStation1, Chd => Chd, "passthrough_ps1", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(PlayStation2, DiscIso => Ps2Cso, "ps2_maxcso", "maxcso --format=cso1", Supported, false, "lossless compression", true, false, "CSO v1"),
    rule!(PlayStation2, DiscIso => Ps2Cso2, "ps2_maxcso", "maxcso --format=cso2", Supported, false, "lossless compression", true, false, "CSO v2"),
    rule!(PlayStation2, DiscIso => Ps2Zso, "ps2_maxcso", "maxcso --format=zso", Supported, true, "lossless compression", true, false, "ZSO"),
    rule!(PlayStation2, DiscIso => Ps2Dax, "ps2_maxcso", "maxcso --format=dax", Supported, false, "lossless compression", true, false, "DAX"),
    rule!(PlayStation2, Cso => Ps2Iso, "ps2_maxcso", "maxcso --decompress", Supported, true, "lossless", true, false, "All MaxCSO inputs"),
    rule!(PlayStation2, Cso2 => Ps2Iso, "ps2_maxcso", "maxcso --decompress", Supported, true, "lossless", true, false, "All MaxCSO inputs"),
    rule!(PlayStation2, Zso => Ps2Iso, "ps2_maxcso", "maxcso --decompress", Supported, true, "lossless", true, false, "All MaxCSO inputs"),
    rule!(PlayStation2, Dax => Ps2Iso, "ps2_maxcso", "maxcso --decompress", Supported, true, "lossless", true, false, "All MaxCSO inputs"),
    rule!(PlayStation2, DiscIso => Ps2Chd, "ps2_chdman", "chdman createdvd", Supported, false, "lossless compression", true, false, "DVD only; CD titles use CD semantics"),
    rule!(PlayStation2, Chd => Ps2Iso, "ps2_chdman", "chdman extractdvd", Supported, false, "lossless", true, false, "DVD CHD only"),
    rule!(PlayStation2, DiscIso => Ps2UsbExtreme, "ps2_usbextreme", "PS2IsoManager-compatible ul.* writer", Supported, false, "lossless split", false, false, "1 GiB chunks and 64-byte ul.cfg records"),
    rule!(PlayStation2, DiscIso => Ps2Iso, "passthrough_ps2", "validate and copy", PassThrough, true, "lossless", false, false, "No conversion"),
    rule!(PlayStation2, Ps2UsbExtreme => Ps2Iso, "validation_ps2", "unsupported reverse direction", Unsupported, false, "n/a", false, false, "No proven reverse engine is integrated"),
    rule!(PlayStation2, Ps2ClassicsEnc => Passthrough, "validation_ps2", "recognise only", ValidationOnly, false, "n/a", false, true, "Keys and a verified engine are not integrated"),
    rule!(PlayStation3, Ps3JbFolder => Ps3Pkg, "ps3_pkg", "Aldostools PKG builder integration pending", Unsupported, false, "n/a", true, true, "PS3DK was retired; no verified replacement package builder is installed"),
    rule!(PlayStation3, Ps3Iso => Ps3Pkg, "ps3_pkg", "Aldostools PKG builder integration pending", Unsupported, false, "n/a", true, true, "PS3DK was retired; no verified replacement package builder is installed"),
    rule!(PlayStation3, Ps3SplitIso => Ps3Pkg, "ps3_pkg", "Aldostools PKG builder integration pending", Unsupported, false, "n/a", true, true, "PS3DK was retired; no verified replacement package builder is installed"),
    rule!(PlayStation3, Ps3Iso => Ps3JbFolder, "ps3_iso", "extractps3iso", Supported, true, "lossless extraction", true, false, "Uses the bundled PS3 ISO utilities or DIGITAL_FORGE_TOOL_PS3_EXTRACT_ISO override"),
    rule!(PlayStation3, Ps3SplitIso => Ps3JbFolder, "ps3_iso", "reassemble then extractps3iso", Supported, false, "lossless extraction", true, false, "Reassembles contiguous split parts, then uses the bundled PS3 ISO utilities"),
    rule!(PlayStation3, Ps3JbFolder => Ps3Iso, "ps3_iso", "makeps3iso", Supported, false, "disc image creation", true, false, "Uses the bundled PS3 ISO utilities or DIGITAL_FORGE_TOOL_PS3_GENERATE_ISO override"),
    rule!(PlayStation3, Ps3Pkg => Ps3Pkg, "ps3_pkg", "validate and copy", PassThrough, true, "lossless", false, false, "Existing PKG is retained after structural validation"),
    rule!(PlayStation4, Ps4SplitPkg => Ps4Folder, "ps4_pkg_merge", "pkg-merge + extract", Supported, true, "reassembly & extraction", true, false, "Reassembles and extracts to game folder"),
    rule!(PlayStation4, Ps4SplitPkg => Ps4FakePkg, "ps4_pkg_merge", "pkg-merge", Supported, false, "lossless reassembly", true, false, "Output validated with PkgTool"),
    rule!(PlayStation4, Ps4SplitPkg => Ps4RetailPkg, "ps4_pkg_merge", "pkg-merge", Supported, false, "lossless reassembly", true, false, "Output validated with PkgTool"),
    rule!(PlayStation4, Ps4FakePkg => Ps4Folder, "ps4_liborbispkg", "OrbisPkgTool extraction", Supported, true, "extraction", true, false, "Extracts package/PFS data to a loose game folder with dynamic Title ID; SELF/PRX containers remain PS4-native"),
    rule!(PlayStation4, Ps4FakePkg => Ps4FakePkg, "ps4_liborbispkg", "Built-in PS4 FPKG validation", PassThrough, false, "lossless", false, false, "Validated direct transfer"),
    rule!(PlayStation4, Ps4FakePkg => Ps4RetailPkg, "ps4_liborbispkg", "Sony retail signing unavailable", Unsupported, false, "n/a", false, false, "An FPKG cannot be relabelled or signed as a retail PKG"),
    rule!(PlayStation4, Ps4RetailPkg => Ps4Folder, "ps4_liborbispkg", "retail extraction unavailable", Unsupported, false, "n/a", true, true, "The PS4 workflow accepts already fake signed packages"),
    rule!(PlayStation4, Ps4RetailPkg => Ps4FakePkg, "ps4_liborbispkg", "retail decryption unavailable", Unsupported, false, "n/a", true, true, "Supply a matching, already fake signed base and update FPKG"),
    rule!(PlayStation4, Ps4RetailPkg => Ps4RetailPkg, "ps4_liborbispkg", "Built-in PS4 Retail PKG validation", PassThrough, false, "lossless", false, false, "Validated direct transfer"),
    rule!(PlayStation4, Ps4Folder => Ps4Folder, "passthrough_ps4", "validate and copy", PassThrough, true, "lossless", false, false, "Preserves the extracted/loose PS4 game directory layout"),
    rule!(PlayStation4, Ps4Folder => Ps4FakePkg, "ps4_liborbispkg", "Generate GP4, then build with CyB1K orbis-pub-cmd", Supported, false, "FPKG creation", true, false, "Builds a base-game or update FPKG from a validated extracted folder"),
    rule!(PlayStation4, Ps4Folder => Ps4RetailPkg, "ps4_liborbispkg", "Sony retail signing unavailable", Unsupported, false, "n/a", false, false, "Official retail PKG creation requires Sony's private signing service"),
    rule!(PlayStation5, Ps5LooseFolder => Ps5ExfatImage, "ps5_mkpfs", "mkpfs pack exfat", Unsupported, false, "n/a", true, false, "No Digital Forge conversion route is configured for this ShadowMount image"),
    rule!(PlayStation5, ShadowMountExfatImage => Ps5LooseFolder, "ps5_mkpfs", "mkpfs unpack --format exfat", Unsupported, false, "n/a", true, false, "No Digital Forge conversion route is configured for this ShadowMount image"),
    rule!(PlayStation5, Ps5LooseFolder => Ps5Ffpfsc, "ps5_mkpfs", "mkpfs pack folder --verify", Unsupported, false, "n/a", true, false, "No Digital Forge conversion route is configured for this ShadowMount image"),
    rule!(PlayStation5, ShadowMountExfatImage => Ps5Ffpfsc, "ps5_mkpfs", "mkpfs pack file", Unsupported, false, "n/a", true, false, "No Digital Forge conversion route is configured for this ShadowMount image"),
    rule!(PlayStation5, ShadowMountFfPkg => Ps5Ffpfsc, "ps5_mkpfs", "mkpfs pack file", Unsupported, false, "n/a", true, false, "No Digital Forge conversion route is configured for this ShadowMount image"),
    rule!(PlayStation5, ShadowMountCompressedPfs => Ps5LooseFolder, "ps5_mkpfs", "mkpfs unpack --deep", Unsupported, false, "n/a", true, false, "No Digital Forge conversion route is configured for this ShadowMount image"),
    rule!(PlayStation5, Ps5LooseFolder => Ps5Ffpfs, "ps5_mkpfs", "mkpfs pack folder --raw", Unsupported, false, "n/a", true, false, "No Digital Forge conversion route is configured for this ShadowMount image"),
    rule!(PlayStation5, Ps5LooseFolder => Ps5LooseFolder, "native_passthrough", "validate native folder for ItemzFlow", PassThrough, true, "lossless", false, false, "Requires an already prepared PS5 folder with eboot.bin, matching param.json/param.sfo, and required executable preparation; Digital Forge does not dump or fake-sign game files"),
    rule!(PlayStation5, Ps5DumpTar => Ps5LooseFolder, "native_passthrough", "safe TAR extraction and ItemzFlow folder validation", Supported, true, "lossless", false, false, "Accepts plain TAR only; requires an already prepared native eboot.bin, sce_module, matching param.json/param.sfo. Digital Forge does not transform or fake-sign executables"),
    rule!(PlayStation5, Ps5Pkg => Ps5AnyPS5Output, "ps5_anyps5", "pkg-to-anyps5 debug PKG preparation", Supported, true, "transforming", true, false, "Debug FIH PKG only; retail encrypted PKGs are rejected by the tool"),
    rule!(PlayStation5, Ps5LooseFolder => Ps5AnyPS5Output, "ps5_anyps5", "pkg-to-anyps5 folder preparation", Supported, true, "transforming", true, false, "Produces source/ and app0/ for the AnyPS5 relinker; not a runnable output by itself"),
    rule!(PlayStation5, Ps5PkgManagerMultipart => Ps5Pkg, "ps5_multipart", "PS5MPKG1 reassembly", Unsupported, false, "n/a", false, false, "Multipart package install is not part of the configured PS5 workflows"),
    rule!(PlayStation5, Ps5RawSplitPkg => Ps5Pkg, "ps5_raw_split", "raw byte reassembly", Unsupported, false, "n/a", false, false, "Split package install is not part of the configured PS5 workflows"),
    rule!(PlayStation5, Ps5Pkg => Ps5Pkg, "passthrough_ps5", "validate and copy", Unsupported, false, "n/a", false, false, "Only the ItemzFlow folder and AnyPS5 preparation routes are configured"),
    rule!(PlayStation5, Ps5SplitBackup => Ps5Pkg, "validation_ps5", "unsupported backup representation", Unsupported, false, "n/a", false, false, "No proven app.json/app_N builder"),
    rule!(Psp, DiscIso => PspCso, "psp_maxcso", "maxcso --format=cso1", Supported, false, "lossless compression", true, false, "CSO v1"),
    rule!(Psp, DiscIso => PspCso2, "psp_maxcso", "maxcso --format=cso2", Supported, false, "lossless compression", true, false, "CSO v2"),
    rule!(Psp, DiscIso => PspZso, "psp_maxcso", "maxcso --format=zso", Supported, true, "lossless compression", true, false, "ZSO"),
    rule!(Psp, DiscIso => PspDax, "psp_maxcso", "maxcso --format=dax", Supported, false, "lossless compression", true, false, "DAX"),
    rule!(Psp, Cso => PspIso, "psp_maxcso", "maxcso --decompress", Supported, true, "lossless", true, false, "All MaxCSO inputs"),
    rule!(Psp, Cso2 => PspIso, "psp_maxcso", "maxcso --decompress", Supported, true, "lossless", true, false, "All MaxCSO inputs"),
    rule!(Psp, Zso => PspIso, "psp_maxcso", "maxcso --decompress", Supported, true, "lossless", true, false, "All MaxCSO inputs"),
    rule!(Psp, Dax => PspIso, "psp_maxcso", "maxcso --decompress", Supported, true, "lossless", true, false, "All MaxCSO inputs"),
    rule!(Psp, DiscIso => PspIso, "passthrough_psp", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(Psp, Pbp => PspPbp, "passthrough_psp", "validate and copy", PassThrough, true, "lossless", false, false, "PSP PBP is pass-through only"),
    rule!(Psp, Jso => Passthrough, "validation_psp", "recognise only", ValidationOnly, false, "n/a", false, false, "MaxCSO does not support JSO"),
    rule!(PlayStationVita, VitaPkg => VitaZip, "vita_pkg2zip", "pkg2zip package.pkg [zRIF]", Supported, true, "extraction", true, false, "ZIP is the default output; optional zRIF creates work.bin"),
    rule!(PlayStationVita, VitaPkg => VitaNoNpDrmFolder, "vita_pkg2zip", "pkg2zip -x package.pkg [zRIF]", Supported, true, "extraction", true, false, "Optional zRIF; reports license absence"),
    rule!(PlayStationVita, VitaVpk => VitaNoNpDrmFolder, "vita_vpk", "ZIP extraction", Supported, true, "lossless extraction", false, false, "Validates Vita structure"),
    rule!(OriginalXbox, XboxOriginalFolder => XboxOriginalXiso, "xbox_extract_xiso", "extract-xiso -c", Supported, true, "filesystem image", true, false, "XboxDev/extract-xiso"),
    rule!(OriginalXbox, XboxOriginalIso => XboxOriginalFolder, "xbox_extract_xiso", "extract-xiso -x -d", Supported, true, "lossless extraction", true, false, "XboxDev/extract-xiso"),
    rule!(OriginalXbox, XboxOriginalIso => XboxOriginalTrimmedXiso, "xbox_extract_xiso", "extract-xiso rewrite", Supported, false, "filesystem rewrite", true, false, "Upstream rewrite mode"),
    rule!(OriginalXbox, XboxOriginalIso => XboxOriginalXiso, "passthrough_xbox", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(OriginalXbox, XboxOriginalCci => Passthrough, "validation_xbox", "recognise only", ValidationOnly, false, "n/a", false, false, "CCI engine not integrated"),
    rule!(Xbox360, Xbox360Iso => Xbox360God, "xbox360_iso2god", "Iso2God", Supported, true, "container transform", true, false, "Source deletion disabled"),
    rule!(Xbox360, Xbox360Iso => Xbox360Iso, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "Xenia accepts Xbox 360 disc images directly"),
    rule!(Xbox360, Xbox360God => Xbox360God, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Xenia accepts Games on Demand containers directly"),
    rule!(Xbox360, Xbox360Folder => Xbox360Folder, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Xenia launches extracted games through default.xex"),
    rule!(Xbox360, Xbox360God => Xbox360Iso, "xbox360_god2iso", "god2iso", Supported, true, "container transform", true, false, "raburton/god2iso"),
    rule!(Xbox360, Xbox360Iso => Xbox360Folder, "xbox360_extract_xiso", "extract-xiso -x -d", Supported, false, "lossless extraction", true, false, "Xbox 360/XGD3 extraction"),
    rule!(Xbox360, Xbox360Iso => Xbox360SplitIso, "xbox360_split", "Digital Forge fixed-size split", Supported, false, "lossless split", false, false, "Deterministic contiguous parts"),
    rule!(Xbox360, Xbox360SplitIso => Xbox360Iso, "xbox360_split", "Digital Forge byte reassembly", Supported, true, "lossless", false, false, "SHA-256 round-trip tested"),
    rule!(Xbox360, Xbox360LiveContainer => Passthrough, "validation_stfs", "CON/LIVE/PIRS validation", ValidationOnly, false, "n/a", false, false, "Extraction disabled until audited XboxInternals port exists"),
    // Nintendo cartridge and handheld formats are validation + pass-through.
    pass!(NintendoEntertainmentSystem, NesRom),
    pass!(FamicomDiskSystem, FdsImage),
    pass!(SuperNintendo, SnesRom),
    pass!(Satellaview, SatellaviewRom),
    pass!(SufamiTurbo, SufamiTurboRom),
    rule!(Nintendo64, N64V64 => N64Z64, "n64_normalize", "adjacent-byte swap", Supported, true, "lossless byte-order transform", false, false, "Canonical big-endian ROM"),
    rule!(Nintendo64, N64N64 => N64Z64, "n64_normalize", "reverse each four-byte word", Supported, true, "lossless byte-order transform", false, false, "Canonical big-endian ROM"),
    rule!(Nintendo64, N64Z64 => N64Z64, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already canonical"),
    pass!(Nintendo64DD, N64DdImage),
    pass!(GameBoy, GameBoyRom),
    pass!(GameBoyColor, GameBoyColorRom),
    pass!(GameBoyAdvance, GameBoyAdvanceRom),
    pass!(VirtualBoy, VirtualBoyRom),
    pass!(NintendoDS, NintendoDsRom),
    pass!(NintendoDSi, NintendoDsiRom),
    pass!(NintendoDSi, NintendoDsRom),
    pass!(Nintendo3DS, Nintendo3dsCci),
    pass!(Nintendo3DS, Nintendo3dsCxi),
    pass!(Nintendo3DS, Nintendo3ds3dsx),
    pass!(PokemonMini, PokemonMiniRom),
    // DolphinTool is only a standalone disc-image converter here.
    rule!(GameCube, GameCubeIso => NintendoRvz, "nintendo_gc_wii", "DolphinTool convert --format rvz", Supported, true, "lossless compression", true, false, "Zstandard level 5, 128 KiB blocks"),
    rule!(GameCube, GameCubeGcm => NintendoRvz, "nintendo_gc_wii", "DolphinTool convert --format rvz", Supported, true, "lossless compression", true, false, "GCM is a plain GameCube disc image"),
    rule!(GameCube, GameCubeGcz => NintendoRvz, "nintendo_gc_wii", "DolphinTool convert --format rvz", Supported, true, "lossless transcode", true, false, "Zstandard level 5, 128 KiB blocks"),
    rule!(GameCube, NintendoWia => NintendoRvz, "nintendo_gc_wii", "DolphinTool convert --format rvz", Supported, true, "lossless transcode", true, false, "Zstandard level 5, 128 KiB blocks"),
    rule!(GameCube, NintendoRvz => NintendoDiscIso, "nintendo_gc_wii", "DolphinTool convert --format iso", Supported, false, "lossless extraction", true, false, "Plain disc image"),
    rule!(GameCube, NintendoWia => NintendoDiscIso, "nintendo_gc_wii", "DolphinTool convert --format iso", Supported, false, "lossless extraction", true, false, "Plain disc image"),
    rule!(GameCube, GameCubeGcz => NintendoDiscIso, "nintendo_gc_wii", "DolphinTool convert --format iso", Supported, false, "lossless extraction", true, false, "Plain disc image"),
    rule!(GameCube, GameCubeIso => NintendoDiscIso, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(GameCube, NintendoRvz => NintendoRvz, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    rule!(Wii, WiiIso => NintendoRvz, "nintendo_gc_wii", "DolphinTool convert --format rvz", Supported, true, "lossless compression", true, false, "Zstandard level 5, 128 KiB blocks"),
    rule!(Wii, WiiWbfs => NintendoRvz, "nintendo_gc_wii", "DolphinTool convert --format rvz", Supported, true, "container transcode", true, false, "Input support provided by DiscIO"),
    rule!(Wii, GameCubeGcz => NintendoRvz, "nintendo_gc_wii", "DolphinTool convert --format rvz", Supported, true, "lossless transcode", true, false, "Zstandard level 5, 128 KiB blocks"),
    rule!(Wii, NintendoWia => NintendoRvz, "nintendo_gc_wii", "DolphinTool convert --format rvz", Supported, true, "lossless transcode", true, false, "Zstandard level 5, 128 KiB blocks"),
    rule!(Wii, NintendoRvz => NintendoDiscIso, "nintendo_gc_wii", "DolphinTool convert --format iso", Supported, false, "lossless extraction", true, false, "Plain disc image"),
    rule!(Wii, NintendoWia => NintendoDiscIso, "nintendo_gc_wii", "DolphinTool convert --format iso", Supported, false, "lossless extraction", true, false, "Plain disc image"),
    rule!(Wii, GameCubeGcz => NintendoDiscIso, "nintendo_gc_wii", "DolphinTool convert --format iso", Supported, false, "lossless extraction", true, false, "Plain disc image"),
    rule!(Wii, WiiIso => NintendoDiscIso, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(Wii, NintendoRvz => NintendoRvz, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    // Sega cartridge families.
    pass!(SegaSG1000, SegaSgRom),
    pass!(SegaMasterSystem, SegaSmsRom),
    pass!(SegaGameGear, SegaGameGearRom),
    pass!(SegaMegaDrive, SegaMdBinRom),
    pass!(SegaMegaDrive, SegaSmdRom),
    pass!(Sega32X, Sega32xRom),
    pass!(SegaPico, SegaMdBinRom),
    // CD systems supported by chdman's audited createcd/extractcd path.
    rule!(SegaCD, BinCue => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "Preserves every track"),
    rule!(SegaCD, Chd => BinCue, "optical_chdman", "chdman extractcd", Supported, false, "lossless extraction", true, false, "CD CHD extraction"),
    rule!(SegaCD, BinCue => BinCue, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(SegaCD, Chd => Chd, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    pass!(SegaCD, DiscIso),
    pass!(SegaCD, DiscPlaylist),
    rule!(SegaSaturn, BinCue => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "Preserves every track"),
    rule!(SegaSaturn, Chd => BinCue, "optical_chdman", "chdman extractcd", Supported, false, "lossless extraction", true, false, "CD CHD extraction"),
    rule!(SegaSaturn, BinCue => BinCue, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(SegaSaturn, Chd => Chd, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    pass!(SegaSaturn, DiscCcd),
    pass!(SegaSaturn, DiscPlaylist),
    rule!(PcEngineCD, BinCue => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "Preserves every track"),
    rule!(PcEngineCD, Chd => BinCue, "optical_chdman", "chdman extractcd", Supported, false, "lossless extraction", true, false, "CD CHD extraction"),
    rule!(PcEngineCD, BinCue => BinCue, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(PcEngineCD, Chd => Chd, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    pass!(PcEngineCD, DiscIso),
    pass!(PcEngineCD, DiscImg),
    pass!(PcEngineCD, DiscCcd),
    pass!(PcEngineCD, DiscPlaylist),
    rule!(PcFx, BinCue => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "Preserves every track"),
    rule!(PcFx, Chd => BinCue, "optical_chdman", "chdman extractcd", Supported, false, "lossless extraction", true, false, "CD CHD extraction"),
    rule!(PcFx, BinCue => BinCue, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(PcFx, Chd => Chd, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    pass!(PcFx, DiscCcd),
    pass!(PcFx, DiscToc),
    rule!(ThreeDO, BinCue => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "Preserves every track"),
    rule!(ThreeDO, Chd => BinCue, "optical_chdman", "chdman extractcd", Supported, false, "lossless extraction", true, false, "CD CHD extraction"),
    rule!(ThreeDO, BinCue => BinCue, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(ThreeDO, Chd => Chd, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    pass!(ThreeDO, DiscIso),
    rule!(NeoGeoCD, BinCue => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "Preserves every track"),
    rule!(NeoGeoCD, Chd => BinCue, "optical_chdman", "chdman extractcd", Supported, false, "lossless extraction", true, false, "CD CHD extraction"),
    rule!(NeoGeoCD, BinCue => BinCue, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(NeoGeoCD, Chd => Chd, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    rule!(SegaDreamcast, Gdi => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "GDI and referenced tracks"),
    rule!(SegaDreamcast, BinCue => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "CUE and referenced tracks"),
    rule!(SegaDreamcast, Chd => Gdi, "optical_chdman", "chdman extractcd GDI", Supported, false, "lossless extraction", true, false, "Extracts GDI descriptor and tracks"),
    pass!(SegaDreamcast, Gdi),
    pass!(SegaDreamcast, BinCue),
    pass!(SegaDreamcast, Cdi),
    rule!(SegaDreamcast, Chd => Chd, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    // NEC/SNK/Atari/Bandai and other native ROM formats.
    pass!(PcEngine, PcEngineRom),
    pass!(SuperGrafx, SuperGrafxRom),
    pass!(SuperGrafx, PcEngineRom),
    pass!(NeoGeoAESMVS, NeoGeoRom),
    pass!(NeoGeoPocket, NeoGeoPocketRom),
    pass!(NeoGeoPocketColor, NeoGeoPocketColorRom),
    pass!(Atari2600, Atari2600Rom),
    pass!(Atari5200, Atari5200Rom),
    pass!(Atari7800, Atari7800Rom),
    pass!(AtariLynx, AtariLynxRom),
    pass!(AtariJaguar, AtariJaguarRom),
    pass!(AtariJaguarCD, BinCue),
    pass!(AtariJaguarCD, Cdi),
    pass!(WonderSwan, WonderSwanRom),
    pass!(WonderSwanColor, WonderSwanColorRom),
    pass!(ColecoVision, ColecoRom),
    pass!(Intellivision, IntellivisionRom),
    pass!(Odyssey2Videopac, Odyssey2Rom),
    pass!(Vectrex, VectrexRom),
    pass!(FairchildChannelF, FairchildChannelFRom),
    pass!(WataraSupervision, WataraSupervisionRom),
    pass!(MegaDuckCougarBoy, MegaDuckRom),
    // These archives are game containers, never transport archives.
    pass!(SegaNaomi, NaomiRomSet),
    pass!(SegaNaomi, Chd),
    pass!(SegaNaomi2, Naomi2RomSet),
    pass!(SegaNaomi2, Chd),
    pass!(SegaAtomiswave, AtomiswaveRomSet),
    pass!(SegaAtomiswave, Chd),
    pass!(SegaSystemSP, SystemSpRomSet),
    pass!(SegaSystemSP, Chd),
    // Final libretro-core-info dedicated-console audit additions.
    rule!(PhilipsCdI, BinCue => Chd, "optical_chdman", "chdman createcd", Supported, true, "lossless compression", true, false, "Valid CD descriptor and referenced tracks"),
    rule!(PhilipsCdI, Chd => BinCue, "optical_chdman", "chdman extractcd", Supported, false, "lossless extraction", true, false, "CD CHD extraction"),
    rule!(PhilipsCdI, BinCue => BinCue, "native_passthrough", "validate and copy", PassThrough, false, "lossless", false, false, "No conversion"),
    rule!(PhilipsCdI, Chd => Chd, "native_passthrough", "validate and copy", PassThrough, true, "lossless", false, false, "Already preferred"),
    pass!(PhilipsCdI, DiscIso),
    pass!(AmstradGx4000, AmstradGx4000Cpr),
    pass!(EpochCassetteVision, EpochCassetteVisionRom),
    pass!(EpochSuperCassetteVision, EpochSuperCassetteVisionRom),
    pass!(VTechCreatiVision, VTechCreatiVisionRom),
    pass!(NichibutsuMyVision, NichibutsuMyVisionRom),
    pass!(AtariXegs, AtariXegsImage),
    pass!(EmersonArcadia2001, Arcadia2001Rom),
    pass!(IntertonVc4000, IntertonVc4000Rom),
    pass!(ElektorTvGamesComputer, ElektorTvcRom),
    pass!(Arduboy, ArduboyImage),
    pass!(AmigaCd32, BinCue),
    pass!(AmigaCd32, DiscIso),
    pass!(AmigaCd32, DiscCcd),
    pass!(DingooA320, DingooNativeApp),
    pass!(SegaModel3, SegaModel3RomSet),
    pass!(SegaModel3, Chd),
    pass!(Nuon, NuonImage),
    pass!(Nuon, DiscIso),
    pass!(Nuon, DiscImg),
    pass!(Nuon, Chd),
    pass!(AquaplusPEce, AquaplusPeceImage),
    pass!(PocketStation, PocketStationImage),
    pass!(SegaVmu, SegaVmuImage),
    pass!(NokiaNGage, NokiaNGagePackage),
    pass!(TamagotchiP1, TamagotchiRom),
    pass!(Uzebox, UzeboxImage),
    pass!(WiiU, WiiUDiscImage),
    pass!(WiiU, WiiUArchive),
    pass!(WiiU, WiiUExecutable),
    pass!(SunplusSpmp8000, Spmp8000Rom),
    pass!(SunplusNative32, Native32Package),
    pass!(CapcomCps1, CapcomCps1RomSet),
    pass!(CapcomCps2, CapcomCps2RomSet),
    pass!(CapcomCps3, CapcomCps3RomSet),
    pass!(HandheldElectronicGames, HandheldElectronicImage),
];
