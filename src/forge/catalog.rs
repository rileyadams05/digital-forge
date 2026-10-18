use super::format::SourceFormat;
use super::platform::Platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformCategory {
    CartridgeConsole,
    DiscConsole,
    Handheld,
    ArcadeDerived,
}

#[derive(Debug)]
pub struct PlatformProfile {
    pub platform: Platform,
    pub display_name: &'static str,
    pub source_formats: &'static [SourceFormat],
    pub preferred_format: Option<SourceFormat>,
    pub category: PlatformCategory,
}

pub const LIBRETRO_CORE_INFO_AUDIT_SOURCE: &str = "https://github.com/libretro/libretro-core-info";
pub const LIBRETRO_CORE_INFO_AUDIT_REVISION: &str = "5a74858ab2f7a50cebb5a6330895bc38899531c0";

macro_rules! p {
    ($platform:ident, $category:ident, [$($source:ident),+], $preferred:ident) => {
        PlatformProfile {
            platform: Platform::$platform,
            display_name: Platform::$platform.display_name(),
            source_formats: &[$(SourceFormat::$source),+],
            preferred_format: Some(SourceFormat::$preferred),
            category: PlatformCategory::$category,
        }
    };
}

pub static PLATFORM_CATALOG: &[PlatformProfile] = &[
    p!(PlayStation1, DiscConsole, [BinCue, Pbp, Chd], Chd),
    p!(
        PlayStation2,
        DiscConsole,
        [DiscIso, Ps2BinCue, Chd, Cso, Cso2, Zso, Dax],
        DiscIso
    ),
    p!(
        PlayStation3,
        DiscConsole,
        [Ps3JbFolder, Ps3Iso, Ps3SplitIso, Ps3Pkg],
        Ps3Pkg
    ),
    p!(
        PlayStation4,
        DiscConsole,
        [Ps4RetailPkg, Ps4FakePkg, Ps4SplitPkg, Ps4Folder],
        Ps4Folder
    ),
    p!(
        PlayStation5,
        DiscConsole,
        [
            Ps5Pkg,
            Ps5RawSplitPkg,
            Ps5LooseFolder,
            Ps5DumpTar,
            ShadowMountFfPkg,
            ShadowMountExfatImage,
            ShadowMountPfsImage,
            ShadowMountCompressedPfs
        ],
        Ps5Pkg
    ),
    p!(Psp, Handheld, [DiscIso, Cso, Cso2, Zso, Dax, Pbp], DiscIso),
    p!(
        PlayStationVita,
        Handheld,
        [VitaPkg, VitaVpk, VitaExtracted],
        VitaExtracted
    ),
    p!(
        OriginalXbox,
        DiscConsole,
        [XboxOriginalIso, XboxOriginalFolder],
        XboxOriginalIso
    ),
    p!(
        Xbox360,
        DiscConsole,
        [Xbox360Iso, Xbox360God, Xbox360Folder, Xbox360SplitIso],
        Xbox360God
    ),
    p!(
        NintendoEntertainmentSystem,
        CartridgeConsole,
        [NesRom],
        NesRom
    ),
    p!(FamicomDiskSystem, CartridgeConsole, [FdsImage], FdsImage),
    p!(SuperNintendo, CartridgeConsole, [SnesRom], SnesRom),
    p!(
        Satellaview,
        CartridgeConsole,
        [SatellaviewRom, SnesRom],
        SatellaviewRom
    ),
    p!(
        SufamiTurbo,
        CartridgeConsole,
        [SufamiTurboRom, SnesRom],
        SufamiTurboRom
    ),
    p!(
        Nintendo64,
        CartridgeConsole,
        [N64Z64, N64V64, N64N64],
        N64Z64
    ),
    p!(Nintendo64DD, CartridgeConsole, [N64DdImage], N64DdImage),
    p!(GameBoy, Handheld, [GameBoyRom], GameBoyRom),
    p!(GameBoyColor, Handheld, [GameBoyColorRom], GameBoyColorRom),
    p!(
        GameBoyAdvance,
        Handheld,
        [GameBoyAdvanceRom],
        GameBoyAdvanceRom
    ),
    p!(VirtualBoy, CartridgeConsole, [VirtualBoyRom], VirtualBoyRom),
    p!(NintendoDS, Handheld, [NintendoDsRom], NintendoDsRom),
    p!(
        NintendoDSi,
        Handheld,
        [NintendoDsiRom, NintendoDsRom],
        NintendoDsiRom
    ),
    p!(
        Nintendo3DS,
        Handheld,
        [Nintendo3dsCci, Nintendo3dsCxi, Nintendo3ds3dsx],
        Nintendo3dsCci
    ),
    p!(PokemonMini, Handheld, [PokemonMiniRom], PokemonMiniRom),
    p!(
        GameCube,
        DiscConsole,
        [
            GameCubeIso,
            GameCubeGcm,
            GameCubeGcz,
            NintendoWia,
            NintendoRvz
        ],
        NintendoRvz
    ),
    p!(
        Wii,
        DiscConsole,
        [WiiIso, WiiWbfs, GameCubeGcz, NintendoWia, NintendoRvz],
        NintendoRvz
    ),
    p!(SegaSG1000, CartridgeConsole, [SegaSgRom], SegaSgRom),
    p!(SegaMasterSystem, CartridgeConsole, [SegaSmsRom], SegaSmsRom),
    p!(SegaGameGear, Handheld, [SegaGameGearRom], SegaGameGearRom),
    p!(
        SegaMegaDrive,
        CartridgeConsole,
        [SegaMdBinRom, SegaSmdRom],
        SegaMdBinRom
    ),
    p!(
        SegaCD,
        DiscConsole,
        [BinCue, DiscIso, Chd, DiscPlaylist],
        Chd
    ),
    p!(Sega32X, CartridgeConsole, [Sega32xRom], Sega32xRom),
    p!(
        SegaSaturn,
        DiscConsole,
        [BinCue, Chd, DiscCcd, DiscPlaylist],
        Chd
    ),
    p!(SegaPico, CartridgeConsole, [SegaMdBinRom], SegaMdBinRom),
    p!(SegaDreamcast, DiscConsole, [Gdi, BinCue, Cdi, Chd], Chd),
    p!(SegaNaomi, ArcadeDerived, [NaomiRomSet, Chd], NaomiRomSet),
    p!(SegaNaomi2, ArcadeDerived, [Naomi2RomSet, Chd], Naomi2RomSet),
    p!(
        SegaAtomiswave,
        ArcadeDerived,
        [AtomiswaveRomSet, Chd],
        AtomiswaveRomSet
    ),
    p!(
        SegaSystemSP,
        ArcadeDerived,
        [SystemSpRomSet, Chd],
        SystemSpRomSet
    ),
    p!(PcEngine, CartridgeConsole, [PcEngineRom], PcEngineRom),
    p!(
        SuperGrafx,
        CartridgeConsole,
        [SuperGrafxRom, PcEngineRom],
        SuperGrafxRom
    ),
    p!(
        PcEngineCD,
        DiscConsole,
        [BinCue, DiscIso, DiscImg, DiscCcd, Chd, DiscPlaylist],
        Chd
    ),
    p!(PcFx, DiscConsole, [BinCue, DiscCcd, DiscToc, Chd], Chd),
    p!(NeoGeoAESMVS, ArcadeDerived, [NeoGeoRom], NeoGeoRom),
    p!(NeoGeoCD, DiscConsole, [BinCue, Chd], Chd),
    p!(NeoGeoPocket, Handheld, [NeoGeoPocketRom], NeoGeoPocketRom),
    p!(
        NeoGeoPocketColor,
        Handheld,
        [NeoGeoPocketColorRom],
        NeoGeoPocketColorRom
    ),
    p!(Atari2600, CartridgeConsole, [Atari2600Rom], Atari2600Rom),
    p!(Atari5200, CartridgeConsole, [Atari5200Rom], Atari5200Rom),
    p!(Atari7800, CartridgeConsole, [Atari7800Rom], Atari7800Rom),
    p!(AtariLynx, Handheld, [AtariLynxRom], AtariLynxRom),
    p!(
        AtariJaguar,
        CartridgeConsole,
        [AtariJaguarRom],
        AtariJaguarRom
    ),
    p!(AtariJaguarCD, DiscConsole, [BinCue, Cdi], BinCue),
    p!(WonderSwan, Handheld, [WonderSwanRom], WonderSwanRom),
    p!(
        WonderSwanColor,
        Handheld,
        [WonderSwanColorRom],
        WonderSwanColorRom
    ),
    p!(ThreeDO, DiscConsole, [DiscIso, BinCue, Chd], DiscIso),
    p!(ColecoVision, CartridgeConsole, [ColecoRom], ColecoRom),
    p!(
        Intellivision,
        CartridgeConsole,
        [IntellivisionRom],
        IntellivisionRom
    ),
    p!(
        Odyssey2Videopac,
        CartridgeConsole,
        [Odyssey2Rom],
        Odyssey2Rom
    ),
    p!(Vectrex, CartridgeConsole, [VectrexRom], VectrexRom),
    p!(
        FairchildChannelF,
        CartridgeConsole,
        [FairchildChannelFRom],
        FairchildChannelFRom
    ),
    p!(
        WataraSupervision,
        Handheld,
        [WataraSupervisionRom],
        WataraSupervisionRom
    ),
    p!(MegaDuckCougarBoy, Handheld, [MegaDuckRom], MegaDuckRom),
    p!(PhilipsCdI, DiscConsole, [BinCue, DiscIso, Chd], Chd),
    p!(
        AmstradGx4000,
        CartridgeConsole,
        [AmstradGx4000Cpr],
        AmstradGx4000Cpr
    ),
    p!(
        EpochCassetteVision,
        CartridgeConsole,
        [EpochCassetteVisionRom],
        EpochCassetteVisionRom
    ),
    p!(
        EpochSuperCassetteVision,
        CartridgeConsole,
        [EpochSuperCassetteVisionRom],
        EpochSuperCassetteVisionRom
    ),
    p!(
        VTechCreatiVision,
        CartridgeConsole,
        [VTechCreatiVisionRom],
        VTechCreatiVisionRom
    ),
    p!(
        NichibutsuMyVision,
        CartridgeConsole,
        [NichibutsuMyVisionRom],
        NichibutsuMyVisionRom
    ),
    p!(
        AtariXegs,
        CartridgeConsole,
        [AtariXegsImage],
        AtariXegsImage
    ),
    p!(
        EmersonArcadia2001,
        CartridgeConsole,
        [Arcadia2001Rom],
        Arcadia2001Rom
    ),
    p!(
        IntertonVc4000,
        CartridgeConsole,
        [IntertonVc4000Rom],
        IntertonVc4000Rom
    ),
    p!(
        ElektorTvGamesComputer,
        CartridgeConsole,
        [ElektorTvcRom],
        ElektorTvcRom
    ),
    p!(Arduboy, Handheld, [ArduboyImage], ArduboyImage),
    p!(AmigaCd32, DiscConsole, [BinCue, DiscIso, DiscCcd], DiscIso),
    p!(DingooA320, Handheld, [DingooNativeApp], DingooNativeApp),
    p!(
        SegaModel3,
        ArcadeDerived,
        [SegaModel3RomSet, Chd],
        SegaModel3RomSet
    ),
    p!(
        Nuon,
        DiscConsole,
        [NuonImage, DiscIso, DiscImg, Chd],
        NuonImage
    ),
    p!(
        AquaplusPEce,
        Handheld,
        [AquaplusPeceImage],
        AquaplusPeceImage
    ),
    p!(
        PocketStation,
        Handheld,
        [PocketStationImage],
        PocketStationImage
    ),
    p!(SegaVmu, Handheld, [SegaVmuImage], SegaVmuImage),
    p!(NokiaNGage, Handheld, [NokiaNGagePackage], NokiaNGagePackage),
    p!(TamagotchiP1, Handheld, [TamagotchiRom], TamagotchiRom),
    p!(Uzebox, CartridgeConsole, [UzeboxImage], UzeboxImage),
    p!(
        WiiU,
        DiscConsole,
        [WiiUDiscImage, WiiUArchive, WiiUExecutable],
        WiiUArchive
    ),
    p!(SunplusSpmp8000, Handheld, [Spmp8000Rom], Spmp8000Rom),
    p!(
        SunplusNative32,
        Handheld,
        [Native32Package],
        Native32Package
    ),
    p!(
        CapcomCps1,
        ArcadeDerived,
        [CapcomCps1RomSet],
        CapcomCps1RomSet
    ),
    p!(
        CapcomCps2,
        ArcadeDerived,
        [CapcomCps2RomSet],
        CapcomCps2RomSet
    ),
    p!(
        CapcomCps3,
        ArcadeDerived,
        [CapcomCps3RomSet],
        CapcomCps3RomSet
    ),
    p!(
        HandheldElectronicGames,
        Handheld,
        [HandheldElectronicImage],
        HandheldElectronicImage
    ),
];

