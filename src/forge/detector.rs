use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Error, bail, ensure};

use super::format::{ConfidenceLevel, LicenseStatus, PackageClassification, SourceFormat};
use super::platform::Platform;

#[derive(Debug, Clone)]
pub struct DetectionResult {
    pub platform: Platform,
    pub source_format: SourceFormat,
    pub package_classification: Option<PackageClassification>,
    pub confidence: ConfidenceLevel,
    pub game_id: Option<String>,
    pub title_name: Option<String>,
    pub is_multipart: bool,
    pub is_archive: bool,
    pub is_multidisc: bool,
    pub disc_number: Option<u32>,
    pub total_discs: Option<u32>,
    pub related_files: Vec<PathBuf>,
    pub missing_parts: Vec<String>,
    pub license_status: LicenseStatus,
}

pub struct FormatDetector;

impl FormatDetector {
    pub fn detect(
        source: &Path,
        hint_platform: Option<Platform>,
    ) -> Result<DetectionResult, Error> {
        if !source.exists() {
            bail!("source path does not exist: {}", source.display());
        }

        if source.is_dir() {
            Self::detect_directory(source, hint_platform)
        } else {
            Self::detect_file(source, hint_platform)
        }
    }

    fn detect_directory(
        dir: &Path,
        hint_platform: Option<Platform>,
    ) -> Result<DetectionResult, Error> {
        let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let (is_multidisc, disc_num, total_discs) = parse_disc_marker(dir_name);

        // 1. PS5 Loose / Decrypted Game Folder: check for sce_sys/param.json
        let ps5_param_json = dir.join("sce_sys").join("param.json");
        if ps5_param_json.is_file() {
            let (title_id, title_name, pkg_class) = Self::read_ps5_param_json(&ps5_param_json);
            return Ok(DetectionResult {
                platform: Platform::PlayStation5,
                source_format: SourceFormat::Ps5LooseFolder,
                package_classification: pkg_class,
                confidence: ConfidenceLevel::Definitive,
                game_id: title_id,
                title_name,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![dir.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // Check if loose folder has eboot.bin + sce_sys (and hint is PS5)
        let eboot_bin = dir.join("eboot.bin");
        let sce_sys_dir = dir.join("sce_sys");
        if eboot_bin.is_file()
            && sce_sys_dir.is_dir()
            && hint_platform == Some(Platform::PlayStation5)
        {
            return Ok(DetectionResult {
                platform: Platform::PlayStation5,
                source_format: SourceFormat::Ps5LooseFolder,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::High,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![dir.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 2. PS3 JB Folder: check for PS3_GAME directory or PS3_DISC.SFB
        let ps3_game = dir.join("PS3_GAME");
        let ps3_disc_sfb = dir.join("PS3_DISC.SFB");
        if ps3_game.is_dir() || ps3_disc_sfb.is_file() {
            let (title_id, title_name) = Self::read_ps3_sfo(&ps3_game.join("PARAM.SFO"));
            return Ok(DetectionResult {
                platform: Platform::PlayStation3,
                source_format: SourceFormat::Ps3JbFolder,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: title_id,
                title_name,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![dir.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 3. Original Xbox Extracted Folder: check for default.xbe
        let default_xbe = dir.join("default.xbe");
        if default_xbe.is_file() {
            let (title_id, title_name) = Self::read_xbe_info(&default_xbe);
            return Ok(DetectionResult {
                platform: Platform::OriginalXbox,
                source_format: SourceFormat::XboxOriginalFolder,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: title_id,
                title_name,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![dir.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 4. Xbox 360 Extracted Folder: check for default.xex
        let default_xex = dir.join("default.xex");
        if default_xex.is_file() {
            let (title_id, title_name) = Self::read_xex_info(&default_xex);
            return Ok(DetectionResult {
                platform: Platform::Xbox360,
                source_format: SourceFormat::Xbox360Folder,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: title_id,
                title_name,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![dir.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 5. Xbox 360 GOD container directory (e.g. 415607E7/00007000/...)
        if let Some((title_id, god_files)) = Self::check_xbox360_god_dir(dir) {
            return Ok(DetectionResult {
                platform: Platform::Xbox360,
                source_format: SourceFormat::Xbox360God,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: Some(title_id),
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: god_files,
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // A PS5 app.json/app_N dump set is deliberately not promoted to a
        // package source. There is no integrated PC-side PKG reconstruction engine.

        // 7. PS2 USBExtreme folder: contains ul.cfg and ul.* files
        let ul_cfg = dir.join("ul.cfg");
        if ul_cfg.is_file() {
            let (game_id, ul_files) = Self::parse_usbextreme_dir(dir);
            return Ok(DetectionResult {
                platform: Platform::PlayStation2,
                source_format: SourceFormat::Ps2UsbExtreme,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id,
                title_name: None,
                is_multipart: true,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: ul_files,
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 8. PS4 Extracted Directory or Vita NoNpDrm Directory: contains sce_sys/param.sfo or eboot.bin
        let sce_sfo = dir.join("sce_sys").join("param.sfo");
        if sce_sfo.is_file() || eboot_bin.is_file() {
            let (title_id, title_name) = Self::read_sfo(&sce_sfo);
            let is_ps4 = hint_platform == Some(Platform::PlayStation4)
                || title_id
                    .as_deref()
                    .is_some_and(|id| id.starts_with("CUSA") || id.starts_with("CUSA-"));
            if is_ps4 {
                return Ok(DetectionResult {
                    platform: Platform::PlayStation4,
                    source_format: SourceFormat::Ps4Folder,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::Definitive,
                    game_id: title_id,
                    title_name,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![dir.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            } else if sce_sfo.is_file() || hint_platform == Some(Platform::PlayStationVita) {
                return Ok(DetectionResult {
                    platform: Platform::PlayStationVita,
                    source_format: SourceFormat::VitaExtracted,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::High,
                    game_id: title_id,
                    title_name,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![dir.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            }
        }

        // 9. PS2 / PS1 folder with SYSTEM.CNF
        let system_cnf = dir.join("SYSTEM.CNF");
        if system_cnf.is_file() {
            let cnf_content = fs::read_to_string(&system_cnf).unwrap_or_default();
            if cnf_content.contains("BOOT2") {
                let game_id = parse_system_cnf_game_id(&cnf_content);
                return Ok(DetectionResult {
                    platform: Platform::PlayStation2,
                    source_format: SourceFormat::DiscIso,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::High,
                    game_id,
                    title_name: None,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![dir.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            } else if cnf_content.contains("BOOT") {
                let game_id = parse_system_cnf_game_id(&cnf_content);
                return Ok(DetectionResult {
                    platform: Platform::PlayStation1,
                    source_format: SourceFormat::BinCue,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::High,
                    game_id,
                    title_name: None,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![dir.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            }
        }

        // Fallback for directory
        let fallback_platform = hint_platform.unwrap_or(Platform::PlayStation5);
        Ok(DetectionResult {
            platform: fallback_platform,
            source_format: SourceFormat::Unknown,
            package_classification: None,
            confidence: ConfidenceLevel::Low,
            game_id: None,
            title_name: None,
            is_multipart: false,
            is_archive: false,
            is_multidisc,
            disc_number: disc_num,
            total_discs,
            related_files: vec![dir.to_path_buf()],
            missing_parts: Vec::new(),
            license_status: LicenseStatus::NotApplicable,
        })
    }

    fn detect_file(
        file_path: &Path,
        hint_platform: Option<Platform>,
    ) -> Result<DetectionResult, Error> {
        let file_name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let (is_multidisc, disc_num, total_discs) = parse_disc_marker(file_name);

        if let Some(result) = detect_console_file(
            file_path,
            hint_platform,
            is_multidisc,
            disc_num,
            total_discs,
        )? {
            return Ok(result);
        }

        // PS5 filesystem images are accepted only when their content agrees with
        // the extension. The extension alone never establishes a format.
        if let Some(ext) = file_path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("ffpkg") && has_ufs2_superblock(file_path) {
                return Ok(DetectionResult {
                    platform: Platform::PlayStation5,
                    source_format: SourceFormat::ShadowMountFfPkg,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::Definitive,
                    game_id: None,
                    title_name: None,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![file_path.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            }
            if ext.eq_ignore_ascii_case("exfat") && has_exfat_boot_sector(file_path) {
                return Ok(DetectionResult {
                    platform: Platform::PlayStation5,
                    source_format: SourceFormat::ShadowMountExfatImage,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::Definitive,
                    game_id: None,
                    title_name: None,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![file_path.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            }
            if ext.eq_ignore_ascii_case("ffpfs") && has_pfs_magic(file_path) {
                return Ok(DetectionResult {
                    platform: Platform::PlayStation5,
                    source_format: SourceFormat::ShadowMountPfsImage,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::Definitive,
                    game_id: None,
                    title_name: None,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![file_path.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            }
            if ext.eq_ignore_ascii_case("ffpfsc") && has_pfsc_magic(file_path) {
                return Ok(DetectionResult {
                    platform: Platform::PlayStation5,
                    source_format: SourceFormat::ShadowMountCompressedPfs,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::Definitive,
                    game_id: None,
                    title_name: None,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![file_path.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            }
        }

        // 2. Check if file is part of a multipart archive (.7z.001, .part1.rar, .zip.001, etc.)
        if let Some(multipart_info) = check_multipart_archive(file_path) {
            return Ok(DetectionResult {
                platform: hint_platform.unwrap_or(Platform::Xbox360),
                source_format: SourceFormat::MultipartArchive,
                package_classification: None,
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: true,
                is_archive: true,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: multipart_info.present_files,
                missing_parts: multipart_info.missing_parts,
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 3. Check if file is a standard archive (.7z, .zip, .rar, .tar, .gz)
        if is_archive_extension(file_path) {
            return Ok(DetectionResult {
                platform: hint_platform.unwrap_or(Platform::Xbox360),
                source_format: SourceFormat::Archive,
                package_classification: None,
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: true,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 4. Check for split ISO (.iso.0, .iso.1, etc.)
        if let Some((split_files, missing)) = check_split_iso(file_path) {
            let platform = hint_platform.unwrap_or(Platform::PlayStation3);
            let source_format = if platform == Platform::Xbox360 {
                SourceFormat::Xbox360SplitIso
            } else {
                SourceFormat::Ps3SplitIso
            };
            return Ok(DetectionResult {
                platform,
                source_format,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: true,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: split_files,
                missing_parts: missing,
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 5. Check for split PS4 / PS5 PKG (.pkg.0, .pkg.1)
        if let Some((pkg_pieces, missing)) = check_split_pkg(file_path) {
            let mut is_ps5_multipart = false;
            let mut is_ps5_pkg = false;
            let mut detected_game_id = None;

            if let Some(first_piece) = pkg_pieces.first() {
                if let Ok(mut piece_f) = File::open(first_piece) {
                    let mut piece_hdr = [0u8; 4096];
                    let n = piece_f.read(&mut piece_hdr).unwrap_or(0);
                    let piece_hdr = &piece_hdr[..n];

                    if piece_hdr.len() == 4096
                        && piece_hdr.starts_with(b"PS5MPKG1")
                        && u32::from_le_bytes(piece_hdr[8..12].try_into().unwrap_or_default()) == 1
                    {
                        is_ps5_multipart = true;
                    } else if piece_hdr.starts_with(b"\x7FPKG") {
                        let cid = parse_pkg_content_id(piece_hdr);
                        if let Some(ref c) = cid {
                            if c.contains("PPSA") || c.contains("EB") {
                                is_ps5_pkg = true;
                            }
                            detected_game_id = title_id_from_content_id(c);
                        }
                    }
                }
            }

            let (platform, source_format) = if is_ps5_multipart {
                (Platform::PlayStation5, SourceFormat::Ps5PkgManagerMultipart)
            } else if is_ps5_pkg || hint_platform == Some(Platform::PlayStation5) {
                (Platform::PlayStation5, SourceFormat::Ps5RawSplitPkg)
            } else {
                (Platform::PlayStation4, SourceFormat::Ps4SplitPkg)
            };

            return Ok(DetectionResult {
                platform,
                source_format,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: detected_game_id,
                title_name: None,
                is_multipart: true,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: pkg_pieces,
                missing_parts: missing,
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // Deep inspection of file header / magic bytes
        let mut f =
            File::open(file_path).with_context(|| format!("open {}", file_path.display()))?;
        let file_len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut header = [0u8; 4096];
        let bytes_read = f.read(&mut header).unwrap_or(0);
        let header = &header[..bytes_read];

        // Sony PKG/CNT and PS5 FIH package headers.
        if header.starts_with(b"\x7FPKG")
            || header.starts_with(b"\x7FCNT")
            || header.starts_with(b"\x7FFIH")
        {
            return Self::detect_pkg_content(file_path, header, file_len, hint_platform);
        }

        // 7. PlayStation PBP format (\0PBP)
        if header.starts_with(b"\x00PBP") {
            return Self::detect_pbp_content(file_path, &mut f, hint_platform);
        }

        // 8. CSO compressed image (CISO)
        if header.starts_with(b"CISO") {
            let platform = hint_platform.unwrap_or(Platform::Psp);
            let source_format = if header.get(20).copied().unwrap_or(1) >= 2 {
                SourceFormat::Cso2
            } else {
                SourceFormat::Cso
            };
            return Ok(DetectionResult {
                platform,
                source_format,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 9. ZSO uses the documented ZISO header magic.
        if header.starts_with(b"ZISO") {
            let platform = hint_platform.unwrap_or(Platform::PlayStation2);
            return Ok(DetectionResult {
                platform,
                source_format: SourceFormat::Zso,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 10. DAX compressed image (DAX\0)
        if header.starts_with(b"DAX\x00") {
            return Ok(DetectionResult {
                platform: Platform::Psp,
                source_format: SourceFormat::Dax,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // JSO is recognised for validation only; MaxCSO does not support it.
        if header.starts_with(b"JISO") {
            return Ok(DetectionResult {
                platform: Platform::Psp,
                source_format: SourceFormat::Jso,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // CCI is not identified from invented CERB/CCI magic. It remains
        // validation-only until a real Repackinator parser is integrated.

        // 12. CHD compressed disc image
        if header.starts_with(b"MComprHD") {
            let platform = hint_platform.unwrap_or(Platform::PlayStation1);
            return Ok(DetectionResult {
                platform,
                source_format: SourceFormat::Chd,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 12b. PS1 CloneCD (CCD / IMG / SUB)
        if file_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("ccd"))
        {
            let img = file_path.with_extension("img");
            let sub = file_path.with_extension("sub");
            let mut related = vec![file_path.to_path_buf()];
            if img.is_file() {
                related.push(img);
            }
            if sub.is_file() {
                related.push(sub);
            }
            return Ok(DetectionResult {
                platform: Platform::PlayStation1,
                source_format: SourceFormat::Ps1Ccd,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: related,
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 12c. PS1 Alcohol 120% (MDF / MDS)
        if file_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mds") || e.eq_ignore_ascii_case("mdf"))
        {
            let mdf = file_path.with_extension("mdf");
            let mds = file_path.with_extension("mds");
            let mut related = vec![file_path.to_path_buf()];
            if mdf.is_file() && !related.contains(&mdf) {
                related.push(mdf);
            }
            if mds.is_file() && !related.contains(&mds) {
                related.push(mds);
            }
            return Ok(DetectionResult {
                platform: Platform::PlayStation1,
                source_format: SourceFormat::Ps1Mdf,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: related,
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 12d. PS1 PSIO CU2 sheet
        if file_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cu2"))
        {
            let bin = file_path.with_extension("bin");
            let mut related = vec![file_path.to_path_buf()];
            if bin.is_file() {
                related.push(bin);
            }
            return Ok(DetectionResult {
                platform: Platform::PlayStation1,
                source_format: SourceFormat::Ps1Cu2,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: related,
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 12e. POPStarter VCD Disc Image (.vcd)
        if file_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("vcd"))
        {
            return Ok(DetectionResult {
                platform: Platform::PlayStation1,
                source_format: SourceFormat::Ps1Vcd,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 12f. ECM compressed image (.ecm / .bin.ecm)
        if file_name.to_lowercase().ends_with(".ecm") {
            return Ok(DetectionResult {
                platform: Platform::PlayStation1,
                source_format: SourceFormat::Ps1Ecm,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 12g. PS2 Classics encrypted image (ISO.BIN.ENC)
        if file_name.eq_ignore_ascii_case("ISO.BIN.ENC")
            || file_path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("enc"))
        {
            return Ok(DetectionResult {
                platform: Platform::PlayStation2,
                source_format: SourceFormat::Ps2ClassicsEnc,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 13. BIN / CUE / IMG disc image
        if file_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
        {
            let (tracks, missing) = Self::parse_cue_sheet(file_path);
            let platform = hint_platform.unwrap_or(Platform::PlayStation1);
            let source_format = if platform == Platform::PlayStation2 {
                SourceFormat::Ps2BinCue
            } else {
                SourceFormat::BinCue
            };
            return Ok(DetectionResult {
                platform,
                source_format,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: tracks,
                missing_parts: missing,
                license_status: LicenseStatus::NotApplicable,
            });
        }

        if file_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("bin") || e.eq_ignore_ascii_case("img"))
        {
            let cue_companion = file_path.with_extension("cue");
            if !cue_companion.is_file()
                && !matches!(
                    hint_platform,
                    Some(
                        Platform::PlayStation1
                            | Platform::PlayStation2
                            | Platform::SegaCD
                            | Platform::SegaSaturn
                            | Platform::PcEngineCD
                            | Platform::PcFx
                            | Platform::ThreeDO
                            | Platform::NeoGeoCD
                            | Platform::AtariJaguarCD
                    )
                )
            {
                return Ok(DetectionResult {
                    platform: hint_platform.unwrap_or(Platform::PlayStation1),
                    source_format: SourceFormat::Unknown,
                    package_classification: None,
                    confidence: ConfidenceLevel::Low,
                    game_id: None,
                    title_name: None,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![file_path.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            }
            let related = if cue_companion.is_file() {
                vec![file_path.to_path_buf(), cue_companion]
            } else {
                vec![file_path.to_path_buf()]
            };
            let platform = hint_platform.unwrap_or(Platform::PlayStation1);
            let source_format = if platform == Platform::PlayStation2 {
                SourceFormat::Ps2BinCue
            } else {
                SourceFormat::BinCue
            };
            return Ok(DetectionResult {
                platform,
                source_format,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::High,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: related,
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 14. Xbox Executable (XBE / XEX)
        if header.starts_with(b"XBEH") {
            let (title_id, title_name) = Self::read_xbe_info(file_path);
            return Ok(DetectionResult {
                platform: Platform::OriginalXbox,
                source_format: SourceFormat::XboxOriginalFolder,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: title_id,
                title_name,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        if header.starts_with(b"XEX2")
            || header.starts_with(b"XEX1")
            || header.starts_with(b"XEX%")
            || header.starts_with(b"XEX-")
        {
            let (title_id, title_name) = Self::read_xex_info(file_path);
            return Ok(DetectionResult {
                platform: Platform::Xbox360,
                source_format: SourceFormat::Xbox360Folder,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: title_id,
                title_name,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 15. Xbox 360 STFS container (CON / LIVE / PIRS)
        if header.starts_with(b"CON ") || header.starts_with(b"LIVE") || header.starts_with(b"PIRS")
        {
            return Ok(DetectionResult {
                platform: Platform::Xbox360,
                source_format: SourceFormat::Xbox360God,
                package_classification: Some(PackageClassification::GameApp),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 16. Vita VPK
        if file_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("vpk"))
        {
            return Ok(DetectionResult {
                platform: Platform::PlayStationVita,
                source_format: SourceFormat::VitaVpk,
                package_classification: Some(PackageClassification::Homebrew),
                confidence: ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc,
                disc_number: disc_num,
                total_discs,
                related_files: vec![file_path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: LicenseStatus::NotApplicable,
            });
        }

        // 17. Disc ISO inspection (XDVDFS, GDF, ISO9660, UDF)
        if file_path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
            || header.len() >= 0x8000
        {
            return Self::detect_disc_iso(&mut f, file_path, file_len, hint_platform);
        }

        // Default fallback
        let platform = hint_platform.unwrap_or(Platform::Xbox360);
        Ok(DetectionResult {
            platform,
            source_format: SourceFormat::Unknown,
            package_classification: None,
            confidence: ConfidenceLevel::Low,
            game_id: None,
            title_name: None,
            is_multipart: false,
            is_archive: false,
            is_multidisc,
            disc_number: disc_num,
            total_discs,
            related_files: vec![file_path.to_path_buf()],
            missing_parts: Vec::new(),
            license_status: LicenseStatus::NotApplicable,
        })
    }

    fn detect_pkg_content(
        pkg_path: &Path,
        header: &[u8],
        _file_len: u64,
        hint_platform: Option<Platform>,
    ) -> Result<DetectionResult, Error> {
        let content_id = parse_pkg_content_id(header);
        let title_id = content_id.as_deref().and_then(title_id_from_content_id);

        // Check for companion .rap / .rif file beside package
        let parent_dir = pkg_path.parent().unwrap_or_else(|| Path::new("."));
        let mut license_status = LicenseStatus::NotApplicable;

        // Debug/development packages carry the high revision bit and do not
        // use a console RAP. Only NPDRM-style PS3 packages enter the RAP
        // resolution path.
        let requires_rap = header
            .get(4..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
            .is_some_and(|package_type| package_type == 1);

        if requires_rap {
            if let Some(ref cid) = content_id {
                let rap_name = format!("{cid}.rap");
                let direct_rap = parent_dir.join(&rap_name);
                if direct_rap.is_file() {
                    license_status = LicenseStatus::FoundBundled(direct_rap);
                } else {
                    let mut found = None;
                    if let Ok(entries) = fs::read_dir(parent_dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path
                                .extension()
                                .is_some_and(|e| e.eq_ignore_ascii_case("rap"))
                            {
                                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                                    if name.eq_ignore_ascii_case(cid)
                                        || (title_id
                                            .as_ref()
                                            .is_some_and(|t| name.contains(t.as_str())))
                                    {
                                        found = Some(path);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if let Some(found_path) = found {
                        license_status = LicenseStatus::FoundBundled(found_path);
                    } else {
                        license_status = LicenseStatus::RequiredMissing {
                            content_id: Some(cid.clone()),
                            title_id: title_id.clone(),
                            suggested_filename: rap_name,
                        };
                    }
                }
            }
        }

        // Determine PackageClassification and Platform from content ID & package markers
        let mut package_classification = PackageClassification::Retail;
        let drm_type = if header.len() >= 0x74 {
            u32::from_be_bytes(header[0x70..0x74].try_into().unwrap_or_default())
        } else {
            0
        };

        let (platform, source_format) = if let Some(hint) = hint_platform {
            match hint {
                Platform::PlayStation5 => (Platform::PlayStation5, SourceFormat::Ps5Pkg),
                Platform::PlayStation4 => {
                    let is_fake = drm_type == 0xF
                        || header.starts_with(b"\x7FCNT")
                        || content_id
                            .as_deref()
                            .is_some_and(|c| c.contains("FPKG") || c.contains("FAKE"));
                    if is_fake {
                        package_classification = PackageClassification::FakePackage;
                        (Platform::PlayStation4, SourceFormat::Ps4FakePkg)
                    } else {
                        package_classification = PackageClassification::Retail;
                        (Platform::PlayStation4, SourceFormat::Ps4RetailPkg)
                    }
                }
                Platform::PlayStationVita => (Platform::PlayStationVita, SourceFormat::VitaPkg),
                _ => (Platform::PlayStation3, SourceFormat::Ps3Pkg),
            }
        } else if let Some(ref cid) = content_id {
            if cid.contains("PPSA") || cid.contains("EB") {
                package_classification = if cid.contains("FPKG") || cid.contains("FAKE") {
                    PackageClassification::FakePackage
                } else if cid.contains("-PATCH") || cid.contains("_00-") {
                    PackageClassification::Update
                } else {
                    PackageClassification::GameApp
                };
                (Platform::PlayStation5, SourceFormat::Ps5Pkg)
            } else if cid.contains("CUSA") || cid.contains("PLAS") {
                let is_fake = drm_type == 0xF
                    || header.starts_with(b"\x7FCNT")
                    || cid.contains("FPKG")
                    || cid.contains("FAKE");
                if is_fake {
                    package_classification = PackageClassification::FakePackage;
                    (Platform::PlayStation4, SourceFormat::Ps4FakePkg)
                } else {
                    package_classification = PackageClassification::Retail;
                    (Platform::PlayStation4, SourceFormat::Ps4RetailPkg)
                }
            } else if cid.contains("PCSB")
                || cid.contains("PCSA")
                || cid.contains("PCSE")
                || cid.contains("PCSG")
            {
                (Platform::PlayStationVita, SourceFormat::VitaPkg)
            } else {
                (Platform::PlayStation3, SourceFormat::Ps3Pkg)
            }
        } else {
            (Platform::PlayStation3, SourceFormat::Ps3Pkg)
        };

        Ok(DetectionResult {
            platform,
            source_format,
            package_classification: Some(package_classification),
            confidence: ConfidenceLevel::Definitive,
            game_id: title_id,
            title_name: None,
            is_multipart: false,
            is_archive: false,
            is_multidisc: false,
            disc_number: None,
            total_discs: None,
            related_files: vec![pkg_path.to_path_buf()],
            missing_parts: Vec::new(),
            license_status,
        })
    }

    fn detect_pbp_content<R: Read + Seek>(
        file_path: &Path,
        reader: &mut R,
        hint_platform: Option<Platform>,
    ) -> Result<DetectionResult, Error> {
        let mut header = [0u8; 40];
        reader.seek(SeekFrom::Start(0))?;
        reader.read_exact(&mut header)?;
        let psar_offset = u32::from_le_bytes(header[36..40].try_into()?) as u64;
        reader.seek(SeekFrom::Start(psar_offset))?;
        let mut psar = [0u8; 64];
        let count = reader.read(&mut psar)?;
        let is_ps1 = psar[..count].windows(8).any(|window| window == b"PSISOIMG");
        let platform = if is_ps1 {
            Platform::PlayStation1
        } else {
            Platform::Psp
        };
        if let Some(hint) = hint_platform {
            ensure!(
                hint == platform,
                "PBP content identifies as {}, not {}",
                platform.display_name(),
                hint.display_name()
            );
        }
        Ok(DetectionResult {
            platform,
            source_format: SourceFormat::Pbp,
            package_classification: Some(PackageClassification::GameApp),
            confidence: ConfidenceLevel::Definitive,
            game_id: None,
            title_name: None,
            is_multipart: false,
            is_archive: false,
            is_multidisc: false,
            disc_number: None,
            total_discs: None,
            related_files: vec![file_path.to_path_buf()],
            missing_parts: Vec::new(),
            license_status: LicenseStatus::NotApplicable,
        })
    }

    fn detect_disc_iso<R: Read + Seek>(
        reader: &mut R,
        file_path: &Path,
        _file_len: u64,
        hint_platform: Option<Platform>,
    ) -> Result<DetectionResult, Error> {
        let name = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let (is_multidisc, disc_num, total_discs) = parse_disc_marker(name);

        // Check for Original Xbox XDVDFS header at offset 0x10000 (sector 32)
        if reader.seek(SeekFrom::Start(0x10000)).is_ok() {
            let mut magic = [0u8; 20];
            if reader.read_exact(&mut magic).is_ok() && &magic == b"MICROSOFT*XBOX*MEDIA" {
                return Ok(DetectionResult {
                    platform: Platform::OriginalXbox,
                    source_format: SourceFormat::XboxOriginalIso,
                    package_classification: Some(PackageClassification::GameApp),
                    confidence: ConfidenceLevel::Definitive,
                    game_id: None,
                    title_name: None,
                    is_multipart: false,
                    is_archive: false,
                    is_multidisc,
                    disc_number: disc_num,
                    total_discs,
                    related_files: vec![file_path.to_path_buf()],
                    missing_parts: Vec::new(),
                    license_status: LicenseStatus::NotApplicable,
                });
            }
        }

        // Check for Xbox 360 GDF header at standard XGD offsets
        for &offset in &[0x80000u64, 0x2080000u64, 0x10000u64] {
            if reader.seek(SeekFrom::Start(offset)).is_ok() {
                let mut magic = [0u8; 20];
                if reader.read_exact(&mut magic).is_ok() && &magic == b"MICROSOFT*XBOX*MEDIA" {
                    return Ok(DetectionResult {
                        platform: Platform::Xbox360,
                        source_format: SourceFormat::Xbox360Iso,
                        package_classification: Some(PackageClassification::GameApp),
                        confidence: ConfidenceLevel::Definitive,
                        game_id: None,
                        title_name: None,
                        is_multipart: false,
                        is_archive: false,
                        is_multidisc,
                        disc_number: disc_num,
                        total_discs,
                        related_files: vec![file_path.to_path_buf()],
                        missing_parts: Vec::new(),
                        license_status: LicenseStatus::NotApplicable,
                    });
                }
            }
        }

        // Check for ISO9660 Volume Descriptor at offset 0x8000 (sector 16)
        if reader.seek(SeekFrom::Start(0x8000)).is_ok() {
            let mut desc = [0u8; 2048];
            if reader.read_exact(&mut desc).is_ok() && &desc[1..6] == b"CD001" {
                let sys_id = String::from_utf8_lossy(&desc[8..40]);
                let vol_id = String::from_utf8_lossy(&desc[40..72]);
                if sys_id.contains("PLAYSTATION")
                    || vol_id.contains("PS2")
                    || vol_id.contains("PLAYSTATION")
                {
                    let platform = if hint_platform == Some(Platform::PlayStation1) {
                        Platform::PlayStation1
                    } else if hint_platform == Some(Platform::Psp) {
                        Platform::Psp
                    } else {
                        Platform::PlayStation2
                    };
                    return Ok(DetectionResult {
                        platform,
                        source_format: SourceFormat::DiscIso,
                        package_classification: Some(PackageClassification::GameApp),
                        confidence: ConfidenceLevel::High,
                        game_id: None,
                        title_name: Some(vol_id.trim().to_string()),
                        is_multipart: false,
                        is_archive: false,
                        is_multidisc,
                        disc_number: disc_num,
                        total_discs,
                        related_files: vec![file_path.to_path_buf()],
                        missing_parts: Vec::new(),
                        license_status: LicenseStatus::NotApplicable,
                    });
                }
                if let Some(
                    platform @ (Platform::SegaCD
                    | Platform::PcEngineCD
                    | Platform::ThreeDO
                    | Platform::PhilipsCdI),
                ) = hint_platform
                {
                    return Ok(DetectionResult {
                        platform,
                        source_format: SourceFormat::DiscIso,
                        package_classification: Some(PackageClassification::GameApp),
                        confidence: ConfidenceLevel::High,
                        game_id: None,
                        title_name: Some(vol_id.trim().to_string()),
                        is_multipart: false,
                        is_archive: false,
                        is_multidisc,
                        disc_number: disc_num,
                        total_discs,
                        related_files: vec![file_path.to_path_buf()],
                        missing_parts: Vec::new(),
                        license_status: LicenseStatus::NotApplicable,
                    });
                }
            }
        }

        let platform = hint_platform.unwrap_or(Platform::PlayStation3);

        Ok(DetectionResult {
            platform,
            source_format: SourceFormat::Unknown,
            package_classification: None,
            confidence: ConfidenceLevel::Low,
            game_id: None,
            title_name: None,
            is_multipart: false,
            is_archive: false,
            is_multidisc,
            disc_number: disc_num,
            total_discs,
            related_files: vec![file_path.to_path_buf()],
            missing_parts: Vec::new(),
            license_status: LicenseStatus::NotApplicable,
        })
    }

    fn read_ps5_param_json(
        json_path: &Path,
    ) -> (
        Option<String>,
        Option<String>,
        Option<PackageClassification>,
    ) {
        if let Ok(content) = fs::read_to_string(json_path) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                let title_id = v
                    .get("titleId")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                let title_name = v
                    .get("titleName")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        v.get("localizedParameters")
                            .and_then(|lp| lp.get("default"))
                            .and_then(|d| d.get("titleName"))
                            .and_then(|n| n.as_str())
                            .map(|s| s.to_string())
                    });
                let category = v.get("category").and_then(|c| c.as_str()).unwrap_or("");
                let classification = match category {
                    "gp" => Some(PackageClassification::Update),
                    "ac" => Some(PackageClassification::Dlc),
                    "gd" | "gdc" | "gda" => Some(PackageClassification::GameApp),
                    _ => Some(PackageClassification::Unknown),
                };
                return (title_id, title_name, classification);
            }
        }
        (None, None, None)
    }

    pub fn read_sfo(sfo_path: &Path) -> (Option<String>, Option<String>) {
        if !sfo_path.is_file() {
            return (None, None);
        }
        let bytes = fs::read(sfo_path).unwrap_or_default();
        if bytes.len() < 20 || !bytes.starts_with(b"\x00PSF") {
            return (None, None);
        }
        let title_id = extract_sfo_string(&bytes, "TITLE_ID");
        let title = extract_sfo_string(&bytes, "TITLE");
        (title_id, title)
    }

    pub fn read_sfo_category(sfo_path: &Path) -> Option<String> {
        Self::read_sfo_key(sfo_path, "CATEGORY")
    }

    pub fn read_sfo_key(sfo_path: &Path, key: &str) -> Option<String> {
        if !sfo_path.is_file() {
            return None;
        }
        let bytes = fs::read(sfo_path).ok()?;
        if bytes.len() < 20 || !bytes.starts_with(b"\x00PSF") {
            return None;
        }
        extract_sfo_string(&bytes, key)
    }

    pub fn read_sfo_content_id(sfo_path: &Path) -> Option<String> {
        Self::read_sfo_key(sfo_path, "CONTENT_ID")
    }

    fn read_ps3_sfo(sfo_path: &Path) -> (Option<String>, Option<String>) {
        Self::read_sfo(sfo_path)
    }

    fn read_xbe_info(xbe_path: &Path) -> (Option<String>, Option<String>) {
        if let Ok(file) = File::open(xbe_path) {
            if let Ok(xbe) = crate::executable::xbe::XbeHeader::read(file) {
                if let Some(info) = xbe.fields.execution_info {
                    return (Some(format!("{:08X}", info.title_id)), None);
                }
            }
        }
        (None, None)
    }

    fn read_xex_info(xex_path: &Path) -> (Option<String>, Option<String>) {
        if let Ok(file) = File::open(xex_path) {
            if let Ok(xex) = crate::executable::xex::XexHeader::read(file) {
                if let Some(info) = xex.fields.execution_info {
                    return (Some(format!("{:08X}", info.title_id)), None);
                }
            }
        }
        (None, None)
    }

    fn check_xbox360_god_dir(dir: &Path) -> Option<(String, Vec<PathBuf>)> {
        let name = dir.file_name()?.to_str()?;
        if name.len() == 8 && name.chars().all(|c| c.is_ascii_hexdigit()) {
            let sub7000 = dir.join("00007000");
            if sub7000.is_dir() {
                let mut files = Vec::new();
                if let Ok(entries) = fs::read_dir(&sub7000) {
                    for entry in entries.flatten() {
                        files.push(entry.path());
                    }
                }
                return Some((name.to_ascii_uppercase(), files));
            }
        }
        None
    }

    fn parse_usbextreme_dir(dir: &Path) -> (Option<String>, Vec<PathBuf>) {
        let mut files = Vec::new();
        let mut game_id = None;
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                if name.starts_with("ul.") {
                    let parts: Vec<&str> = name.split('.').collect();
                    if parts.len() >= 3 && game_id.is_none() {
                        game_id = Some(parts[2].to_string());
                    }
                    files.push(path);
                }
            }
        }
        files.sort();
        (game_id, files)
    }

    fn parse_cue_sheet(cue_path: &Path) -> (Vec<PathBuf>, Vec<String>) {
        let mut files = vec![cue_path.to_path_buf()];
        let mut missing = Vec::new();
        let parent = cue_path.parent().unwrap_or_else(|| Path::new("."));
        if let Ok(content) = fs::read_to_string(cue_path) {
            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("FILE") {
                    if let Some(start) = trimmed.find('"') {
                        if let Some(end) = trimmed[start + 1..].find('"') {
                            let track_file = &trimmed[start + 1..start + 1 + end];
                            let track_path = parent.join(track_file);
                            if track_path.is_file() {
                                files.push(track_path);
                            } else {
                                missing.push(track_file.to_string());
                            }
                        }
                    }
                }
            }
        }
        (files, missing)
    }
}

// Helpers
fn has_exfat_boot_sector(path: &Path) -> bool {
    let mut sector = [0u8; 512];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut sector))
        .is_ok()
        && &sector[3..11] == b"EXFAT   "
        && sector[510..512] == [0x55, 0xaa]
}

fn detect_console_file(
    path: &Path,
    hint: Option<Platform>,
    is_multidisc: bool,
    disc_number: Option<u32>,
    total_discs: Option<u32>,
) -> Result<Option<DetectionResult>, Error> {
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(4 * 1024 * 1024)
        .read_to_end(&mut bytes)?;

    let make = |platform: Platform,
                source_format: SourceFormat,
                confidence: ConfidenceLevel,
                related_files: Vec<PathBuf>,
                missing_parts: Vec<String>| DetectionResult {
        platform,
        source_format,
        package_classification: None,
        confidence,
        game_id: None,
        title_name: None,
        is_multipart: related_files.len() > 1,
        is_archive: matches!(ext.as_str(), "zip" | "7z")
            && matches!(
                source_format,
                SourceFormat::NaomiRomSet
                    | SourceFormat::Naomi2RomSet
                    | SourceFormat::AtomiswaveRomSet
                    | SourceFormat::SystemSpRomSet
                    | SourceFormat::NeoGeoRom
                    | SourceFormat::SegaModel3RomSet
                    | SourceFormat::CapcomCps1RomSet
                    | SourceFormat::CapcomCps2RomSet
                    | SourceFormat::CapcomCps3RomSet
                    | SourceFormat::EpochCassetteVisionRom
                    | SourceFormat::EpochSuperCassetteVisionRom
                    | SourceFormat::Native32Package
            ),
        is_multidisc,
        disc_number,
        total_discs,
        related_files,
        missing_parts,
        license_status: LicenseStatus::NotApplicable,
    };
    let one = || vec![path.to_path_buf()];

    if matches!(ext.as_str(), "zip" | "7z")
        && (bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"7z\xbc\xaf\x27\x1c"))
    {
        let structured = match hint {
            Some(Platform::SegaNaomi) => Some((Platform::SegaNaomi, SourceFormat::NaomiRomSet)),
            Some(Platform::SegaNaomi2) => Some((Platform::SegaNaomi2, SourceFormat::Naomi2RomSet)),
            Some(Platform::SegaAtomiswave) => {
                Some((Platform::SegaAtomiswave, SourceFormat::AtomiswaveRomSet))
            }
            Some(Platform::SegaSystemSP) => {
                Some((Platform::SegaSystemSP, SourceFormat::SystemSpRomSet))
            }
            Some(Platform::NeoGeoAESMVS) => Some((Platform::NeoGeoAESMVS, SourceFormat::NeoGeoRom)),
            Some(Platform::SegaModel3) => {
                Some((Platform::SegaModel3, SourceFormat::SegaModel3RomSet))
            }
            Some(Platform::CapcomCps1) => {
                Some((Platform::CapcomCps1, SourceFormat::CapcomCps1RomSet))
            }
            Some(Platform::CapcomCps2) => {
                Some((Platform::CapcomCps2, SourceFormat::CapcomCps2RomSet))
            }
            Some(Platform::CapcomCps3) => {
                Some((Platform::CapcomCps3, SourceFormat::CapcomCps3RomSet))
            }
            Some(Platform::EpochCassetteVision) => Some((
                Platform::EpochCassetteVision,
                SourceFormat::EpochCassetteVisionRom,
            )),
            Some(Platform::EpochSuperCassetteVision) => Some((
                Platform::EpochSuperCassetteVision,
                SourceFormat::EpochSuperCassetteVisionRom,
            )),
            Some(Platform::SunplusNative32) => {
                Some((Platform::SunplusNative32, SourceFormat::Native32Package))
            }
            _ => None,
        };
        if let Some((platform, format)) = structured {
            return Ok(Some(make(
                platform,
                format,
                ConfidenceLevel::High,
                one(),
                Vec::new(),
            )));
        }
    }

    if bytes.len() >= 4 {
        let (platform, format) = match &bytes[..4] {
            [0x80, 0x37, 0x12, 0x40] => (Platform::Nintendo64, SourceFormat::N64Z64),
            [0x37, 0x80, 0x40, 0x12] => (Platform::Nintendo64, SourceFormat::N64V64),
            [0x40, 0x12, 0x37, 0x80] => (Platform::Nintendo64, SourceFormat::N64N64),
            _ => (Platform::Nintendo64, SourceFormat::Unknown),
        };
        if format != SourceFormat::Unknown {
            return Ok(Some(make(
                platform,
                format,
                ConfidenceLevel::Definitive,
                one(),
                Vec::new(),
            )));
        }
    }

    if bytes.starts_with(b"NES\x1a") || bytes.starts_with(b"UNIF") {
        return Ok(Some(make(
            Platform::NintendoEntertainmentSystem,
            SourceFormat::NesRom,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }
    if bytes.starts_with(b"FDS\x1a") {
        return Ok(Some(make(
            Platform::FamicomDiskSystem,
            SourceFormat::FdsImage,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }

    const NINTENDO_LOGO: [u8; 48] = [
        0xce, 0xed, 0x66, 0x66, 0xcc, 0x0d, 0x00, 0x0b, 0x03, 0x73, 0x00, 0x83, 0x00, 0x0c, 0x00,
        0x0d, 0x00, 0x08, 0x11, 0x1f, 0x88, 0x89, 0x00, 0x0e, 0xdc, 0xcc, 0x6e, 0xe6, 0xdd, 0xdd,
        0xd9, 0x99, 0xbb, 0xbb, 0x67, 0x63, 0x6e, 0x0e, 0xec, 0xcc, 0xdd, 0xdc, 0x99, 0x9f, 0xbb,
        0xb9, 0x33, 0x3e,
    ];
    if bytes.len() >= 0x150 && bytes[0x104..0x134] == NINTENDO_LOGO {
        let mut checksum = 0u8;
        for byte in &bytes[0x134..=0x14c] {
            checksum = checksum.wrapping_sub(*byte).wrapping_sub(1);
        }
        if checksum == bytes[0x14d] {
            let color = matches!(bytes[0x143], 0x80 | 0xc0);
            return Ok(Some(make(
                if color {
                    Platform::GameBoyColor
                } else {
                    Platform::GameBoy
                },
                if color {
                    SourceFormat::GameBoyColorRom
                } else {
                    SourceFormat::GameBoyRom
                },
                ConfidenceLevel::Definitive,
                one(),
                Vec::new(),
            )));
        }
    }

    if bytes.len() >= 0xc0 && bytes[4..8] == [0x24, 0xff, 0xae, 0x51] {
        let checksum = bytes[0xa0..0xbd]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_sub(*byte))
            .wrapping_sub(0x19);
        if checksum == bytes[0xbd] {
            return Ok(Some(make(
                Platform::GameBoyAdvance,
                SourceFormat::GameBoyAdvanceRom,
                ConfidenceLevel::Definitive,
                one(),
                Vec::new(),
            )));
        }
    }

    if bytes.len() >= 0x200
        && bytes[0xc0..0xc4] == [0x24, 0xff, 0xae, 0x51]
        && bytes[0x0c..0x10]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric())
    {
        let dsi = matches!(bytes[0x12], 2 | 3) || hint == Some(Platform::NintendoDSi);
        return Ok(Some(make(
            if dsi {
                Platform::NintendoDSi
            } else {
                Platform::NintendoDS
            },
            if dsi {
                SourceFormat::NintendoDsiRom
            } else {
                SourceFormat::NintendoDsRom
            },
            ConfidenceLevel::High,
            one(),
            Vec::new(),
        )));
    }

    if bytes.len() >= 0x104 && &bytes[0x100..0x104] == b"NCSD" {
        return Ok(Some(make(
            Platform::Nintendo3DS,
            SourceFormat::Nintendo3dsCci,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }
    if bytes.len() >= 0x104 && &bytes[0x100..0x104] == b"NCCH" {
        return Ok(Some(make(
            Platform::Nintendo3DS,
            SourceFormat::Nintendo3dsCxi,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }
    if bytes.starts_with(b"3DSX") {
        return Ok(Some(make(
            Platform::Nintendo3DS,
            SourceFormat::Nintendo3ds3dsx,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }

    if bytes.len() >= 0x20 && bytes[0x1c..0x20] == [0xc2, 0x33, 0x9f, 0x3d] {
        let format = if ext == "gcm" {
            SourceFormat::GameCubeGcm
        } else {
            SourceFormat::GameCubeIso
        };
        return Ok(Some(make(
            Platform::GameCube,
            format,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }
    if bytes.len() >= 0x1c && bytes[0x18..0x1c] == [0x5d, 0x1c, 0x9e, 0xa3] {
        return Ok(Some(make(
            Platform::Wii,
            SourceFormat::WiiIso,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }
    if bytes.starts_with(b"WBFS") {
        return Ok(Some(make(
            Platform::Wii,
            SourceFormat::WiiWbfs,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }
    if bytes.starts_with(&[0x01, 0xc0, 0x0b, 0xb1]) {
        let platform = hint
            .filter(|p| matches!(p, Platform::GameCube | Platform::Wii))
            .unwrap_or(Platform::GameCube);
        return Ok(Some(make(
            platform,
            SourceFormat::GameCubeGcz,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }
    if bytes.starts_with(b"RVZ\x01") {
        let platform = hint
            .filter(|p| matches!(p, Platform::GameCube | Platform::Wii))
            .unwrap_or(Platform::GameCube);
        return Ok(Some(make(
            platform,
            SourceFormat::NintendoRvz,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }
    if bytes.starts_with(b"WIA\x01") {
        let platform = hint
            .filter(|p| matches!(p, Platform::GameCube | Platform::Wii))
            .unwrap_or(Platform::GameCube);
        return Ok(Some(make(
            platform,
            SourceFormat::NintendoWia,
            ConfidenceLevel::Definitive,
            one(),
            Vec::new(),
        )));
    }

    if ext == "gdi" {
        let text = String::from_utf8_lossy(&bytes);
        let mut related = one();
        let mut missing = Vec::new();
        let track_count = text
            .lines()
            .next()
            .and_then(|line| line.trim().parse::<usize>().ok())
            .context("invalid GDI track count")?;
        for line in text.lines().skip(1).filter(|line| !line.trim().is_empty()) {
            let filename = if let Some(start) = line.find('"') {
                let rest = &line[start + 1..];
                rest.find('"').map(|end| &rest[..end])
            } else {
                line.split_whitespace().nth(4)
            }
            .context("invalid GDI track record")?;
            let track = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(filename);
            if track.is_file() {
                related.push(track);
            } else {
                missing.push(filename.to_owned());
            }
        }
        ensure!(
            related.len() + missing.len() == track_count + 1,
            "GDI track count does not match descriptor"
        );
        return Ok(Some(make(
            Platform::SegaDreamcast,
            SourceFormat::Gdi,
            ConfidenceLevel::Definitive,
            related,
            missing,
        )));
    }

    if ext == "m3u"
        && matches!(
            hint,
            Some(Platform::SegaCD | Platform::SegaSaturn | Platform::PcEngineCD)
        )
    {
        let text = String::from_utf8_lossy(&bytes);
        let entries: Vec<_> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect();
        ensure!(!entries.is_empty(), "empty M3U disc playlist");
        let missing = entries
            .iter()
            .filter(|entry| {
                !path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(entry)
                    .exists()
            })
            .map(|entry| (*entry).to_owned())
            .collect();
        return Ok(Some(make(
            hint.unwrap(),
            SourceFormat::DiscPlaylist,
            ConfidenceLevel::High,
            one(),
            missing,
        )));
    }

    if bytes.len() > 0x104 && &bytes[0x100..0x104] == b"SEGA" {
        let platform = match hint {
            Some(Platform::Sega32X) => Platform::Sega32X,
            Some(Platform::SegaPico) => Platform::SegaPico,
            _ => Platform::SegaMegaDrive,
        };
        let format = if platform == Platform::Sega32X {
            SourceFormat::Sega32xRom
        } else if ext == "smd" {
            SourceFormat::SegaSmdRom
        } else {
            SourceFormat::SegaMdBinRom
        };
        return Ok(Some(make(
            platform,
            format,
            ConfidenceLevel::High,
            one(),
            Vec::new(),
        )));
    }

    let unique = match ext.as_str() {
        "cpr" if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"AMS!".as_slice()) => {
            Some((Platform::AmstradGx4000, SourceFormat::AmstradGx4000Cpr))
        }
        "bin777" | "ptn777" => Some((
            Platform::EpochCassetteVision,
            SourceFormat::EpochCassetteVisionRom,
        )),
        "cart" => Some((
            Platform::EpochSuperCassetteVision,
            SourceFormat::EpochSuperCassetteVisionRom,
        )),
        "myv" => Some((
            Platform::NichibutsuMyVision,
            SourceFormat::NichibutsuMyVisionRom,
        )),
        "tvc" => Some((
            Platform::ElektorTvGamesComputer,
            SourceFormat::ElektorTvcRom,
        )),
        "arduboy" if bytes.starts_with(b"PK\x03\x04") => {
            Some((Platform::Arduboy, SourceFormat::ArduboyImage))
        }
        "cc" | "c2s" | "c3s" => Some((Platform::DingooA320, SourceFormat::DingooNativeApp)),
        "nuon" => Some((Platform::Nuon, SourceFormat::NuonImage)),
        "pfi" => Some((Platform::AquaplusPEce, SourceFormat::AquaplusPeceImage)),
        "gme" | "mcr" | "mcs" | "pss" => {
            Some((Platform::PocketStation, SourceFormat::PocketStationImage))
        }
        "vms" | "dci" => Some((Platform::SegaVmu, SourceFormat::SegaVmuImage)),
        "n-gage" | "eka2l1" => Some((Platform::NokiaNGage, SourceFormat::NokiaNGagePackage)),
        "uze" => Some((Platform::Uzebox, SourceFormat::UzeboxImage)),
        "wud" | "wux" => Some((Platform::WiiU, SourceFormat::WiiUDiscImage)),
        "wua" => Some((Platform::WiiU, SourceFormat::WiiUArchive)),
        "rpx" | "tmd" => Some((Platform::WiiU, SourceFormat::WiiUExecutable)),
        "smf" | "sgm" | "ssl" => Some((Platform::SunplusNative32, SourceFormat::Native32Package)),
        "mgw" => Some((
            Platform::HandheldElectronicGames,
            SourceFormat::HandheldElectronicImage,
        )),
        "sfc" | "smc" | "swc" | "fig" if valid_snes_header(&bytes) => {
            Some((Platform::SuperNintendo, SourceFormat::SnesRom))
        }
        "bs" if valid_snes_header(&bytes) => {
            Some((Platform::Satellaview, SourceFormat::SatellaviewRom))
        }
        "st" => Some((Platform::SufamiTurbo, SourceFormat::SufamiTurboRom)),
        "vb" | "vboy" => Some((Platform::VirtualBoy, SourceFormat::VirtualBoyRom)),
        "min" => Some((Platform::PokemonMini, SourceFormat::PokemonMiniRom)),
        "sms" if has_sega_8bit_header(&bytes) => {
            Some((Platform::SegaMasterSystem, SourceFormat::SegaSmsRom))
        }
        "gg" if has_sega_8bit_header(&bytes) => {
            Some((Platform::SegaGameGear, SourceFormat::SegaGameGearRom))
        }
        "sg" => Some((Platform::SegaSG1000, SourceFormat::SegaSgRom)),
        "pce" => Some((Platform::PcEngine, SourceFormat::PcEngineRom)),
        "sgx" => Some((Platform::SuperGrafx, SourceFormat::SuperGrafxRom)),
        "neo" => Some((Platform::NeoGeoAESMVS, SourceFormat::NeoGeoRom)),
        "ngp" => Some((Platform::NeoGeoPocket, SourceFormat::NeoGeoPocketRom)),
        "ngc" | "ngpc" => Some((
            Platform::NeoGeoPocketColor,
            SourceFormat::NeoGeoPocketColorRom,
        )),
        "a26" => Some((Platform::Atari2600, SourceFormat::Atari2600Rom)),
        "a52" => Some((Platform::Atari5200, SourceFormat::Atari5200Rom)),
        "a78" if bytes.starts_with(b"ATARI7800") => {
            Some((Platform::Atari7800, SourceFormat::Atari7800Rom))
        }
        "lnx" if bytes.starts_with(b"LYNX") => {
            Some((Platform::AtariLynx, SourceFormat::AtariLynxRom))
        }
        "cof" if hint == Some(Platform::Nuon) => Some((Platform::Nuon, SourceFormat::NuonImage)),
        "j64" | "jag" | "abs" | "cof" | "prg" => {
            Some((Platform::AtariJaguar, SourceFormat::AtariJaguarRom))
        }
        "ws" => Some((Platform::WonderSwan, SourceFormat::WonderSwanRom)),
        "wsc" => Some((Platform::WonderSwanColor, SourceFormat::WonderSwanColorRom)),
        "col" if hint == Some(Platform::VTechCreatiVision) => Some((
            Platform::VTechCreatiVision,
            SourceFormat::VTechCreatiVisionRom,
        )),
        "col" => Some((Platform::ColecoVision, SourceFormat::ColecoRom)),
        "int" => Some((Platform::Intellivision, SourceFormat::IntellivisionRom)),
        "vec" => Some((Platform::Vectrex, SourceFormat::VectrexRom)),
        "chf" => Some((
            Platform::FairchildChannelF,
            SourceFormat::FairchildChannelFRom,
        )),
        "sv" => Some((
            Platform::WataraSupervision,
            SourceFormat::WataraSupervisionRom,
        )),
        "ndd" => Some((Platform::Nintendo64DD, SourceFormat::N64DdImage)),
        _ => None,
    };
    if let Some((platform, format)) = unique {
        ensure!(!bytes.is_empty(), "ROM/image is empty");
        return Ok(Some(make(
            platform,
            format,
            ConfidenceLevel::Medium,
            one(),
            Vec::new(),
        )));
    }

    if let Some(platform) = hint {
        let format = match (platform, ext.as_str()) {
            (Platform::SuperNintendo, "sfc" | "smc" | "swc" | "fig")
                if valid_snes_header(&bytes) =>
            {
                Some(SourceFormat::SnesRom)
            }
            (Platform::Satellaview, "bs") if valid_snes_header(&bytes) => {
                Some(SourceFormat::SatellaviewRom)
            }
            (Platform::SufamiTurbo, "st") => Some(SourceFormat::SufamiTurboRom),
            (Platform::VirtualBoy, "vb" | "vboy" | "bin") => Some(SourceFormat::VirtualBoyRom),
            (Platform::PokemonMini, "min") => Some(SourceFormat::PokemonMiniRom),
            (Platform::SegaSG1000, "sg" | "rom" | "bin") => Some(SourceFormat::SegaSgRom),
            (Platform::SegaMasterSystem, "sms" | "rom" | "bin") if has_sega_8bit_header(&bytes) => {
                Some(SourceFormat::SegaSmsRom)
            }
            (Platform::SegaGameGear, "gg" | "rom" | "bin") if has_sega_8bit_header(&bytes) => {
                Some(SourceFormat::SegaGameGearRom)
            }
            (Platform::PcEngine, "pce") => Some(SourceFormat::PcEngineRom),
            (Platform::SuperGrafx, "sgx" | "pce") => Some(SourceFormat::SuperGrafxRom),
            (Platform::NeoGeoAESMVS, "neo") => Some(SourceFormat::NeoGeoRom),
            (Platform::NeoGeoPocket, "ngp") => Some(SourceFormat::NeoGeoPocketRom),
            (Platform::NeoGeoPocketColor, "ngc" | "ngpc") => {
                Some(SourceFormat::NeoGeoPocketColorRom)
            }
            (Platform::Atari2600, "a26" | "bin") => Some(SourceFormat::Atari2600Rom),
            (Platform::Atari5200, "a52" | "bin") => Some(SourceFormat::Atari5200Rom),
            (Platform::Atari7800, "a78" | "bin") if bytes.starts_with(b"ATARI7800") => {
                Some(SourceFormat::Atari7800Rom)
            }
            (Platform::AtariLynx, "lnx") if bytes.starts_with(b"LYNX") => {
                Some(SourceFormat::AtariLynxRom)
            }
            (Platform::AtariLynx, "o" | "bin") => Some(SourceFormat::AtariLynxRom),
            (Platform::AtariJaguar, "j64" | "jag" | "rom" | "abs" | "cof" | "bin" | "prg") => {
                Some(SourceFormat::AtariJaguarRom)
            }
            (Platform::WonderSwan, "ws") => Some(SourceFormat::WonderSwanRom),
            (Platform::WonderSwanColor, "wsc") => Some(SourceFormat::WonderSwanColorRom),
            (Platform::ColecoVision, "col" | "rom" | "bin") => Some(SourceFormat::ColecoRom),
            (Platform::Intellivision, "int" | "rom" | "bin") => {
                Some(SourceFormat::IntellivisionRom)
            }
            (Platform::Odyssey2Videopac, "bin") => Some(SourceFormat::Odyssey2Rom),
            (Platform::Vectrex, "vec" | "bin") => Some(SourceFormat::VectrexRom),
            (Platform::FairchildChannelF, "chf" | "rom" | "bin") => {
                Some(SourceFormat::FairchildChannelFRom)
            }
            (Platform::WataraSupervision, "sv" | "bin") => Some(SourceFormat::WataraSupervisionRom),
            (Platform::MegaDuckCougarBoy, "bin" | "rom") => Some(SourceFormat::MegaDuckRom),
            (Platform::AtariJaguarCD, "cdi") => Some(SourceFormat::Cdi),
            (Platform::SegaDreamcast, "cdi") => Some(SourceFormat::Cdi),
            (Platform::SegaSaturn | Platform::PcEngineCD | Platform::PcFx, "ccd") => {
                Some(SourceFormat::DiscCcd)
            }
            (Platform::PcFx, "toc") => Some(SourceFormat::DiscToc),
            (Platform::PcEngineCD, "img") => Some(SourceFormat::DiscImg),
            (Platform::Nintendo64DD, "ndd") => Some(SourceFormat::N64DdImage),
            (Platform::EpochCassetteVision, "bin" | "bin777" | "ptn777") => {
                Some(SourceFormat::EpochCassetteVisionRom)
            }
            (Platform::EpochSuperCassetteVision, "cart" | "bin" | "rom" | "0") => {
                Some(SourceFormat::EpochSuperCassetteVisionRom)
            }
            (Platform::VTechCreatiVision, "col" | "rom" | "bin") => {
                Some(SourceFormat::VTechCreatiVisionRom)
            }
            (Platform::NichibutsuMyVision, "myv" | "rom" | "bin") => {
                Some(SourceFormat::NichibutsuMyVisionRom)
            }
            (Platform::AtariXegs, "car" | "rom" | "bin" | "xex") => {
                Some(SourceFormat::AtariXegsImage)
            }
            (Platform::EmersonArcadia2001, "bin") => Some(SourceFormat::Arcadia2001Rom),
            (Platform::IntertonVc4000, "bin") => Some(SourceFormat::IntertonVc4000Rom),
            (Platform::ElektorTvGamesComputer, "tvc" | "bin") => Some(SourceFormat::ElektorTvcRom),
            (Platform::Arduboy, "hex") if bytes.starts_with(b":") => {
                Some(SourceFormat::ArduboyImage)
            }
            (Platform::Arduboy, "arduboy") if bytes.starts_with(b"PK\x03\x04") => {
                Some(SourceFormat::ArduboyImage)
            }
            (Platform::DingooA320, "app" | "cc" | "c2s" | "c3s") => {
                Some(SourceFormat::DingooNativeApp)
            }
            (Platform::Nuon, "run" | "cof" | "nuon" | "cd" | "iso" | "img") => {
                Some(SourceFormat::NuonImage)
            }
            (Platform::PocketStation, "bin" | "gme" | "mcr" | "mcs" | "pss") => {
                Some(SourceFormat::PocketStationImage)
            }
            (Platform::SegaVmu, "vms" | "dci" | "bin") => Some(SourceFormat::SegaVmuImage),
            (Platform::NokiaNGage, "sis" | "sisx" | "n-gage" | "eka2l1") => {
                Some(SourceFormat::NokiaNGagePackage)
            }
            (Platform::TamagotchiP1, "b" | "bin" | "rom") => Some(SourceFormat::TamagotchiRom),
            (Platform::WiiU, "iso") => Some(SourceFormat::WiiUDiscImage),
            (Platform::WiiU, "elf") => Some(SourceFormat::WiiUExecutable),
            (Platform::SunplusSpmp8000, "bin") => Some(SourceFormat::Spmp8000Rom),
            _ => None,
        };
        if let Some(format) = format {
            ensure!(!bytes.is_empty(), "ROM/image is empty");
            return Ok(Some(make(
                platform,
                format,
                ConfidenceLevel::Medium,
                one(),
                Vec::new(),
            )));
        }
    }
    Ok(None)
}

fn valid_snes_header(bytes: &[u8]) -> bool {
    let copier = if bytes.len() % 1024 == 512 { 512 } else { 0 };
    [0x7fc0usize, 0xffc0, 0x40ffc0].into_iter().any(|base| {
        let offset = base + copier;
        if bytes.len() < offset + 0x20 {
            return false;
        }
        let complement = u16::from_le_bytes([bytes[offset + 0x1c], bytes[offset + 0x1d]]);
        let checksum = u16::from_le_bytes([bytes[offset + 0x1e], bytes[offset + 0x1f]]);
        checksum != 0 && (checksum ^ complement) == 0xffff
    })
}

fn has_sega_8bit_header(bytes: &[u8]) -> bool {
    [0x1ff0usize, 0x3ff0, 0x7ff0]
        .into_iter()
        .any(|offset| bytes.get(offset..offset + 8) == Some(b"TMR SEGA"))
}

fn has_ufs2_superblock(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    [65_536u64 + 1_372, 8_192 + 1_372, 1_372]
        .into_iter()
        .any(|offset| {
            let mut magic = [0u8; 4];
            file.seek(SeekFrom::Start(offset)).is_ok()
                && file.read_exact(&mut magic).is_ok()
                && u32::from_le_bytes(magic) == 0x1954_0119
        })
}

fn has_pfsc_magic(path: &Path) -> bool {
    has_leading_magic(path, b"PFSC")
}

fn has_pfs_magic(path: &Path) -> bool {
    has_leading_magic(path, b"PFS\0") || has_leading_magic(path, b"PFS ")
}

fn has_leading_magic(path: &Path, magic: &[u8]) -> bool {
    let mut bytes = vec![0u8; magic.len()];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok()
        && bytes == magic
}

fn is_archive_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ext.eq_ignore_ascii_case("zip")
                || ext.eq_ignore_ascii_case("7z")
                || ext.eq_ignore_ascii_case("rar")
                || ext.eq_ignore_ascii_case("tar")
                || ext.eq_ignore_ascii_case("gz")
        })
}

struct MultipartCheckResult {
    present_files: Vec<PathBuf>,
    missing_parts: Vec<String>,
}

fn check_multipart_archive(file_path: &Path) -> Option<MultipartCheckResult> {
    let name = file_path.file_name()?.to_str()?;
    let parent = file_path.parent().unwrap_or_else(|| Path::new("."));

    // Check .7z.001 / .zip.001 pattern
    if name.ends_with(".001") {
        let prefix = &name[..name.len() - 4];
        let mut present = Vec::new();
        let mut missing = Vec::new();
        let mut index = 1;
        loop {
            let part_name = format!("{prefix}.{:03}", index);
            let part_path = parent.join(&part_name);
            if part_path.is_file() {
                present.push(part_path);
                index += 1;
            } else {
                let next_name = format!("{prefix}.{:03}", index + 1);
                if parent.join(&next_name).is_file() {
                    missing.push(part_name);
                    index += 1;
                } else {
                    break;
                }
            }
        }
        if present.len() > 1 || !missing.is_empty() {
            return Some(MultipartCheckResult {
                present_files: present,
                missing_parts: missing,
            });
        }
    }

    // Check part1.rar, .part1.rar, _part1.rar, -part1.rar pattern
    let lower_name = name.to_ascii_lowercase();
    let part_info = if let Some(part_idx) = lower_name.find(".part") {
        Some((part_idx + 5, &name[..part_idx + 5]))
    } else if let Some(part_idx) = lower_name.find("_part") {
        Some((part_idx + 5, &name[..part_idx + 5]))
    } else if let Some(part_idx) = lower_name.find("-part") {
        Some((part_idx + 5, &name[..part_idx + 5]))
    } else if lower_name.starts_with("part") {
        Some((4, &name[..4]))
    } else {
        None
    };
    if let Some((digits_start, prefix)) = part_info {
        let suffix = &name[digits_start..];
        if let Some(dot_rar) = suffix.to_ascii_lowercase().find(".rar") {
            let digits = &suffix[..dot_rar];
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                let width = digits.len();
                let mut present = Vec::new();
                let mut missing = Vec::new();
                let mut index = 1;
                loop {
                    let part_name = format!("{prefix}{:0width$}.rar", index, width = width);
                    let part_path = parent.join(&part_name);
                    if part_path.is_file() {
                        present.push(part_path);
                        index += 1;
                    } else {
                        let next_name = format!("{prefix}{:0width$}.rar", index + 1, width = width);
                        if parent.join(&next_name).is_file() {
                            missing.push(part_name);
                            index += 1;
                        } else {
                            break;
                        }
                    }
                }
                return Some(MultipartCheckResult {
                    present_files: present,
                    missing_parts: missing,
                });
            }
        }
    }

    None
}

fn check_split_iso(file_path: &Path) -> Option<(Vec<PathBuf>, Vec<String>)> {
    let name = file_path.file_name()?.to_str()?;
    let parent = file_path.parent().unwrap_or_else(|| Path::new("."));
    if name.contains(".iso.") {
        let prefix = name.split(".iso.").next()?;
        let mut present = Vec::new();
        let mut missing = Vec::new();
        let mut index = 0;
        loop {
            let part_name = format!("{prefix}.iso.{index}");
            let part_path = parent.join(&part_name);
            if part_path.is_file() {
                present.push(part_path);
                index += 1;
            } else {
                let next_name = format!("{prefix}.iso.{}", index + 1);
                if parent.join(&next_name).is_file() {
                    missing.push(part_name);
                    index += 1;
                } else {
                    break;
                }
            }
        }
        if !present.is_empty() {
            return Some((present, missing));
        }
    }
    // PS3 disc dumps commonly use .66600, .66601, ... parts. They are
    // sequential raw ISO chunks, not numbered archive files.
    if let Some((prefix, suffix)) = name.rsplit_once(".666") {
        if suffix.len() == 2 && suffix.parse::<u32>().is_ok() {
            let mut present = Vec::new();
            let mut missing = Vec::new();
            let mut index = 0;
            loop {
                let part_name = format!("{prefix}.666{index:02}");
                let part_path = parent.join(&part_name);
                if part_path.is_file() {
                    present.push(part_path);
                    index += 1;
                } else {
                    let next_name = format!("{prefix}.666{:02}", index + 1);
                    if parent.join(&next_name).is_file() {
                        missing.push(part_name);
                        index += 1;
                    } else {
                        break;
                    }
                }
            }
            if !present.is_empty() {
                return Some((present, missing));
            }
        }
    }
    None
}

fn check_split_pkg(file_path: &Path) -> Option<(Vec<PathBuf>, Vec<String>)> {
    let name = file_path.file_name()?.to_str()?;
    let parent = file_path.parent().unwrap_or_else(|| Path::new("."));
    if name.contains(".pkg.") {
        let prefix = name.split(".pkg.").next()?;
        let mut present = Vec::new();
        let mut missing = Vec::new();
        let mut index = 0;
        loop {
            let part_name = format!("{prefix}.pkg.{index}");
            let part_path = parent.join(&part_name);
            if part_path.is_file() {
                present.push(part_path);
                index += 1;
            } else {
                let next_name = format!("{prefix}.pkg.{}", index + 1);
                if parent.join(&next_name).is_file() {
                    missing.push(part_name);
                    index += 1;
                } else {
                    break;
                }
            }
        }
        if !present.is_empty() {
            return Some((present, missing));
        }
    }
    None
}

fn parse_disc_marker(name: &str) -> (bool, Option<u32>, Option<u32>) {
    let lower = name.to_ascii_lowercase();
    for marker in &["disc", "disk", "cd", "dvd", "part"] {
        if let Some(pos) = lower.find(marker) {
            let rest = &lower[pos + marker.len()..];
            let trimmed =
                rest.trim_start_matches(|c: char| c == '_' || c == '-' || c == ' ' || c == '#');
            let digits: String = trimmed.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(num) = digits.parse::<u32>() {
                return (true, Some(num), None);
            }
        }
    }
    (false, None, None)
}

fn parse_system_cnf_game_id(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("BOOT") {
            if let Some(colon) = trimmed.find(':') {
                let path_part = &trimmed[colon + 1..];
                let file_part = path_part
                    .trim_start_matches('\\')
                    .split(';')
                    .next()
                    .unwrap_or(path_part);
                return Some(file_part.replace('_', "-").replace('.', ""));
            }
        }
    }
    None
}

pub fn parse_pkg_content_id(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 0x60
        || (!bytes.starts_with(b"\x7FPKG")
            && !bytes.starts_with(b"\x7FCNT")
            && !bytes.starts_with(b"\x7FFIH"))
    {
        return None;
    }
    // PS4 / PS5 packages typically store the 36-byte content ID at offset 0x40.
    // PS3 / PSP packages typically store it at offset 0x30.
    let slice_0x40 = bytes.get(0x40..0x70).or_else(|| bytes.get(0x40..0x64));
    if let Some(slice) = slice_0x40 {
        if let Ok(id) = std::str::from_utf8(slice) {
            let id = id.trim_matches('\0').trim();
            if id.len() >= 12 && id.contains('-') {
                return Some(id.to_owned());
            }
        }
    }
    let raw = bytes.get(0x30..0x60)?;
    let id = std::str::from_utf8(raw).ok()?.trim_matches('\0').trim();
    if id.len() < 12 || !id.contains('-') {
        return None;
    }
    Some(id.to_owned())
}

pub fn title_id_from_content_id(content_id: &str) -> Option<String> {
    let title_id = content_id.split('-').nth(1)?.split('_').next()?;
    if (4..=16).contains(&title_id.len()) && title_id.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        Some(title_id.to_ascii_uppercase())
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub struct SfoEntry {
    pub key: String,
    pub format: u16,
    pub data: Vec<u8>,
}

pub fn parse_sfo_entries(bytes: &[u8]) -> Option<Vec<SfoEntry>> {
    if bytes.len() < 20 || !bytes.starts_with(b"\x00PSF") {
        return None;
    }
    let key_table_offset = u32::from_le_bytes(bytes[8..12].try_into().ok()?) as usize;
    let val_table_offset = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    let entry_count = u32::from_le_bytes(bytes[16..20].try_into().ok()?) as usize;

    if 20 + entry_count * 16 > bytes.len() {
        return None;
    }

    let mut entries = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let entry_offset = 20 + i * 16;
        let key_offset =
            u16::from_le_bytes(bytes[entry_offset..entry_offset + 2].try_into().ok()?) as usize;
        let param_fmt =
            u16::from_le_bytes(bytes[entry_offset + 2..entry_offset + 4].try_into().ok()?);
        let val_len =
            u32::from_le_bytes(bytes[entry_offset + 4..entry_offset + 8].try_into().ok()?) as usize;
        let _val_max_len =
            u32::from_le_bytes(bytes[entry_offset + 8..entry_offset + 12].try_into().ok()?)
                as usize;
        let val_offset = u32::from_le_bytes(
            bytes[entry_offset + 12..entry_offset + 16]
                .try_into()
                .ok()?,
        ) as usize;

        let abs_key_offset = key_table_offset + key_offset;
        if abs_key_offset >= bytes.len() {
            continue;
        }
        let key_end = (abs_key_offset..bytes.len())
            .find(|&idx| bytes[idx] == 0)
            .unwrap_or(bytes.len());
        let key = match std::str::from_utf8(&bytes[abs_key_offset..key_end]) {
            Ok(k) => k.to_string(),
            Err(_) => continue,
        };

        let abs_val_offset = val_table_offset + val_offset;
        if abs_val_offset > bytes.len() {
            continue;
        }
        let end_val_offset = (abs_val_offset + val_len).min(bytes.len());
        let data = bytes[abs_val_offset..end_val_offset].to_vec();

        entries.push(SfoEntry {
            key,
            format: param_fmt,
            data,
        });
    }

    Some(entries)
}

pub fn extract_sfo_string(bytes: &[u8], target_key: &str) -> Option<String> {
    if let Some(entries) = parse_sfo_entries(bytes) {
        for entry in entries {
            if entry.key.eq_ignore_ascii_case(target_key) {
                if entry.format == 0x0404 && entry.data.len() >= 4 {
                    let val = u32::from_le_bytes(entry.data[..4].try_into().unwrap_or_default());
                    return Some(val.to_string());
                }
                if let Ok(s) = std::str::from_utf8(&entry.data) {
                    let trimmed = s.trim_end_matches('\0').trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
        }
    }
    None
}

pub fn extract_ps4_title_id_from_str(s: &str) -> Option<String> {
    let upper = s.to_ascii_uppercase();
    if let Some(pos) = upper.find("CUSA") {
        let remainder = &upper[pos..];
        let digits_start = if remainder.starts_with("CUSA-") || remainder.starts_with("CUSA_") {
            5
        } else {
            4
        };
        if remainder.len() >= digits_start + 5 {
            let candidate_digits = &remainder[digits_start..digits_start + 5];
            if candidate_digits.chars().all(|c| c.is_ascii_digit()) {
                return Some(format!("CUSA{candidate_digits}"));
            }
        }
    }
    None
}
