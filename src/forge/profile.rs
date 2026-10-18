use super::format::TargetFormat;
use super::platform::Platform;
use serde::{Deserialize, Serialize};

/// Why the user is preparing the game.  This is intentionally separate from
/// `TargetFormat`: a PKG, ISO or folder is an implementation detail, while the
/// destination decides which of those representations is actually useful.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UsageMode {
    PhysicalConsole,
    Emulator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConsoleStorage {
    Internal,
    External,
}

impl UsageMode {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::PhysicalConsole => "Physical Console",
            Self::Emulator => "Emulator",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetWorkflow {
    Ps4ItemzFlow,
    Ps4InternalInstall,
    Ps4ShadPs4,
    Ps5ItemzFlow,
    Ps5InternalInstall,
    Ps5AnyPS5,
    Xbox360Console,
    Xbox360Xenia,
    EstablishedConsoleWorkflow,
    NativeEmulatorFormat,
}

impl TargetWorkflow {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Ps4ItemzFlow => "ItemzFlow FG App Folder",
            Self::Ps4InternalInstall => "PS4 Internal Storage — FPKG / PKG",
            Self::Ps4ShadPs4 => "shadPS4",
            Self::Ps5ItemzFlow => "ItemzFlow — Prepare Native PS5 Game Folder",
            Self::Ps5InternalInstall => "PS5 Internal Storage — PKG",
            Self::Ps5AnyPS5 => "AnyPS5 — Prepare for PC",
            Self::Xbox360Console => "RGH / JTAG Console — Games on Demand",
            Self::Xbox360Xenia => "Xenia — Keep a Natively Supported Format",
            Self::EstablishedConsoleWorkflow => "Established Physical-Console Workflow",
            Self::NativeEmulatorFormat => "Emulator — Keep a Natively Supported Format",
        }
    }

    pub const fn usage_mode(self) -> UsageMode {
        match self {
            Self::Ps4ItemzFlow
            | Self::Ps4InternalInstall
            | Self::Ps5ItemzFlow
            | Self::Ps5InternalInstall
            | Self::Xbox360Console
            | Self::EstablishedConsoleWorkflow => UsageMode::PhysicalConsole,
            Self::Ps4ShadPs4
            | Self::Ps5AnyPS5
            | Self::Xbox360Xenia
            | Self::NativeEmulatorFormat => UsageMode::Emulator,
        }
    }

    pub const fn forced_target(self) -> Option<TargetFormat> {
        match self {
            Self::Ps4ItemzFlow => Some(TargetFormat::Ps4Folder),
            // Internal PS4 storage is package-only. ItemzFlow folder output is
            // reserved for the explicit external-storage workflow.
            Self::Ps4InternalInstall => Some(TargetFormat::Ps4FakePkg),
            Self::Ps4ShadPs4 => Some(TargetFormat::Ps4Folder),
            Self::Ps5ItemzFlow => Some(TargetFormat::Ps5LooseFolder),
            // Internal PS5 storage is package-only. A loose PS5 dump has no
            // safe package builder in this application and must not be
            // presented as an internal-installable result.
            Self::Ps5InternalInstall => Some(TargetFormat::Ps5Pkg),
            Self::Ps5AnyPS5 => Some(TargetFormat::Ps5AnyPS5Output),
            Self::Xbox360Console => Some(TargetFormat::Xbox360God),
            Self::Xbox360Xenia | Self::EstablishedConsoleWorkflow | Self::NativeEmulatorFormat => {
                None
            }
        }
    }

    pub fn target_profile(self, platform: Platform) -> TargetProfile {
        match self {
            Self::Ps4ItemzFlow | Self::Ps4ShadPs4 => TargetProfile::Ps4ExtractedFolder,
            Self::Ps4InternalInstall => TargetProfile::Ps4FakePkg,
            Self::Ps5ItemzFlow => TargetProfile::Ps5LooseFolder,
            Self::Ps5AnyPS5 => TargetProfile::Ps5AnyPS5,
            Self::Ps5InternalInstall => TargetProfile::Ps5StandardPkg,
            Self::Xbox360Console => TargetProfile::Xbox360RghGod,
            Self::Xbox360Xenia | Self::NativeEmulatorFormat => TargetProfile::NativePassthrough,
            Self::EstablishedConsoleWorkflow => platform.default_profile(),
        }
    }
}

