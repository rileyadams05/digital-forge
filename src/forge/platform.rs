use serde::{Deserialize, Serialize};

macro_rules! platforms {
    (@unit $variant:ident) => { () };
    ($( $variant:ident => ($display:literal, $code:literal) ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub enum Platform { $( $variant, )+ }

        impl Platform {
            pub const ALL: [Platform; <[()]>::len(&[$(platforms!(@unit $variant),)+])] = [$(Platform::$variant,)+];
            pub const fn display_name(self) -> &'static str {
                match self { $(Self::$variant => $display,)+ }
            }
            pub const fn short_code(self) -> &'static str {
                match self { $(Self::$variant => $code,)+ }
            }
        }
    };
}

platforms! {
    PlayStation1 => ("PlayStation 1", "ps1"),
    PlayStation2 => ("PlayStation 2", "ps2"),
    PlayStation3 => ("PlayStation 3", "ps3"),
    PlayStation4 => ("PlayStation 4", "ps4"),
    PlayStation5 => ("PlayStation 5", "ps5"),
    Psp => ("PlayStation Portable (PSP)", "psp"),
    PlayStationVita => ("PlayStation Vita", "vita"),
    OriginalXbox => ("Original Xbox", "xbox_orig"),
    Xbox360 => ("Xbox 360", "xbox360"),
    NintendoEntertainmentSystem => ("Nintendo Entertainment System", "nes"),
    FamicomDiskSystem => ("Famicom Disk System", "fds"),
    SuperNintendo => ("Super Nintendo Entertainment System", "snes"),
    Satellaview => ("Satellaview", "satellaview"),
    SufamiTurbo => ("Sufami Turbo", "sufami_turbo"),
    Nintendo64 => ("Nintendo 64", "n64"),
    Nintendo64DD => ("Nintendo 64DD", "n64dd"),
    GameBoy => ("Nintendo Game Boy", "gb"),
    GameBoyColor => ("Nintendo Game Boy Color", "gbc"),
    GameBoyAdvance => ("Nintendo Game Boy Advance", "gba"),
    VirtualBoy => ("Nintendo Virtual Boy", "virtual_boy"),
    NintendoDS => ("Nintendo DS", "nds"),
    NintendoDSi => ("Nintendo DSi", "dsi"),
    Nintendo3DS => ("Nintendo 3DS", "3ds"),
    PokemonMini => ("Pokémon Mini", "pokemon_mini"),
    GameCube => ("Nintendo GameCube", "gamecube"),
    Wii => ("Nintendo Wii", "wii"),
    SegaSG1000 => ("Sega SG-1000", "sg1000"),
    SegaMasterSystem => ("Sega Master System", "master_system"),
    SegaGameGear => ("Sega Game Gear", "game_gear"),
    SegaMegaDrive => ("Sega Mega Drive / Genesis", "mega_drive"),
    SegaCD => ("Sega CD / Mega-CD", "sega_cd"),
    Sega32X => ("Sega 32X", "32x"),
    SegaSaturn => ("Sega Saturn", "saturn"),
    SegaPico => ("Sega Pico", "pico"),
    SegaDreamcast => ("Sega Dreamcast", "dreamcast"),
    SegaNaomi => ("Sega NAOMI", "naomi"),
    SegaNaomi2 => ("Sega NAOMI 2", "naomi2"),
    SegaAtomiswave => ("Sammy Atomiswave", "atomiswave"),
    SegaSystemSP => ("Sega System SP", "system_sp"),
    PcEngine => ("PC Engine / TurboGrafx-16", "pc_engine"),
    SuperGrafx => ("PC Engine SuperGrafx", "supergrafx"),
    PcEngineCD => ("PC Engine CD / TurboGrafx-CD", "pc_engine_cd"),
    PcFx => ("NEC PC-FX", "pc_fx"),
    NeoGeoAESMVS => ("SNK Neo Geo AES / MVS", "neo_geo"),
    NeoGeoCD => ("SNK Neo Geo CD", "neo_geo_cd"),
    NeoGeoPocket => ("SNK Neo Geo Pocket", "ngp"),
    NeoGeoPocketColor => ("SNK Neo Geo Pocket Color", "ngpc"),
    Atari2600 => ("Atari 2600", "atari_2600"),
    Atari5200 => ("Atari 5200", "atari_5200"),
    Atari7800 => ("Atari 7800", "atari_7800"),
    AtariLynx => ("Atari Lynx", "atari_lynx"),
    AtariJaguar => ("Atari Jaguar", "atari_jaguar"),
    AtariJaguarCD => ("Atari Jaguar CD", "atari_jaguar_cd"),
    WonderSwan => ("Bandai WonderSwan", "wonderswan"),
    WonderSwanColor => ("Bandai WonderSwan Color", "wonderswan_color"),
    ThreeDO => ("3DO Interactive Multiplayer", "3do"),
    ColecoVision => ("ColecoVision", "colecovision"),
    Intellivision => ("Mattel Intellivision", "intellivision"),
    Odyssey2Videopac => ("Magnavox Odyssey² / Philips Videopac", "odyssey2"),
    Vectrex => ("GCE Vectrex", "vectrex"),
    FairchildChannelF => ("Fairchild Channel F", "channel_f"),
    WataraSupervision => ("Watara Supervision", "supervision"),
    MegaDuckCougarBoy => ("Mega Duck / Cougar Boy", "mega_duck"),
    PhilipsCdI => ("Philips CD-i", "philips_cdi"),
    AmstradGx4000 => ("Amstrad GX4000", "gx4000"),
    EpochCassetteVision => ("Epoch Cassette Vision", "cassette_vision"),
    EpochSuperCassetteVision => ("Epoch Super Cassette Vision", "super_cassette_vision"),
    VTechCreatiVision => ("VTech CreatiVision", "creativision"),
    NichibutsuMyVision => ("Nichibutsu My Vision", "my_vision"),
    AtariXegs => ("Atari XEGS", "atari_xegs"),
    EmersonArcadia2001 => ("Emerson Arcadia 2001", "arcadia_2001"),
    IntertonVc4000 => ("Interton VC 4000", "vc_4000"),
    ElektorTvGamesComputer => ("Elektor TV Games Computer", "elektor_tvc"),
    Arduboy => ("Arduboy", "arduboy"),
    AmigaCd32 => ("Commodore Amiga CD32", "amiga_cd32"),
    DingooA320 => ("Dingoo A320 / Gemei A330", "dingoo_a320"),
    SegaModel3 => ("Sega Model 3", "sega_model_3"),
    Nuon => ("VM Labs NUON", "nuon"),
    AquaplusPEce => ("Aquaplus P/ECE", "pece"),
    PocketStation => ("Sony PocketStation", "pocketstation"),
    SegaVmu => ("Sega Visual Memory Unit", "sega_vmu"),
    NokiaNGage => ("Nokia N-Gage", "n_gage"),
    TamagotchiP1 => ("Bandai Tamagotchi P1", "tamagotchi_p1"),
    Uzebox => ("Uzebox", "uzebox"),
    WiiU => ("Nintendo Wii U", "wii_u"),
    SunplusSpmp8000 => ("Sunplus SPMP8000", "spmp8000"),
    SunplusNative32 => ("Sunplus Native32", "native32"),
    CapcomCps1 => ("Capcom CP System I", "cps1"),
    CapcomCps2 => ("Capcom CP System II", "cps2"),
    CapcomCps3 => ("Capcom CP System III", "cps3"),
    HandheldElectronicGames => ("Handheld Electronic Games", "handheld_electronic"),
}