/// Current libretro-core-info entries deliberately outside Digital Forge's
/// dedicated-console scope. These are documented so a later audit does not
/// mistake them for omissions.
pub const COMPUTER_ONLY_EXCLUSIONS: &[(&str, &str)] = &[
    ("Acorn BBC Micro", "home computer"),
    (
        "Amstrad CPC",
        "home computer; GX4000 is represented separately",
    ),
    ("Apple II and Mac68k", "personal computers"),
    (
        "Atari 8-bit computers",
        "computer family; XEGS is represented separately",
    ),
    ("Atari ST/STE/TT/Falcon", "personal computers"),
    ("BK-0010/BK-0011", "personal computers"),
    (
        "Commodore Amiga and CDTV",
        "computer/multimedia systems; CD32 is represented separately",
    ),
    (
        "Commodore C64/C128/PET/PLUS4/VIC-20/CBM-II",
        "home computers",
    ),
    (
        "Commodore C64DTV and SuperCPU",
        "C64-compatible derivatives using the computer software database",
    ),
    ("DOS, IBM PC/XT and PCem targets", "personal computers"),
    ("Enterprise 128 and Videoton TV-Computer", "home computers"),
    ("Galaksija", "home computer"),
    ("MSX and Spectravideo", "home computers"),
    ("NEC PC-88 and PC-98", "personal computers"),
    ("Philips P2000T", "home computer"),
    ("SAM Coupé", "home computer"),
    ("Sharp X1 and X68000", "personal computers"),
    ("Sinclair ZX Spectrum and ZX81", "home computers"),
    ("Thomson MO/TO", "home computers"),
    ("TI-83", "calculator platform"),
];