pub fn target_workflows(platform: Platform, usage: UsageMode) -> Vec<TargetWorkflow> {
    match (platform, usage) {
        (Platform::PlayStation4, UsageMode::PhysicalConsole) => vec![TargetWorkflow::Ps4ItemzFlow],
        (Platform::PlayStation4, UsageMode::Emulator) => vec![TargetWorkflow::Ps4ShadPs4],
        (Platform::PlayStation5, UsageMode::PhysicalConsole) => vec![TargetWorkflow::Ps5ItemzFlow],
        (Platform::PlayStation5, UsageMode::Emulator) => vec![TargetWorkflow::Ps5AnyPS5],
        (Platform::Xbox360, UsageMode::PhysicalConsole) => vec![TargetWorkflow::Xbox360Console],
        (Platform::Xbox360, UsageMode::Emulator) => vec![TargetWorkflow::Xbox360Xenia],
        (_, UsageMode::PhysicalConsole) => vec![TargetWorkflow::EstablishedConsoleWorkflow],
        (_, UsageMode::Emulator) => vec![TargetWorkflow::NativeEmulatorFormat],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ps5_physical_and_pc_choices_use_separate_workflows() {
        assert_eq!(
            target_workflow_for_usage(Platform::PlayStation5, UsageMode::PhysicalConsole),
            Some(TargetWorkflow::Ps5ItemzFlow)
        );
        assert_eq!(
            target_workflow_for_usage(Platform::PlayStation5, UsageMode::Emulator),
            Some(TargetWorkflow::Ps5AnyPS5)
        );
    }
}

pub const fn target_workflow_for_console_storage(
    platform: Platform,
    storage: ConsoleStorage,
) -> Option<TargetWorkflow> {
    match (platform, storage) {
        (Platform::PlayStation4, ConsoleStorage::Internal) => {
            Some(TargetWorkflow::Ps4InternalInstall)
        }
        (Platform::PlayStation4, ConsoleStorage::External) => Some(TargetWorkflow::Ps4ItemzFlow),
        (Platform::PlayStation5, _) => None,
        _ => None,
    }
}

pub const fn target_workflow_for_usage(
    platform: Platform,
    usage: UsageMode,
) -> Option<TargetWorkflow> {
    match (platform, usage) {
        (Platform::PlayStation4, UsageMode::PhysicalConsole) => Some(TargetWorkflow::Ps4ItemzFlow),
        (Platform::PlayStation4, UsageMode::Emulator) => Some(TargetWorkflow::Ps4ShadPs4),
        (Platform::PlayStation5, UsageMode::PhysicalConsole) => Some(TargetWorkflow::Ps5ItemzFlow),
        (Platform::PlayStation5, UsageMode::Emulator) => Some(TargetWorkflow::Ps5AnyPS5),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TargetProfile {
    NativePassthrough,
    OpticalChd,
    Nintendo64Z64,
    NintendoRvz,
    // PS1
    Ps1BinCue,
    Ps1Pbp,
    Ps1Chd,
    Ps1PsioCu2,
    Ps1PopstarterVcd,

    // PS2
    Ps2OplExFat,
    Ps2OplFat32,
    Ps2InternalHdd,
    Ps2MaxCsoZso,
    Ps2MaxCsoCso,
    Ps2Chd,
    Ps2Classics,

    // PS3 (PKG is the only Digital Forge output)
    Ps3PkgDirect,

    // PS4
    Ps4FakePkg,
    Ps4RetailPkg,
    Ps4ExtractedFolder,

    // PS5
    Ps5StandardPkg,
    Ps5EtaHenDpi,
    Ps5PkgManager,
    Ps5ShadowMountFolder,
    Ps5ShadowMountImage,
    Ps5LooseFolder,
    Ps5AnyPS5,
    Ps5ExternalUsb,

    // PSP
    PspIso,
    PspCso,
    PspZso,
    PspDax,
    PspJso,
    PspPs1Pbp,

    // PS Vita
    VitaNoNpDrm,
    VitaLegacyVpk,

    // Original Xbox
    XboxOriginalCerbios,
    XboxOriginalModdedDash,
    XboxOriginalXiso,
    XboxOriginalTrimmedXiso,

    // Xbox 360
    Xbox360RghGod,
    Xbox360ExtractedFolder,
    Xbox360RawIso,
    Xbox360SplitIso,
}

impl TargetProfile {
    pub fn platform(&self) -> Platform {
        match self {
            Self::NativePassthrough => Platform::NintendoEntertainmentSystem,
            Self::OpticalChd => Platform::SegaCD,
            Self::Nintendo64Z64 => Platform::Nintendo64,
            Self::NintendoRvz => Platform::GameCube,
            Self::Ps1BinCue
            | Self::Ps1Pbp
            | Self::Ps1Chd
            | Self::Ps1PsioCu2
            | Self::Ps1PopstarterVcd => Platform::PlayStation1,
            Self::Ps2OplExFat
            | Self::Ps2OplFat32
            | Self::Ps2InternalHdd
            | Self::Ps2MaxCsoZso
            | Self::Ps2MaxCsoCso
            | Self::Ps2Chd
            | Self::Ps2Classics => Platform::PlayStation2,
            Self::Ps3PkgDirect => Platform::PlayStation3,
            Self::Ps4FakePkg | Self::Ps4RetailPkg | Self::Ps4ExtractedFolder => {
                Platform::PlayStation4
            }
            Self::Ps5StandardPkg
            | Self::Ps5EtaHenDpi
            | Self::Ps5PkgManager
            | Self::Ps5ShadowMountFolder
            | Self::Ps5ShadowMountImage
            | Self::Ps5LooseFolder
            | Self::Ps5AnyPS5
            | Self::Ps5ExternalUsb => Platform::PlayStation5,
            Self::PspIso
            | Self::PspCso
            | Self::PspZso
            | Self::PspDax
            | Self::PspJso
            | Self::PspPs1Pbp => Platform::Psp,
            Self::VitaNoNpDrm | Self::VitaLegacyVpk => Platform::PlayStationVita,
            Self::XboxOriginalCerbios
            | Self::XboxOriginalModdedDash
            | Self::XboxOriginalXiso
            | Self::XboxOriginalTrimmedXiso => Platform::OriginalXbox,
            Self::Xbox360RghGod
            | Self::Xbox360ExtractedFolder
            | Self::Xbox360RawIso
            | Self::Xbox360SplitIso => Platform::Xbox360,
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::NativePassthrough => "Native Console Format (Validate and Copy)",
            Self::OpticalChd => "CHD Compressed Optical Disc",
            Self::Nintendo64Z64 => "Nintendo 64 Z64 Canonical Byte Order",
            Self::NintendoRvz => "Nintendo GameCube / Wii RVZ",
            Self::Ps1BinCue => "PS1: Standard BIN/CUE (Broad Compatibility / ODE / DuckStation)",
            Self::Ps1Pbp => "PS1: EBOOT.PBP (PSP / PS3 / ePSXe)",
            Self::Ps1Chd => "PS1: CHD Compressed (RetroArch / DuckStation)",
            Self::Ps1PsioCu2 => "PS1: PSIO CU2 Sheet + BIN (PSIO Loader)",
            Self::Ps1PopstarterVcd => "PS1: POPStarter VCD (PS2 USB/SMB)",
            Self::Ps2OplExFat => "PS2: OPL exFAT USB / SMB / MX4SIO (ISO format)",
            Self::Ps2OplFat32 => "PS2: OPL FAT32 USB (USBExtreme split chunks)",
            Self::Ps2InternalHdd => "PS2: Internal APA HDD (HDLoader / HDL-Dump)",
            Self::Ps2MaxCsoZso => "PS2: Compressed ZSO (OPL 1.2.0+)",
            Self::Ps2MaxCsoCso => "PS2: Compressed CSO (OPL / PCSX2)",
            Self::Ps2Chd => "PS2: Compressed CHD (PCSX2 / RetroArch)",
            Self::Ps2Classics => "PS2: PS2 Classics Encrypted ISO (PS3/HEN)",
            Self::Ps3PkgDirect => "PS3: Installable PKG",
            Self::Ps4FakePkg => "PS4: Fake PKG (FPKG / .pkg)",
            Self::Ps4RetailPkg => "PS4: Retail PKG (.pkg)",
            Self::Ps4ExtractedFolder => "PS4: Extracted / Loose Application Directory",
            Self::Ps5StandardPkg => "PS5: Standard Complete PKG / FPKG",
            Self::Ps5EtaHenDpi => "PS5: etaHEN Direct PKG Installer (Port 9090 Network Stream)",
            Self::Ps5PkgManager => "PS5: PS5 PKG Manager (Port 8844 / USB / Direct Network)",
            Self::Ps5ShadowMountFolder => {
                "PS5: ShadowMountPlus Folder (Direct scan path with sce_sys root)"
            }
            Self::Ps5ShadowMountImage => {
                "PS5: ShadowMountPlus Image Container (.ffpkg / .exfat / .ffpfs)"
            }
            Self::Ps5LooseFolder => "PS5: Loose Game Folder (Standard Directory Layout)",
            Self::Ps5AnyPS5 => "PS5: AnyPS5 PC Preparation (source/ + app0/)",
            Self::Ps5ExternalUsb => "PS5: External USB Drive (Structured Directory Deployment)",
            Self::PspIso => "PSP: Standard ISO (Broad Compatibility)",
            Self::PspCso => "PSP: CSO Compressed ISO (MaxCSO)",
            Self::PspZso => "PSP: ZSO Compressed ISO (LZ4)",
            Self::PspDax => "PSP: DAX Compressed Disc Image (Dark_AleX)",
            Self::PspJso => "PSP: JSO Compressed Disc Image",
            Self::PspPs1Pbp => "PSP: PS1 EBOOT.PBP",
            Self::VitaNoNpDrm => "PS Vita: NoNpDrm Extracted Hierarchy (Recommended)",
            Self::VitaLegacyVpk => "PS Vita: Legacy VPK Package",
            Self::XboxOriginalCerbios => "Original Xbox: Cerbios BIOS (CCI Compressed Image)",
            Self::XboxOriginalModdedDash => "Original Xbox: Modded Dashboard (Extracted Folder)",
            Self::XboxOriginalXiso => "Original Xbox: Standard XISO Disc Image",
            Self::XboxOriginalTrimmedXiso => "Original Xbox: Trimmed XISO (Zero Padding Removed)",
            Self::Xbox360RghGod => "Xbox 360: RGH / JTAG Games On Demand (GOD - Recommended)",
            Self::Xbox360ExtractedFolder => "Xbox 360: Extracted Disc Folder (default.xex)",
            Self::Xbox360RawIso => "Xbox 360: Uncompressed Disc ISO (XGD2 / XGD3)",
            Self::Xbox360SplitIso => "Xbox 360: Split ISO for FAT32 USB",
        }
    }

    pub fn recommended_target(&self) -> TargetFormat {
        match self {
            Self::NativePassthrough => TargetFormat::Passthrough,
            Self::OpticalChd => TargetFormat::Chd,
            Self::Nintendo64Z64 => TargetFormat::N64Z64,
            Self::NintendoRvz => TargetFormat::NintendoRvz,
            Self::Ps1BinCue => TargetFormat::BinCue,
            Self::Ps1Pbp => TargetFormat::Pbp,
            Self::Ps1Chd => TargetFormat::Chd,
            Self::Ps1PsioCu2 => TargetFormat::Ps1Cu2,
            Self::Ps1PopstarterVcd => TargetFormat::Ps1Vcd,
            Self::Ps2OplExFat => TargetFormat::Ps2Iso,
            Self::Ps2OplFat32 => TargetFormat::Ps2UsbExtreme,
            Self::Ps2InternalHdd => TargetFormat::Ps2HdLoader,
            Self::Ps2MaxCsoZso => TargetFormat::Ps2Zso,
            Self::Ps2MaxCsoCso => TargetFormat::Ps2Cso,
            Self::Ps2Chd => TargetFormat::Ps2Chd,
            Self::Ps2Classics => TargetFormat::Ps2ClassicsEnc,
            Self::Ps3PkgDirect => TargetFormat::Ps3Pkg,
            Self::Ps4FakePkg => TargetFormat::Ps4FakePkg,
            Self::Ps4RetailPkg => TargetFormat::Ps4RetailPkg,
            Self::Ps4ExtractedFolder => TargetFormat::Ps4Folder,
            Self::Ps5StandardPkg | Self::Ps5EtaHenDpi | Self::Ps5PkgManager => TargetFormat::Ps5Pkg,
            Self::Ps5ShadowMountFolder => TargetFormat::ShadowMountFolder,
            Self::Ps5ShadowMountImage => TargetFormat::ShadowMountImage,
            Self::Ps5LooseFolder | Self::Ps5ExternalUsb => TargetFormat::Ps5LooseFolder,
            Self::Ps5AnyPS5 => TargetFormat::Ps5AnyPS5Output,
            Self::PspIso => TargetFormat::PspIso,
            Self::PspCso => TargetFormat::PspCso,
            Self::PspZso => TargetFormat::PspZso,
            Self::PspDax => TargetFormat::PspDax,
            Self::PspJso => TargetFormat::PspJso,
            Self::PspPs1Pbp => TargetFormat::PspPbp,
            Self::VitaNoNpDrm => TargetFormat::VitaNoNpDrmFolder,
            Self::VitaLegacyVpk => TargetFormat::VitaVpk,
            Self::XboxOriginalCerbios => TargetFormat::XboxOriginalCci,
            Self::XboxOriginalModdedDash => TargetFormat::XboxOriginalFolder,
            Self::XboxOriginalXiso => TargetFormat::XboxOriginalXiso,
            Self::XboxOriginalTrimmedXiso => TargetFormat::XboxOriginalTrimmedXiso,
            Self::Xbox360RghGod => TargetFormat::Xbox360God,
            Self::Xbox360ExtractedFolder => TargetFormat::Xbox360Folder,
            Self::Xbox360RawIso => TargetFormat::Xbox360Iso,
            Self::Xbox360SplitIso => TargetFormat::Xbox360SplitIso,
        }
    }
}