impl Platform {
    pub fn folder_name(self) -> &'static str {
        self.display_name()
    }

    pub fn nxe_relative_dir(self) -> &'static str {
        match self {
            Self::Xbox360 => "Games\\Xenia\\",
            Self::PlayStation2 => "Games\\XBSX2\\",
            Self::Psp => "Games\\RetroArch\\PSP\\",
            Self::PlayStation1 => "Games\\RetroArch\\PlayStation\\",
            Self::OriginalXbox => "Games\\XQEMU\\",
            Self::PlayStation3 => "Games\\RPCS3\\",
            Self::PlayStationVita => "Games\\Vita3K\\",
            Self::PlayStation4 => "Games\\shadPS4\\",
            Self::PlayStation5 => "Games\\PS5\\",
            _ => "Games\\Digital Forge\\",
        }
    }

    pub fn default_profile(self) -> crate::forge::profile::TargetProfile {
        use crate::forge::profile::TargetProfile;
        match self {
            Self::PlayStation1 => TargetProfile::Ps1BinCue,
            Self::PlayStation2 => TargetProfile::Ps2OplExFat,
            Self::PlayStation3 => TargetProfile::Ps3PkgDirect,
            Self::PlayStation4 => TargetProfile::Ps4ExtractedFolder,
            Self::PlayStation5 => TargetProfile::Ps5StandardPkg,
            Self::Psp => TargetProfile::PspIso,
            Self::PlayStationVita => TargetProfile::VitaNoNpDrm,
            Self::OriginalXbox => TargetProfile::XboxOriginalCerbios,
            Self::Xbox360 => TargetProfile::Xbox360RghGod,
            Self::Nintendo64 => TargetProfile::Nintendo64Z64,
            Self::GameCube | Self::Wii => TargetProfile::NintendoRvz,
            Self::SegaCD
            | Self::SegaSaturn
            | Self::SegaDreamcast
            | Self::PcEngineCD
            | Self::PcFx
            | Self::NeoGeoCD
            | Self::ThreeDO
            | Self::PhilipsCdI => TargetProfile::OpticalChd,
            _ => TargetProfile::NativePassthrough,
        }
    }
}