pub const NON_HARDWARE_EXCLUSIONS: &[(&str, &str)] = &[
    (
        "Game engines and source ports",
        "software runtimes, not console hardware",
    ),
    (
        "PICO-8, TIC-80, WASM-4, LowRes NX, Vircon32 and VaporSpec",
        "fantasy/software consoles",
    ),
    ("CHIP-8, Uxn/Varvara and MicroW8", "virtual machines"),
    (
        "J2ME/Java ME and generic Symbian OS",
        "mobile software platforms; N-Gage is represented separately",
    ),
    (
        "MAME/MESS/UME and Arcade (various)",
        "aggregate machine catalogs, not one platform",
    ),
    (
        "Laserdisc arcade game and Handheld Electronic databases",
        "aggregate content families; MGW handheld images have one non-device-specific profile",
    ),
];

pub const OTHER_HARDWARE_EXCLUSIONS: &[(&str, &str)] = &[
    (
        "BBK, Longman, Nicai and Wenquxing",
        "electronic dictionaries/learning devices, not dedicated game consoles",
    ),
    ("Palm OS", "general-purpose PDA operating system"),
    (
        "Generic mobile Java and Symbian devices",
        "general-purpose phones; Nokia N-Gage is represented separately",
    ),
];

pub fn profile(platform: Platform) -> &'static PlatformProfile {
    PLATFORM_CATALOG
        .iter()
        .find(|entry| entry.platform == platform)
        .expect("every Platform must have a catalog profile")
}
