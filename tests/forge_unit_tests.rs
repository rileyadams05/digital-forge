use std::fs;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use iso2god::forge::ManualImportMenuState;
use iso2god::forge::backend::ProgressUpdate;
use iso2god::forge::backends::get_backend_for_id;
use iso2god::forge::catalog::{COMPUTER_ONLY_EXCLUSIONS, PLATFORM_CATALOG};
use iso2god::forge::detector::{DetectionResult, FormatDetector};
use iso2god::forge::format::{ConfidenceLevel, LicenseStatus, SourceFormat, TargetFormat};
use iso2god::forge::matrix::{ALL_RULES, FormatMatrix, SupportStatus};
use iso2god::forge::platform::Platform;
use iso2god::forge::profile::{
    ConsoleStorage, TargetProfile, TargetWorkflow, UsageMode, target_workflow_for_console_storage,
    target_workflow_for_usage, target_workflows,
};
use iso2god::forge::{ForgeUi, TempWorkspace};
use sha2::{Digest, Sha256};

fn detection(
    platform: Platform,
    source_format: SourceFormat,
    files: Vec<std::path::PathBuf>,
) -> DetectionResult {
    DetectionResult {
        platform,
        source_format,
        package_classification: None,
        confidence: ConfidenceLevel::Definitive,
        game_id: None,
        title_name: None,
        is_multipart: files.len() > 1,
        is_archive: false,
        is_multidisc: false,
        disc_number: None,
        total_discs: None,
        related_files: files,
        missing_parts: Vec::new(),
        license_status: LicenseStatus::NotApplicable,
    }
}

#[test]
fn every_selectable_matrix_direction_has_a_backend_operation() {
    for rule in ALL_RULES
        .iter()
        .filter(|rule| rule.support_status.selectable())
    {
        let backend = get_backend_for_id(rule.backend_id)
            .unwrap_or_else(|| panic!("missing backend {}", rule.backend_id));
        let input = detection(rule.platform, rule.source_format, Vec::new());
        assert!(
            backend
                .available_outputs(&input)
                .contains(&rule.target_format),
            "{} does not expose {:?} -> {:?}",
            rule.backend_id,
            rule.source_format,
            rule.target_format
        );
        assert!(!rule.operation.is_empty());
        assert!(!rule.lossless_or_transforming.is_empty());
    }
}

#[test]
fn frontend_only_exposes_exact_supported_source_directions() {
    let input = detection(Platform::Psp, SourceFormat::Pbp, Vec::new());
    let choices = ForgeUi::build_format_choices(&input, Platform::Psp, TargetProfile::PspIso);
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0].target_format, TargetFormat::PspPbp);
    assert!(choices[0].is_passthrough);

    for platform in Platform::ALL {
        for source in ALL_RULES
            .iter()
            .filter(|rule| rule.platform == platform)
            .map(|rule| rule.source_format)
        {
            let input = detection(platform, source, Vec::new());
            for choice in
                ForgeUi::build_format_choices(&input, platform, platform.default_profile())
            {
                assert!(choice.rule.support_status.selectable());
            }
        }
    }
}

#[test]
#[cfg(feature = "deferred-console-integration-tests")]
fn ps3_exposes_disc_image_paths_and_disables_unverified_pkg_creation() {
    let expected = [
        (SourceFormat::Ps3JbFolder, &[TargetFormat::Ps3Iso][..]),
        (SourceFormat::Ps3Iso, &[TargetFormat::Ps3JbFolder][..]),
        (SourceFormat::Ps3SplitIso, &[TargetFormat::Ps3JbFolder][..]),
        (SourceFormat::Ps3Pkg, &[TargetFormat::Ps3Pkg][..]),
    ];

    for (source, expected_targets) in expected {
        let choices = FormatMatrix::get_rules_for_source(Platform::PlayStation3, source);
        assert!(!choices.is_empty(), "no PS3 rule for {source:?}");
        let actual_targets: Vec<_> = choices.iter().map(|rule| rule.target_format).collect();
        for target in expected_targets {
            assert!(
                actual_targets.contains(target),
                "PS3 source {source:?} is missing target {target:?}: {choices:?}"
            );
        }
    }

    for source in [
        SourceFormat::Ps3JbFolder,
        SourceFormat::Ps3Iso,
        SourceFormat::Ps3SplitIso,
    ] {
        assert!(
            FormatMatrix::find_rule(Platform::PlayStation3, source, TargetFormat::Ps3Pkg)
                .is_some_and(|rule| !rule.support_status.selectable()),
            "PS3 package creation must remain unavailable until the replacement packager is verified"
        );
    }
}

#[test]
#[cfg(feature = "deferred-console-integration-tests")]
fn ps3_detects_numbered_666_split_iso_parts_and_missing_parts() {
    let root = std::env::temp_dir().join(format!("digital-forge-ps3-666-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("game.66600"), b"part0").unwrap();
    fs::write(root.join("game.66602"), b"part2").unwrap();

    let detected =
        FormatDetector::detect(&root.join("game.66600"), Some(Platform::PlayStation3)).unwrap();
    assert_eq!(detected.source_format, SourceFormat::Ps3SplitIso);
    assert_eq!(detected.related_files.len(), 2);
    assert_eq!(detected.missing_parts, vec!["game.66601"]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[cfg(feature = "deferred-console-integration-tests")]
fn ps3_iso_utilities_round_trip_synthetic_jb_game_and_split_iso() {
    use std::sync::atomic::AtomicBool;

    let extract = std::env::var_os("DIGITAL_FORGE_TOOL_PS3_EXTRACT_ISO")
        .map(std::path::PathBuf::from)
        .expect("set DIGITAL_FORGE_TOOL_PS3_EXTRACT_ISO to extractps3iso.exe");
    let generate = std::env::var_os("DIGITAL_FORGE_TOOL_PS3_GENERATE_ISO")
        .map(std::path::PathBuf::from)
        .expect("set DIGITAL_FORGE_TOOL_PS3_GENERATE_ISO to makeps3iso.exe");
    assert!(extract.is_file());
    assert!(generate.is_file());

    fn sfo(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let mut table = Vec::new();
        for (key, value) in entries {
            let key_offset = keys.len() as u16;
            keys.extend_from_slice(key.as_bytes());
            keys.push(0);
            let value_offset = values.len() as u32;
            values.extend_from_slice(value.as_bytes());
            values.push(0);
            let value_length = values.len() as u32 - value_offset;
            while values.len() % 4 != 0 {
                values.push(0);
            }
            table.extend_from_slice(&key_offset.to_le_bytes());
            table.extend_from_slice(&0x0204u16.to_le_bytes());
            table.extend_from_slice(&value_length.to_le_bytes());
            table.extend_from_slice(&value_length.to_le_bytes());
            table.extend_from_slice(&value_offset.to_le_bytes());
        }
        let key_table_offset = 20u32 + table.len() as u32;
        let value_table_offset = key_table_offset + keys.len() as u32;
        let mut result = b"\0PSF".to_vec();
        result.extend_from_slice(&0x00000101u32.to_le_bytes());
        result.extend_from_slice(&key_table_offset.to_le_bytes());
        result.extend_from_slice(&value_table_offset.to_le_bytes());
        result.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        result.extend_from_slice(&table);
        result.extend_from_slice(&keys);
        result.extend_from_slice(&values);
        result
    }

    let workspace = TempWorkspace::new("ps3_iso_roundtrip").unwrap();
    let game = workspace.root.join("synthetic_game");
    fs::create_dir_all(game.join("PS3_GAME/USRDIR")).unwrap();
    fs::write(
        game.join("PS3_GAME/PARAM.SFO"),
        sfo(&[
            ("TITLE", "Digital Forge fixture"),
            ("TITLE_ID", "BCUS12345"),
        ]),
    )
    .unwrap();
    fs::write(
        game.join("PS3_GAME/USRDIR/fixture.dat"),
        b"synthetic PS3 fixture payload",
    )
    .unwrap();
    fs::write(game.join("PS3_DISC.SFB"), b"synthetic disc marker").unwrap();

    let backend = get_backend_for_id("ps3_iso").unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let no_progress = Arc::new(|_: ProgressUpdate| {});
    let jb_input = detection(
        Platform::PlayStation3,
        SourceFormat::Ps3JbFolder,
        vec![game.clone()],
    );
    let iso_output = workspace.root.join("first_iso");
    let built = backend
        .convert(
            &jb_input,
            TargetFormat::Ps3Iso,
            &iso_output,
            no_progress.clone(),
            cancelled.clone(),
        )
        .unwrap();
    assert!(built.output_path.is_file());
    assert!(fs::metadata(&built.output_path).unwrap().len() > 0);

    let split_dir = workspace.root.join("split");
    fs::create_dir_all(&split_dir).unwrap();
    let iso_bytes = fs::read(&built.output_path).unwrap();
    let midpoint = iso_bytes.len() / 2;
    let part0 = split_dir.join("synthetic.iso.0");
    let part1 = split_dir.join("synthetic.iso.1");
    fs::write(&part0, &iso_bytes[..midpoint]).unwrap();
    fs::write(&part1, &iso_bytes[midpoint..]).unwrap();
    let split_input = detection(
        Platform::PlayStation3,
        SourceFormat::Ps3SplitIso,
        vec![part0, part1],
    );
    let extracted_output = workspace.root.join("split_extracted");
    let extracted = backend
        .convert(
            &split_input,
            TargetFormat::Ps3JbFolder,
            &extracted_output,
            no_progress,
            cancelled,
        )
        .unwrap();
    assert!(extracted.output_path.join("PS3_GAME/PARAM.SFO").is_file());
    assert_eq!(
        fs::read(extracted.output_path.join("PS3_GAME/USRDIR/fixture.dat")).unwrap(),
        b"synthetic PS3 fixture payload"
    );
}

#[test]
fn ps4_pkg_input_recommends_ps4_folder_over_pkg() {
    let input = detection(Platform::PlayStation4, SourceFormat::Ps4FakePkg, Vec::new());
    let choices = ForgeUi::build_format_choices(
        &input,
        Platform::PlayStation4,
        Platform::PlayStation4.default_profile(),
    );
    assert_eq!(choices.len(), 3);
    assert_eq!(choices[0].target_format, TargetFormat::Ps4Folder);
    assert!(choices[0].is_recommended);
    let target_formats: Vec<_> = choices.iter().map(|c| c.target_format).collect();
    assert!(target_formats.contains(&TargetFormat::Ps4Folder));
    assert!(target_formats.contains(&TargetFormat::Ps4FakePkg));
    assert!(target_formats.contains(&TargetFormat::Ps4RetailPkg));
}

#[test]
fn ps4_all_source_formats_expose_three_distinct_output_choices() {
    let test_sources = [
        SourceFormat::Ps4FakePkg,
        SourceFormat::Ps4RetailPkg,
        SourceFormat::Ps4Folder,
        SourceFormat::Ps4SplitPkg,
        SourceFormat::Archive,
        SourceFormat::Unknown,
    ];

    for source in test_sources {
        let input = detection(Platform::PlayStation4, source, Vec::new());
        let choices = ForgeUi::build_format_choices(
            &input,
            Platform::PlayStation4,
            Platform::PlayStation4.default_profile(),
        );
        assert_eq!(
            choices.len(),
            3,
            "Source {:?} should produce exactly 3 internal format options (Loose Folder, FPKG, Retail PKG)",
            source
        );
        let target_formats: Vec<_> = choices.iter().map(|c| c.target_format).collect();
        assert!(target_formats.contains(&TargetFormat::Ps4Folder));
        assert!(target_formats.contains(&TargetFormat::Ps4FakePkg));
        assert!(target_formats.contains(&TargetFormat::Ps4RetailPkg));
    }
}

#[test]
fn ps4_magic_alone_is_not_accepted_as_a_valid_package() {
    let workspace = TempWorkspace::new("ps4_direct_passthrough").unwrap();
    let source = workspace.root.join("game.pkg");
    let mut package = vec![0u8; 4096];
    package[..4].copy_from_slice(b"\x7fPKG");
    fs::write(&source, package).unwrap();
    let dlc = workspace.root.join("dlc.pkg");
    fs::copy(&source, &dlc).unwrap();
    let input = detection(
        Platform::PlayStation4,
        SourceFormat::Ps4FakePkg,
        vec![source.clone(), dlc.clone()],
    );
    let output_dir = workspace.root.join("staged");
    let backend = get_backend_for_id("ps4_liborbispkg").unwrap();
    let error = backend
        .convert(
            &input,
            TargetFormat::Ps4FakePkg,
            &output_dir,
            Arc::new(|_: ProgressUpdate| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();

    assert!(format!("{error:#}").contains("1 MiB minimum"));
    assert!(fs::read_dir(output_dir).unwrap().next().is_none());
}

#[test]
fn ps4_orbis_pkgtool_backend_builds_and_validates_base_and_update_fpkg() {
    let tool = iso2god::forge::backends::find_native_orbis_pkg_tool().unwrap_or_else(|| {
        panic!(
            "managed OrbisPkgTool runtime was not found; checked the app runtime and repository-managed PS4 tool directories"
        )
    });
    assert!(tool.is_file(), "OrbisPkgTool not found: {}", tool.display());

    fn make_game(root: &std::path::Path, tool: &std::path::Path, category: &str) {
        fs::create_dir_all(root.join("sce_sys")).unwrap();
        fs::write(
            root.join("eboot.bin"),
            b"synthetic non-launchable packaging fixture",
        )
        .unwrap();
        let sfo = std::process::Command::new(tool)
            .args([
                "sfo",
                "create",
                root.join("sce_sys/param.sfo").to_str().unwrap(),
                "--title",
                "Synthetic Digital Forge fixture",
                "--title-id",
                "CUSA00001",
                "--content-id",
                "UP0001-CUSA00001_00-DIGITALFORGE0001",
                "--category",
                category,
            ])
            .output()
            .unwrap();
        assert!(
            sfo.status.success(),
            "OrbisPkgTool SFO fixture creation failed: {}{}",
            String::from_utf8_lossy(&sfo.stdout),
            String::from_utf8_lossy(&sfo.stderr)
        );
        set_sfo(tool, &root.join("sce_sys/param.sfo"), "CATEGORY", category);
    }

    fn set_sfo(tool: &std::path::Path, path: &std::path::Path, key: &str, value: &str) {
        let result = std::process::Command::new(tool)
            .args(["sfo", "set", path.to_str().unwrap(), key, value])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "OrbisPkgTool could not set {key}: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    let workspace = TempWorkspace::new("ps4_orbis_pkgtool_integration").unwrap();
    let backend = get_backend_for_id("ps4_liborbispkg").unwrap();
    for (category, label) in [("gd", "base"), ("gp", "update")] {
        let game = workspace.root.join(format!("{label}_game"));
        let output = workspace.root.join(format!("{label}_output"));
        make_game(&game, &tool, category);
        let input = detection(Platform::PlayStation4, SourceFormat::Ps4Folder, vec![game]);
        let result = backend
            .convert(
                &input,
                TargetFormat::Ps4FakePkg,
                &output,
                Arc::new(|_: ProgressUpdate| {}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap_or_else(|error| panic!("synthetic PS4 {label} build failed: {error:#}"));
        assert!(result.output_path.is_file());
        assert!(fs::metadata(&result.output_path).unwrap().len() >= 1024 * 1024);
        assert!(backend.validate_output(&result).unwrap().is_valid);
        assert!(
            result
                .tool_report
                .as_ref()
                .is_some_and(|report| report.upstream_version.contains("OrbisPkgTool"))
        );
        let info = std::process::Command::new(&tool)
            .arg("info")
            .arg(&result.output_path)
            .output()
            .unwrap();
        assert!(
            info.status.success(),
            "OrbisPkgTool could not read generated {label} package metadata: {}{}",
            String::from_utf8_lossy(&info.stdout),
            String::from_utf8_lossy(&info.stderr)
        );
        let info_text = format!(
            "{}{}",
            String::from_utf8_lossy(&info.stdout),
            String::from_utf8_lossy(&info.stderr)
        );
        assert!(info_text.contains("CUSA00001"), "{info_text}");

        if label == "base" {
            let extracted = workspace.root.join("base_extracted");
            let pkg_input = detection(
                Platform::PlayStation4,
                SourceFormat::Ps4FakePkg,
                vec![result.output_path.clone()],
            );
            let extracted_result = backend
                .convert(
                    &pkg_input,
                    TargetFormat::Ps4Folder,
                    &extracted,
                    Arc::new(|_: ProgressUpdate| {}),
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap_or_else(|error| panic!("synthetic PS4 base extraction failed: {error:#}"));
            let extracted_sfo = extracted_result.output_path.join("sce_sys/param.sfo");
            assert!(extracted_result.output_path.join("eboot.bin").is_file());
            assert!(extracted_sfo.is_file());
            assert_eq!(
                FormatDetector::read_sfo(&extracted_sfo).0.as_deref(),
                Some("CUSA00001")
            );
        }
    }

    // Exercise the same base-identity / patch-version merge used by the PS4
    // workflow, then package and inspect the merged result with OrbisPkgTool.
    let merge_base = workspace.root.join("merge_base");
    let merge_update = workspace.root.join("merge_update");
    make_game(&merge_base, &tool, "gd");
    make_game(&merge_update, &tool, "gp");
    let base_sfo = merge_base.join("sce_sys/param.sfo");
    let update_sfo = merge_update.join("sce_sys/param.sfo");
    set_sfo(&tool, &base_sfo, "APP_VER", "01.00");
    set_sfo(&tool, &base_sfo, "VERSION", "01.00");
    set_sfo(&tool, &update_sfo, "APP_VER", "01.05");
    set_sfo(&tool, &update_sfo, "VERSION", "01.05");
    let patch_data = merge_update.join("patch-fixture.dat");
    fs::write(&patch_data, b"synthetic update overlay").unwrap();
    fs::copy(&patch_data, merge_base.join("patch-fixture.dat")).unwrap();
    iso2god::forge::ps4::normalize_itemzflow_update_sfo(&base_sfo, &update_sfo, &base_sfo)
        .expect("matching base/update metadata should merge");
    assert_eq!(
        FormatDetector::read_sfo_key(&base_sfo, "APP_VER").as_deref(),
        Some("01.05")
    );
    assert_eq!(
        FormatDetector::read_sfo_category(&base_sfo).as_deref(),
        Some("gd")
    );
    assert_eq!(
        FormatDetector::read_sfo_content_id(&base_sfo).as_deref(),
        Some("UP0001-CUSA00001_00-DIGITALFORGE0001")
    );
    let merged_output = workspace.root.join("merged_output");
    let merged_input = detection(
        Platform::PlayStation4,
        SourceFormat::Ps4Folder,
        vec![merge_base],
    );
    let merged_result = backend
        .convert(
            &merged_input,
            TargetFormat::Ps4FakePkg,
            &merged_output,
            Arc::new(|_: ProgressUpdate| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_or_else(|error| panic!("synthetic PS4 base/update integration failed: {error:#}"));
    assert!(backend.validate_output(&merged_result).unwrap().is_valid);
    let merged_info = std::process::Command::new(&tool)
        .arg("info")
        .arg(&merged_result.output_path)
        .output()
        .unwrap();
    assert!(
        merged_info.status.success(),
        "OrbisPkgTool could not reopen merged FPKG metadata: {}{}",
        String::from_utf8_lossy(&merged_info.stdout),
        String::from_utf8_lossy(&merged_info.stderr)
    );
}

#[test]
fn removed_routes_are_not_selectable() {
    assert!(
        FormatMatrix::find_rule(Platform::Psp, SourceFormat::Pbp, TargetFormat::PspIso).is_none()
    );
    assert!(
        FormatMatrix::find_rule(Platform::Psp, SourceFormat::DiscIso, TargetFormat::PspJso)
            .is_none()
    );
    assert_eq!(
        FormatMatrix::find_rule(
            Platform::PlayStation2,
            SourceFormat::Ps2UsbExtreme,
            TargetFormat::Ps2Iso
        )
        .unwrap()
        .support_status,
        SupportStatus::Unsupported
    );
    assert_eq!(
        FormatMatrix::find_rule(
            Platform::PlayStation5,
            SourceFormat::Ps5SplitBackup,
            TargetFormat::Ps5Pkg
        )
        .unwrap()
        .support_status,
        SupportStatus::Unsupported
    );
}

#[test]
fn cue_detection_preserves_multibin_track_order_and_rejects_missing_tracks() {
    let workspace = TempWorkspace::new("cue_fixture").unwrap();
    let cue = workspace.root.join("disc.cue");
    fs::write(workspace.root.join("track01.bin"), vec![0u8; 2352]).unwrap();
    fs::write(workspace.root.join("track02.bin"), vec![1u8; 2352]).unwrap();
    fs::write(
        &cue,
        "FILE \"track01.bin\" BINARY\r\n  TRACK 01 MODE2/2352\r\n    INDEX 01 00:00:00\r\nFILE \"track02.bin\" BINARY\r\n  TRACK 02 AUDIO\r\n    INDEX 01 00:00:00\r\n",
    )
    .unwrap();
    let found = FormatDetector::detect(&cue, Some(Platform::PlayStation1)).unwrap();
    assert_eq!(found.source_format, SourceFormat::BinCue);
    assert_eq!(found.related_files[0], cue);
    assert_eq!(found.related_files[1].file_name().unwrap(), "track01.bin");
    assert_eq!(found.related_files[2].file_name().unwrap(), "track02.bin");
    assert!(found.missing_parts.is_empty());

    fs::remove_file(workspace.root.join("track02.bin")).unwrap();
    let missing = FormatDetector::detect(&cue, Some(Platform::PlayStation1)).unwrap();
    assert_eq!(missing.missing_parts, vec!["track02.bin"]);
}

fn write_pbp(path: &std::path::Path, ps1: bool) {
    let mut bytes = vec![0u8; 160];
    bytes[0..4].copy_from_slice(b"\0PBP");
    bytes[4..8].copy_from_slice(&0x0001_0000u32.to_le_bytes());
    for (index, offset) in (40u32..=110).step_by(10).enumerate() {
        let start = 8 + index * 4;
        bytes[start..start + 4].copy_from_slice(&offset.to_le_bytes());
    }
    if ps1 {
        bytes[110..118].copy_from_slice(b"PSISOIMG");
    } else {
        bytes[110..118].copy_from_slice(b"PSPIMAGE");
    }
    fs::write(path, bytes).unwrap();
}

#[test]
fn pbp_detection_distinguishes_ps1_classics_from_psp_native() {
    let workspace = TempWorkspace::new("pbp_fixture").unwrap();
    let ps1 = workspace.root.join("ps1.pbp");
    let psp = workspace.root.join("psp.pbp");
    write_pbp(&ps1, true);
    write_pbp(&psp, false);
    assert_eq!(
        FormatDetector::detect(&ps1, None).unwrap().platform,
        Platform::PlayStation1
    );
    assert_eq!(
        FormatDetector::detect(&psp, None).unwrap().platform,
        Platform::Psp
    );
    assert!(FormatDetector::detect(&psp, Some(Platform::PlayStation1)).is_err());
}

#[test]
fn ps5_filesystem_detection_is_content_aware() {
    let workspace = TempWorkspace::new("ps5_fs_fixture").unwrap();
    let fake = workspace.root.join("fake.exfat");
    fs::write(&fake, vec![0u8; 512]).unwrap();
    assert_ne!(
        FormatDetector::detect(&fake, Some(Platform::PlayStation5))
            .unwrap()
            .source_format,
        SourceFormat::ShadowMountExfatImage
    );

    let real = workspace.root.join("real.exfat");
    let mut sector = vec![0u8; 512];
    sector[3..11].copy_from_slice(b"EXFAT   ");
    sector[510] = 0x55;
    sector[511] = 0xaa;
    fs::write(&real, sector).unwrap();
    assert_eq!(
        FormatDetector::detect(&real, None).unwrap().source_format,
        SourceFormat::ShadowMountExfatImage
    );

    let ufs = workspace.root.join("game.ffpkg");
    let mut image = vec![0u8; 65_536 + 1_376];
    image[65_536 + 1_372..65_536 + 1_376].copy_from_slice(&0x1954_0119u32.to_le_bytes());
    fs::write(&ufs, image).unwrap();
    assert_eq!(
        FormatDetector::detect(&ufs, None).unwrap().source_format,
        SourceFormat::ShadowMountFfPkg
    );
}

#[test]
fn usbextreme_writer_matches_the_upstream_64_byte_record_layout() {
    let workspace = TempWorkspace::new("ul_fixture").unwrap();
    let iso = workspace.root.join("game.iso");
    fs::write(&iso, vec![0x5a; 4096]).unwrap();
    let mut input = detection(
        Platform::PlayStation2,
        SourceFormat::DiscIso,
        vec![iso.clone()],
    );
    input.game_id = Some("SLUS_202.65".to_owned());
    input.title_name = Some("Test Game".to_owned());
    let output = workspace.root.join("out");
    let result = get_backend_for_id("ps2_usbextreme")
        .unwrap()
        .convert(
            &input,
            TargetFormat::Ps2UsbExtreme,
            &output,
            Arc::new(|_: ProgressUpdate| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let record = fs::read(&result.output_path).unwrap();
    assert_eq!(record.len(), 64);
    assert_eq!(&record[0..9], b"Test Game");
    assert_eq!(&record[0x20..0x2f], b"ul.SLUS_202.65\0");
    assert_eq!(record[0x2f], 1);
    assert_eq!(record[0x30], 0x12);
    assert_eq!(record[0x35], 0x08);
    assert_eq!(result.aux_files.len(), 1);
    assert_eq!(
        fs::read(&result.aux_files[0]).unwrap(),
        fs::read(&iso).unwrap()
    );
    assert!(iso.exists(), "source ISO must be preserved");
}

#[test]
fn xbox360_raw_split_roundtrip_is_sha256_identical() {
    let workspace = TempWorkspace::new("split_roundtrip").unwrap();
    let source = workspace.root.join("game.iso");
    let mut bytes = (0..200_000u32)
        .flat_map(u32::to_le_bytes)
        .collect::<Vec<_>>();
    bytes[0x10000..0x10014].copy_from_slice(b"MICROSOFT*XBOX*MEDIA");
    fs::write(&source, &bytes).unwrap();
    let input = detection(
        Platform::Xbox360,
        SourceFormat::Xbox360Iso,
        vec![source.clone()],
    );
    let split = get_backend_for_id("xbox360_split")
        .unwrap()
        .convert(
            &input,
            TargetFormat::Xbox360SplitIso,
            &workspace.root.join("split"),
            Arc::new(|_: ProgressUpdate| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let mut parts = vec![split.output_path];
    parts.extend(split.aux_files);
    let merged = get_backend_for_id("xbox360_split")
        .unwrap()
        .convert(
            &detection(Platform::Xbox360, SourceFormat::Xbox360SplitIso, parts),
            TargetFormat::Xbox360Iso,
            &workspace.root.join("merged"),
            Arc::new(|_: ProgressUpdate| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert_eq!(
        Sha256::digest(fs::read(source).unwrap()),
        Sha256::digest(fs::read(merged.output_path).unwrap())
    );
}

#[test]
fn catalog_contains_exactly_one_profile_for_every_platform() {
    assert!(std::ptr::eq(ForgeUi::platform_catalog(), PLATFORM_CATALOG));
    assert_eq!(PLATFORM_CATALOG.len(), Platform::ALL.len());
    for platform in Platform::ALL {
        let profiles: Vec<_> = PLATFORM_CATALOG
            .iter()
            .filter(|profile| profile.platform == platform)
            .collect();
        assert_eq!(
            profiles.len(),
            1,
            "missing or duplicate profile for {platform:?}"
        );
        assert!(!profiles[0].display_name.is_empty());
        assert!(!profiles[0].source_formats.is_empty());
        assert!(ALL_RULES.iter().any(|rule| rule.platform == platform));
    }
}

#[test]
fn manual_import_search_exposes_the_entire_platform_catalog_and_short_names() {
    assert_eq!(
        ForgeUi::manual_import_platform_count(),
        PLATFORM_CATALOG.len()
    );

    let mut menu = ManualImportMenuState::new();
    for profile in PLATFORM_CATALOG {
        menu.set_query(profile.display_name);
        assert!(
            menu.matching_indices()
                .iter()
                .any(|index| PLATFORM_CATALOG[*index].platform == profile.platform),
            "{} was not searchable by display name",
            profile.display_name
        );
        menu.set_query(profile.platform.short_code());
        assert!(
            menu.matching_indices()
                .iter()
                .any(|index| PLATFORM_CATALOG[*index].platform == profile.platform),
            "{} was not searchable by short code {}",
            profile.display_name,
            profile.platform.short_code()
        );
    }

    for (query, platform) in [
        ("PS1", Platform::PlayStation1),
        ("PS2", Platform::PlayStation2),
        ("PS3", Platform::PlayStation3),
        ("PS4", Platform::PlayStation4),
        ("PS5", Platform::PlayStation5),
        ("PSP", Platform::Psp),
        ("PS Vita", Platform::PlayStationVita),
    ] {
        menu.set_query(query);
        assert_eq!(menu.matching_indices().len(), 1, "{query}");
        assert_eq!(
            menu.selected_profile().unwrap().platform,
            platform,
            "{query}"
        );
    }
}

#[test]
fn gamecube_and_wii_routes_use_only_the_discio_tool_adapter() {
    for (platform, source, target) in [
        (
            Platform::GameCube,
            SourceFormat::GameCubeIso,
            TargetFormat::NintendoRvz,
        ),
        (
            Platform::GameCube,
            SourceFormat::NintendoRvz,
            TargetFormat::NintendoDiscIso,
        ),
        (
            Platform::Wii,
            SourceFormat::WiiIso,
            TargetFormat::NintendoRvz,
        ),
        (
            Platform::Wii,
            SourceFormat::WiiWbfs,
            TargetFormat::NintendoRvz,
        ),
    ] {
        let rule = FormatMatrix::find_rule(platform, source, target).unwrap();
        assert_eq!(rule.backend_id, "nintendo_gc_wii");
        assert!(rule.operation.starts_with("DolphinTool convert"));
    }
}

#[test]
fn wii_and_gamecube_detection_uses_disc_headers() {
    let workspace = TempWorkspace::new("nintendo_disc_headers").unwrap();
    let gc = workspace.root.join("game.iso");
    let mut gc_header = vec![0u8; 0x20];
    gc_header[0x1c..0x20].copy_from_slice(&[0xc2, 0x33, 0x9f, 0x3d]);
    fs::write(&gc, gc_header).unwrap();
    assert_eq!(
        FormatDetector::detect(&gc, None).unwrap().platform,
        Platform::GameCube
    );

    let wii = workspace.root.join("game.iso");
    let mut wii_header = vec![0u8; 0x20];
    wii_header[0x18..0x1c].copy_from_slice(&[0x5d, 0x1c, 0x9e, 0xa3]);
    fs::write(&wii, wii_header).unwrap();
    assert_eq!(
        FormatDetector::detect(&wii, None).unwrap().source_format,
        SourceFormat::WiiIso
    );

    let wbfs = workspace.root.join("game.wbfs");
    fs::write(&wbfs, b"WBFS fixture").unwrap();
    assert_eq!(
        FormatDetector::detect(&wbfs, None).unwrap().source_format,
        SourceFormat::WiiWbfs
    );
}

#[test]
fn n64_magic_and_both_normalizers_produce_identical_z64() {
    let workspace = TempWorkspace::new("n64_normalization").unwrap();
    let canonical = vec![0x80, 0x37, 0x12, 0x40, 0x11, 0x22, 0x33, 0x44];
    let z64 = workspace.root.join("game.z64");
    let v64 = workspace.root.join("game.v64");
    let n64 = workspace.root.join("game.n64");
    fs::write(&z64, &canonical).unwrap();
    fs::write(&v64, [0x37, 0x80, 0x40, 0x12, 0x22, 0x11, 0x44, 0x33]).unwrap();
    fs::write(&n64, [0x40, 0x12, 0x37, 0x80, 0x44, 0x33, 0x22, 0x11]).unwrap();
    assert_eq!(
        FormatDetector::detect(&z64, None).unwrap().source_format,
        SourceFormat::N64Z64
    );
    assert_eq!(
        FormatDetector::detect(&v64, None).unwrap().source_format,
        SourceFormat::N64V64
    );
    assert_eq!(
        FormatDetector::detect(&n64, None).unwrap().source_format,
        SourceFormat::N64N64
    );

    for (source, format) in [(&v64, SourceFormat::N64V64), (&n64, SourceFormat::N64N64)] {
        let before = fs::read(source).unwrap();
        let result = get_backend_for_id("n64_normalize")
            .unwrap()
            .convert(
                &detection(Platform::Nintendo64, format, vec![source.to_path_buf()]),
                TargetFormat::N64Z64,
                &workspace.root.join(format!("out-{format:?}")),
                Arc::new(|_: ProgressUpdate| {}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(
            Sha256::digest(fs::read(result.output_path).unwrap()),
            Sha256::digest(&canonical)
        );
        assert_eq!(fs::read(source).unwrap(), before, "source ROM was modified");
    }
}

#[test]
fn nes_gb_gbc_gba_and_nds_headers_are_discriminated() {
    let workspace = TempWorkspace::new("native_headers").unwrap();
    let nes = workspace.root.join("game.nes");
    fs::write(&nes, b"NES\x1a\x00\x00\x00\x00").unwrap();
    assert_eq!(
        FormatDetector::detect(&nes, None).unwrap().source_format,
        SourceFormat::NesRom
    );

    let logo: [u8; 48] = [
        0xce, 0xed, 0x66, 0x66, 0xcc, 0x0d, 0x00, 0x0b, 0x03, 0x73, 0x00, 0x83, 0x00, 0x0c, 0x00,
        0x0d, 0x00, 0x08, 0x11, 0x1f, 0x88, 0x89, 0x00, 0x0e, 0xdc, 0xcc, 0x6e, 0xe6, 0xdd, 0xdd,
        0xd9, 0x99, 0xbb, 0xbb, 0x67, 0x63, 0x6e, 0x0e, 0xec, 0xcc, 0xdd, 0xdc, 0x99, 0x9f, 0xbb,
        0xb9, 0x33, 0x3e,
    ];
    for (name, flag, expected) in [
        ("game.gb", 0, SourceFormat::GameBoyRom),
        ("game.gbc", 0x80, SourceFormat::GameBoyColorRom),
    ] {
        let path = workspace.root.join(name);
        let mut rom = vec![0u8; 0x150];
        rom[0x104..0x134].copy_from_slice(&logo);
        rom[0x143] = flag;
        rom[0x14d] = rom[0x134..=0x14c]
            .iter()
            .fold(0u8, |sum, byte| sum.wrapping_sub(*byte).wrapping_sub(1));
        fs::write(&path, rom).unwrap();
        assert_eq!(
            FormatDetector::detect(&path, None).unwrap().source_format,
            expected
        );
    }

    let gba = workspace.root.join("game.gba");
    let mut gba_rom = vec![0u8; 0xc0];
    gba_rom[4..8].copy_from_slice(&[0x24, 0xff, 0xae, 0x51]);
    gba_rom[0xbd] = gba_rom[0xa0..0xbd]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_sub(*byte))
        .wrapping_sub(0x19);
    fs::write(&gba, gba_rom).unwrap();
    assert_eq!(
        FormatDetector::detect(&gba, None).unwrap().source_format,
        SourceFormat::GameBoyAdvanceRom
    );

    let nds = workspace.root.join("game.nds");
    let mut nds_rom = vec![0u8; 0x200];
    nds_rom[0x0c..0x10].copy_from_slice(b"ABCD");
    nds_rom[0xc0..0xc4].copy_from_slice(&[0x24, 0xff, 0xae, 0x51]);
    fs::write(&nds, nds_rom).unwrap();
    assert_eq!(
        FormatDetector::detect(&nds, None).unwrap().source_format,
        SourceFormat::NintendoDsRom
    );
}

#[test]
fn dreamcast_gdi_parser_preserves_track_order_and_missing_state() {
    let workspace = TempWorkspace::new("gdi_fixture").unwrap();
    let gdi = workspace.root.join("disc.gdi");
    fs::write(workspace.root.join("track01.bin"), [1u8; 16]).unwrap();
    fs::write(workspace.root.join("track02.raw"), [2u8; 16]).unwrap();
    fs::write(
        &gdi,
        "2\n1 0 4 2352 track01.bin 0\n2 45000 0 2352 track02.raw 0\n",
    )
    .unwrap();
    let result = FormatDetector::detect(&gdi, None).unwrap();
    assert_eq!(result.platform, Platform::SegaDreamcast);
    assert_eq!(result.source_format, SourceFormat::Gdi);
    assert_eq!(result.related_files.len(), 3);
    assert!(result.missing_parts.is_empty());
}

#[test]
fn optical_routes_are_limited_to_the_audited_disc_platforms() {
    for platform in [
        Platform::SegaCD,
        Platform::SegaSaturn,
        Platform::PcEngineCD,
        Platform::PcFx,
        Platform::ThreeDO,
        Platform::NeoGeoCD,
    ] {
        assert_eq!(
            FormatMatrix::find_rule(platform, SourceFormat::BinCue, TargetFormat::Chd)
                .unwrap()
                .backend_id,
            "optical_chdman"
        );
    }
    assert!(
        FormatMatrix::find_rule(
            Platform::SegaDreamcast,
            SourceFormat::Gdi,
            TargetFormat::Chd
        )
        .is_some()
    );
    assert!(
        FormatMatrix::find_rule(
            Platform::AtariJaguarCD,
            SourceFormat::BinCue,
            TargetFormat::Chd
        )
        .is_none()
    );
    assert!(
        FormatMatrix::find_rule(
            Platform::SegaDreamcast,
            SourceFormat::Cdi,
            TargetFormat::Chd
        )
        .is_none()
    );
}

#[test]
fn arcade_rom_set_archives_are_detected_as_game_containers() {
    let workspace = TempWorkspace::new("rom_set_archive").unwrap();
    let zip = workspace.root.join("naomi.zip");
    fs::write(&zip, b"PK\x03\x04structured-rom-set").unwrap();
    let detected = FormatDetector::detect(&zip, Some(Platform::SegaNaomi)).unwrap();
    assert_eq!(detected.source_format, SourceFormat::NaomiRomSet);
    assert!(detected.is_archive);
    assert!(
        FormatMatrix::find_rule(
            Platform::SegaNaomi,
            SourceFormat::NaomiRomSet,
            TargetFormat::Passthrough
        )
        .is_some()
    );
}

#[test]
fn final_core_info_audit_adds_every_dedicated_platform_profile() {
    assert_eq!(PLATFORM_CATALOG.len(), 91);
    let additions = [
        Platform::PhilipsCdI,
        Platform::AmstradGx4000,
        Platform::EpochCassetteVision,
        Platform::EpochSuperCassetteVision,
        Platform::VTechCreatiVision,
        Platform::NichibutsuMyVision,
        Platform::AtariXegs,
        Platform::EmersonArcadia2001,
        Platform::IntertonVc4000,
        Platform::ElektorTvGamesComputer,
        Platform::Arduboy,
        Platform::AmigaCd32,
        Platform::DingooA320,
        Platform::SegaModel3,
        Platform::Nuon,
        Platform::AquaplusPEce,
        Platform::PocketStation,
        Platform::SegaVmu,
        Platform::NokiaNGage,
        Platform::TamagotchiP1,
        Platform::Uzebox,
        Platform::WiiU,
        Platform::SunplusSpmp8000,
        Platform::SunplusNative32,
        Platform::CapcomCps1,
        Platform::CapcomCps2,
        Platform::CapcomCps3,
        Platform::HandheldElectronicGames,
    ];
    assert_eq!(additions.len(), 28);
    for platform in additions {
        assert!(
            PLATFORM_CATALOG
                .iter()
                .any(|profile| profile.platform == platform)
        );
        assert!(ALL_RULES.iter().any(|rule| rule.platform == platform));
    }
    assert!(COMPUTER_ONLY_EXCLUSIONS.len() >= 15);
}

#[test]
fn philips_cdi_reuses_only_the_safe_cd_chd_directions() {
    let to_chd = FormatMatrix::find_rule(
        Platform::PhilipsCdI,
        SourceFormat::BinCue,
        TargetFormat::Chd,
    )
    .unwrap();
    let from_chd = FormatMatrix::find_rule(
        Platform::PhilipsCdI,
        SourceFormat::Chd,
        TargetFormat::BinCue,
    )
    .unwrap();
    assert_eq!(to_chd.backend_id, "optical_chdman");
    assert_eq!(from_chd.backend_id, "optical_chdman");
    assert!(to_chd.recommended);
    assert!(
        FormatMatrix::find_rule(
            Platform::PhilipsCdI,
            SourceFormat::DiscIso,
            TargetFormat::Passthrough
        )
        .is_some()
    );
    assert!(
        FormatMatrix::find_rule(
            Platform::PhilipsCdI,
            SourceFormat::DiscIso,
            TargetFormat::Chd
        )
        .is_none()
    );

    let workspace = TempWorkspace::new("cdi_iso").unwrap();
    let iso = workspace.root.join("game.iso");
    let mut bytes = vec![0u8; 0x8800];
    bytes[0x8001..0x8006].copy_from_slice(b"CD001");
    fs::write(&iso, bytes).unwrap();
    let detected = FormatDetector::detect(&iso, Some(Platform::PhilipsCdI)).unwrap();
    assert_eq!(detected.platform, Platform::PhilipsCdI);
    assert_eq!(detected.source_format, SourceFormat::DiscIso);
}

#[test]
fn gx4000_cpr_requires_the_real_riff_ams_container_header() {
    let workspace = TempWorkspace::new("gx4000_cpr").unwrap();
    let valid = workspace.root.join("game.cpr");
    fs::write(&valid, b"RIFF\x04\x00\x00\x00AMS!").unwrap();
    assert_eq!(
        FormatDetector::detect(&valid, None).unwrap().source_format,
        SourceFormat::AmstradGx4000Cpr
    );
    let invalid = workspace.root.join("fake.cpr");
    fs::write(&invalid, b"not a cartridge").unwrap();
    assert_ne!(
        FormatDetector::detect(&invalid, None)
            .unwrap()
            .source_format,
        SourceFormat::AmstradGx4000Cpr
    );
}

#[test]
fn ambiguous_jollycv_and_xegs_formats_require_platform_context() {
    let workspace = TempWorkspace::new("ambiguous_console_roms").unwrap();
    let col = workspace.root.join("game.col");
    fs::write(&col, [0xaa; 32]).unwrap();
    assert_eq!(
        FormatDetector::detect(&col, Some(Platform::VTechCreatiVision))
            .unwrap()
            .source_format,
        SourceFormat::VTechCreatiVisionRom
    );

    let myv = workspace.root.join("game.myv");
    fs::write(&myv, [0x55; 32]).unwrap();
    assert_eq!(
        FormatDetector::detect(&myv, None).unwrap().platform,
        Platform::NichibutsuMyVision
    );

    let xegs = workspace.root.join("game.car");
    fs::write(&xegs, [0x42; 32]).unwrap();
    assert_eq!(
        FormatDetector::detect(&xegs, Some(Platform::AtariXegs))
            .unwrap()
            .source_format,
        SourceFormat::AtariXegsImage
    );
}

#[test]
fn accepted_zip_rom_sets_remain_game_containers() {
    let workspace = TempWorkspace::new("accepted_rom_sets").unwrap();
    for (platform, expected) in [
        (
            Platform::EpochSuperCassetteVision,
            SourceFormat::EpochSuperCassetteVisionRom,
        ),
        (Platform::SegaModel3, SourceFormat::SegaModel3RomSet),
        (Platform::CapcomCps2, SourceFormat::CapcomCps2RomSet),
        (Platform::SunplusNative32, SourceFormat::Native32Package),
    ] {
        let path = workspace.root.join(format!("{platform:?}.zip"));
        fs::write(&path, b"PK\x03\x04structured-game-container").unwrap();
        let detected = FormatDetector::detect(&path, Some(platform)).unwrap();
        assert_eq!(detected.source_format, expected);
        assert!(detected.is_archive);
        assert!(FormatMatrix::find_rule(platform, expected, TargetFormat::Passthrough).is_some());
    }
}

#[test]
fn unique_new_handheld_extensions_route_without_emulator_metadata() {
    let workspace = TempWorkspace::new("new_handheld_extensions").unwrap();
    for (name, platform, format) in [
        (
            "game.pfi",
            Platform::AquaplusPEce,
            SourceFormat::AquaplusPeceImage,
        ),
        ("game.vms", Platform::SegaVmu, SourceFormat::SegaVmuImage),
        (
            "game.n-gage",
            Platform::NokiaNGage,
            SourceFormat::NokiaNGagePackage,
        ),
        ("game.uze", Platform::Uzebox, SourceFormat::UzeboxImage),
        (
            "game.mgw",
            Platform::HandheldElectronicGames,
            SourceFormat::HandheldElectronicImage,
        ),
        ("game.wua", Platform::WiiU, SourceFormat::WiiUArchive),
    ] {
        let path = workspace.root.join(name);
        fs::write(&path, [0x7e; 32]).unwrap();
        let detected = FormatDetector::detect(&path, None).unwrap();
        assert_eq!(detected.platform, platform);
        assert_eq!(detected.source_format, format);
    }
}

fn create_test_sfo(entries: &[(&str, u16, &[u8])]) -> Vec<u8> {
    let entry_count = entries.len() as u32;
    let mut key_table = Vec::new();
    let mut val_table = Vec::new();
    let mut entry_table = Vec::new();

    for (key, fmt, val) in entries {
        let key_offset = key_table.len() as u16;
        key_table.extend_from_slice(key.as_bytes());
        key_table.push(0);

        let val_offset = val_table.len() as u32;
        let val_len = val.len() as u32;
        val_table.extend_from_slice(val);
        while val_table.len() % 4 != 0 {
            val_table.push(0);
        }
        let val_max_len = val_table.len() as u32 - val_offset;

        entry_table.extend_from_slice(&key_offset.to_le_bytes());
        entry_table.extend_from_slice(&fmt.to_le_bytes());
        entry_table.extend_from_slice(&val_len.to_le_bytes());
        entry_table.extend_from_slice(&val_max_len.to_le_bytes());
        entry_table.extend_from_slice(&val_offset.to_le_bytes());
    }

    let header_size = 20u32;
    let entry_table_size = entry_table.len() as u32;
    let key_table_offset = header_size + entry_table_size;
    while (key_table_offset as usize + key_table.len()) % 4 != 0 {
        key_table.push(0);
    }
    let val_table_offset = key_table_offset + key_table.len() as u32;

    let mut sfo = Vec::new();
    sfo.extend_from_slice(b"\x00PSF");
    sfo.extend_from_slice(&0x00000101u32.to_le_bytes());
    sfo.extend_from_slice(&key_table_offset.to_le_bytes());
    sfo.extend_from_slice(&val_table_offset.to_le_bytes());
    sfo.extend_from_slice(&entry_count.to_le_bytes());
    sfo.extend_from_slice(&entry_table);
    sfo.extend_from_slice(&key_table);
    sfo.extend_from_slice(&val_table);
    sfo
}

#[test]
fn ps4_sfo_parsing_accurately_reads_title_id_and_never_confuses_keys() {
    let workspace = TempWorkspace::new("sfo_parser_test").unwrap();
    let sfo_path = workspace.root.join("param.sfo");

    let sfo_bytes = create_test_sfo(&[
        ("APP_VER", 0x0204, b"01.00\0"),
        ("CATEGORY", 0x0204, b"gd\0"),
        (
            "CONTENT_ID",
            0x0204,
            b"EP4396-CUSA09172_00-LANOIRE000000000\0",
        ),
        ("TITLE", 0x0204, b"L.A. Noire\0"),
        ("TITLE_ID", 0x0204, b"CUSA09172\0"),
        ("VERSION", 0x0204, b"01.00\0"),
    ]);
    fs::write(&sfo_path, sfo_bytes).unwrap();

    let (title_id, title) = FormatDetector::read_sfo(&sfo_path);
    assert_eq!(title_id.as_deref(), Some("CUSA09172"));
    assert_eq!(title.as_deref(), Some("L.A. Noire"));

    let category = FormatDetector::read_sfo_category(&sfo_path);
    assert_eq!(category.as_deref(), Some("gd"));

    let app_ver = FormatDetector::read_sfo_key(&sfo_path, "APP_VER");
    assert_eq!(app_ver.as_deref(), Some("01.00"));

    let version = FormatDetector::read_sfo_key(&sfo_path, "VERSION");
    assert_eq!(version.as_deref(), Some("01.00"));

    let content_id = FormatDetector::read_sfo_content_id(&sfo_path);
    assert_eq!(
        content_id.as_deref(),
        Some("EP4396-CUSA09172_00-LANOIRE000000000")
    );
}

#[test]
fn itemzflow_update_normalization_keeps_base_identity_and_uses_patch_version() {
    let workspace = TempWorkspace::new("itemzflow_sfo_merge").unwrap();
    let base = workspace.root.join("base.sfo");
    let update = workspace.root.join("update.sfo");
    let merged = workspace.root.join("merged.sfo");
    let base_system = 0x0505_0000u32.to_le_bytes();
    let update_system = 0x0900_0000u32.to_le_bytes();
    fs::write(
        &base,
        create_test_sfo(&[
            ("APP_VER", 0x0204, b"01.00\0"),
            ("CATEGORY", 0x0204, b"gd\0"),
            (
                "CONTENT_ID",
                0x0204,
                b"EP0001-CUSA09897_00-BASEGAME00000000\0",
            ),
            ("DOWNLOAD_DATA_SIZE", 0x0404, &1u32.to_le_bytes()),
            ("SYSTEM_VER", 0x0404, &base_system),
            ("TITLE", 0x0204, b"LEGO The Incredibles\0"),
            ("TITLE_ID", 0x0204, b"CUSA09897\0"),
            ("VERSION", 0x0204, b"01.00\0"),
        ]),
    )
    .unwrap();
    fs::write(
        &update,
        create_test_sfo(&[
            ("APP_VER", 0x0204, b"01.04\0"),
            ("CATEGORY", 0x0204, b"gp\0"),
            (
                "CONTENT_ID",
                0x0204,
                b"EP0001-CUSA09897_00-PATCH00000000000\0",
            ),
            ("SYSTEM_VER", 0x0404, &update_system),
            ("TITLE", 0x0204, b"Patch metadata\0"),
            ("TITLE_ID", 0x0204, b"CUSA09897\0"),
            ("VERSION", 0x0204, b"01.04\0"),
        ]),
    )
    .unwrap();

    iso2god::forge::ps4::normalize_itemzflow_update_sfo(&base, &update, &merged).unwrap();
    assert_eq!(
        FormatDetector::read_sfo_category(&merged).as_deref(),
        Some("gd")
    );
    assert_eq!(
        FormatDetector::read_sfo_key(&merged, "APP_VER").as_deref(),
        Some("01.04")
    );
    assert_eq!(
        FormatDetector::read_sfo_key(&merged, "VERSION").as_deref(),
        Some("01.04")
    );
    assert_eq!(
        FormatDetector::read_sfo_key(&merged, "TITLE").as_deref(),
        Some("LEGO The Incredibles")
    );
    assert_eq!(
        FormatDetector::read_sfo_content_id(&merged).as_deref(),
        Some("EP0001-CUSA09897_00-BASEGAME00000000")
    );
    assert_eq!(
        FormatDetector::read_sfo_key(&merged, "DOWNLOAD_DATA_SIZE").as_deref(),
        Some("0")
    );
    let expected_system = u32::from_le_bytes(update_system).to_string();
    assert_eq!(
        FormatDetector::read_sfo_key(&merged, "SYSTEM_VER").as_deref(),
        Some(expected_system.as_str())
    );
}

#[test]
fn itemzflow_validator_requires_a_fake_signed_eboot() {
    let workspace = TempWorkspace::new("itemzflow_fself_validation").unwrap();
    let root = workspace.root.join("CUSA09897");
    fs::create_dir_all(root.join("sce_sys")).unwrap();
    fs::write(
        root.join("sce_sys").join("param.sfo"),
        create_test_sfo(&[
            ("CATEGORY", 0x0204, b"gd\0"),
            ("TITLE", 0x0204, b"LEGO The Incredibles\0"),
            ("TITLE_ID", 0x0204, b"CUSA09897\0"),
        ]),
    )
    .unwrap();
    let mut fself = create_test_fself(0xfe10, 1, false);
    fs::write(root.join("eboot.bin"), &fself).unwrap();
    iso2god::forge::ps4::validate_target_folder(&root, TargetWorkflow::Ps4ItemzFlow).unwrap();

    // The old validator read byte 9 (part of KeyType) and accepted this. The
    // real program type is in extended info at 0xe8 for this fixture.
    fself[0xe8..0xf0].copy_from_slice(&4u64.to_le_bytes());
    fs::write(root.join("eboot.bin"), &fself).unwrap();
    assert!(
        iso2god::forge::ps4::validate_target_folder(&root, TargetWorkflow::Ps4ItemzFlow).is_err()
    );
}

#[test]
fn itemzflow_validator_rejects_fake_self_with_encrypted_segments() {
    let workspace = TempWorkspace::new("itemzflow_encrypted_fself").unwrap();
    let root = workspace.root.join("CUSA09897");
    fs::create_dir_all(root.join("sce_sys")).unwrap();
    fs::write(
        root.join("sce_sys").join("param.sfo"),
        create_test_sfo(&[
            ("CATEGORY", 0x0204, b"gd\0"),
            ("TITLE", 0x0204, b"LEGO The Incredibles\0"),
            ("TITLE_ID", 0x0204, b"CUSA09897\0"),
        ]),
    )
    .unwrap();
    fs::write(root.join("eboot.bin"), create_test_fself(0xfe10, 1, true)).unwrap();
    let error = iso2god::forge::ps4::validate_target_folder(&root, TargetWorkflow::Ps4ItemzFlow)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("still has encrypted SELF entries")
    );
}

fn create_test_fself(elf_type: u16, program_type: u64, encrypted: bool) -> Vec<u8> {
    let entry_count = 2u16;
    let elf_offset = 0x20 + entry_count as usize * 0x20;
    let extended_offset = 0xe0usize;
    let header_size = 0x150u16;
    let meta_size = 0x1f0u16;
    let meta_offset = header_size as usize + meta_size as usize;
    let data_offset = meta_offset + 0x20;
    let mut output = vec![0u8; data_offset + 4];
    let output_len = output.len() as u64;

    output[..4].copy_from_slice(b"\x4f\x15\x3d\x1d");
    output[5] = 1;
    output[6] = 1;
    output[7] = 0x12;
    output[8..12].copy_from_slice(&0x101u32.to_le_bytes());
    output[12..14].copy_from_slice(&header_size.to_le_bytes());
    output[14..16].copy_from_slice(&meta_size.to_le_bytes());
    output[16..24].copy_from_slice(&output_len.to_le_bytes());
    output[24..26].copy_from_slice(&entry_count.to_le_bytes());
    output[26..28].copy_from_slice(&0x22u16.to_le_bytes());

    let encrypted_bit = if encrypted { 2u64 } else { 0 };
    let meta_properties = 0x0011_0004u64 | encrypted_bit;
    output[0x20..0x28].copy_from_slice(&meta_properties.to_le_bytes());
    output[0x28..0x30].copy_from_slice(&(meta_offset as u64).to_le_bytes());
    output[0x30..0x38].copy_from_slice(&0x20u64.to_le_bytes());
    output[0x38..0x40].copy_from_slice(&0x20u64.to_le_bytes());
    let data_properties = 0x0000_2804u64 | encrypted_bit;
    output[0x40..0x48].copy_from_slice(&data_properties.to_le_bytes());
    output[0x48..0x50].copy_from_slice(&(data_offset as u64).to_le_bytes());
    output[0x50..0x58].copy_from_slice(&4u64.to_le_bytes());
    output[0x58..0x60].copy_from_slice(&4u64.to_le_bytes());

    output[elf_offset..elf_offset + 4].copy_from_slice(b"\x7fELF");
    output[elf_offset + 4] = 2;
    output[elf_offset + 5] = 1;
    output[elf_offset + 6] = 1;
    output[elf_offset + 16..elf_offset + 18].copy_from_slice(&elf_type.to_le_bytes());
    output[elf_offset + 18..elf_offset + 20].copy_from_slice(&62u16.to_le_bytes());
    output[elf_offset + 20..elf_offset + 24].copy_from_slice(&1u32.to_le_bytes());
    output[elf_offset + 32..elf_offset + 40].copy_from_slice(&64u64.to_le_bytes());
    output[elf_offset + 52..elf_offset + 54].copy_from_slice(&64u16.to_le_bytes());
    output[elf_offset + 54..elf_offset + 56].copy_from_slice(&56u16.to_le_bytes());
    output[elf_offset + 56..elf_offset + 58].copy_from_slice(&1u16.to_le_bytes());

    let program = elf_offset + 64;
    output[program..program + 4].copy_from_slice(&1u32.to_le_bytes());
    output[program + 4..program + 8].copy_from_slice(&5u32.to_le_bytes());
    output[program + 8..program + 16].copy_from_slice(&0x4000u64.to_le_bytes());
    output[program + 32..program + 40].copy_from_slice(&4u64.to_le_bytes());
    output[program + 40..program + 48].copy_from_slice(&4u64.to_le_bytes());
    output[program + 48..program + 56].copy_from_slice(&0x4000u64.to_le_bytes());

    output[extended_offset..extended_offset + 8]
        .copy_from_slice(&0x3100_0000_0000_0001u64.to_le_bytes());
    output[extended_offset + 8..extended_offset + 16].copy_from_slice(&program_type.to_le_bytes());
    output[extended_offset + 32] = 1;
    output[data_offset..data_offset + 4].copy_from_slice(b"TEST");
    output
}

#[test]
fn ps4_transfer_local_rejects_unsupported_nested_dlc_layout() {
    use iso2god::forge::backend::ConversionResult;
    use iso2god::forge::transfer::TransferManager;

    let workspace = TempWorkspace::new("ps4_transfer_test").unwrap();
    let target_dir = workspace.root.join("ps4_library");
    let staging_base = workspace.root.join("staging_base");
    let sce_sys_dir = staging_base.join("sce_sys");
    fs::create_dir_all(&sce_sys_dir).unwrap();

    let base_sfo = create_test_sfo(&[
        ("APP_VER", 0x0204, b"01.00\0"),
        ("CATEGORY", 0x0204, b"gd\0"),
        (
            "CONTENT_ID",
            0x0204,
            b"EP4396-CUSA09172_00-LANOIRE000000000\0",
        ),
        ("TITLE", 0x0204, b"L.A. Noire\0"),
        ("TITLE_ID", 0x0204, b"CUSA09172\0"),
    ]);
    fs::write(sce_sys_dir.join("param.sfo"), base_sfo).unwrap();
    fs::write(sce_sys_dir.join("icon0.png"), b"base_icon_png_data").unwrap();
    fs::write(staging_base.join("eboot.bin"), b"ps4_eboot_bin_payload").unwrap();
    fs::write(staging_base.join("game_data.dat"), b"game_data").unwrap();

    let base_result = ConversionResult {
        platform: Platform::PlayStation4,
        target_format: TargetFormat::Ps4Folder,
        output_path: staging_base.clone(),
        total_size: 1024,
        was_passthrough: false,
        title_id: Some("CUSA09172".to_string()),
        title_name: Some("L.A. Noire".to_string()),
        aux_files: Vec::new(),
        tool_report: None,
    };

    let cancelled = Arc::new(AtomicBool::new(false));
    let final_dest = TransferManager::transfer_local(
        &base_result,
        &target_dir,
        Arc::new(|_| {}),
        cancelled.clone(),
    )
    .unwrap();

    // Verify Base Game is delivered into <target_dir>/CUSA09172
    assert_eq!(final_dest, target_dir.join("CUSA09172"));
    assert!(final_dest.join("eboot.bin").is_file());
    assert!(final_dest.join("sce_sys").join("param.sfo").is_file());
    assert!(final_dest.join("sce_sys").join("icon0.png").is_file());
    assert!(final_dest.join("game_data.dat").is_file());

    // Now test DLC transfer into the same game library
    let staging_dlc = workspace.root.join("staging_dlc");
    let dlc_sce_sys = staging_dlc.join("sce_sys");
    fs::create_dir_all(&dlc_sce_sys).unwrap();

    let dlc_sfo = create_test_sfo(&[
        ("APP_VER", 0x0204, b"01.00\0"),
        ("CATEGORY", 0x0204, b"ac\0"),
        (
            "CONTENT_ID",
            0x0204,
            b"EP4396-CUSA09172_00-DLCEXPANSION0001\0",
        ),
        ("TITLE", 0x0204, b"The Naked City DLC\0"),
        ("TITLE_ID", 0x0204, b"CUSA09172\0"),
    ]);
    fs::write(dlc_sce_sys.join("param.sfo"), dlc_sfo).unwrap();
    fs::write(staging_dlc.join("dlc_content.dat"), b"dlc_extra_case_data").unwrap();

    let dlc_result = ConversionResult {
        platform: Platform::PlayStation4,
        target_format: TargetFormat::Ps4Folder,
        output_path: staging_dlc.clone(),
        total_size: 512,
        was_passthrough: false,
        title_id: Some("CUSA09172".to_string()),
        title_name: Some("The Naked City DLC".to_string()),
        aux_files: Vec::new(),
        tool_report: None,
    };

    let dlc_result =
        TransferManager::transfer_local(&dlc_result, &target_dir, Arc::new(|_| {}), cancelled);
    assert!(dlc_result.is_err());
    assert!(!final_dest.join("addcont").exists());

    // Verify base game icon and eboot were preserved
    assert_eq!(
        fs::read(final_dest.join("sce_sys").join("icon0.png")).unwrap(),
        b"base_icon_png_data"
    );
    assert_eq!(
        fs::read(final_dest.join("eboot.bin")).unwrap(),
        b"ps4_eboot_bin_payload"
    );
}

#[test]
fn ps4_fpkg_and_retail_pkg_are_distinct_options_and_profiles() {
    use iso2god::forge::profile::TargetProfile;

    let fpkg_profile = TargetProfile::Ps4FakePkg;
    let retail_profile = TargetProfile::Ps4RetailPkg;
    let folder_profile = TargetProfile::Ps4ExtractedFolder;

    assert_ne!(
        fpkg_profile.recommended_target(),
        retail_profile.recommended_target()
    );
    assert_ne!(
        fpkg_profile.recommended_target(),
        folder_profile.recommended_target()
    );
    assert_eq!(fpkg_profile.recommended_target(), TargetFormat::Ps4FakePkg);
    assert_eq!(
        retail_profile.recommended_target(),
        TargetFormat::Ps4RetailPkg
    );
    assert_eq!(folder_profile.recommended_target(), TargetFormat::Ps4Folder);

    assert_eq!(fpkg_profile.display_name(), "PS4: Fake PKG (FPKG / .pkg)");
    assert_eq!(retail_profile.display_name(), "PS4: Retail PKG (.pkg)");
    assert_eq!(
        folder_profile.display_name(),
        "PS4: Extracted / Loose Application Directory"
    );
}

#[test]
fn ps4_user_targets_never_select_pkg_or_fpkg_outputs() {
    let source = detection(Platform::PlayStation4, SourceFormat::Ps4FakePkg, vec![]);
    for workflow in [TargetWorkflow::Ps4ItemzFlow, TargetWorkflow::Ps4ShadPs4] {
        let choices = ForgeUi::build_format_choices(
            &source,
            Platform::PlayStation4,
            workflow.target_profile(Platform::PlayStation4),
        );
        let choice =
            ForgeUi::choice_for_target(&source, Platform::PlayStation4, workflow, &choices)
                .unwrap();
        assert_eq!(choice.target_format, TargetFormat::Ps4Folder);
    }
    assert_eq!(
        target_workflows(Platform::PlayStation4, UsageMode::PhysicalConsole),
        vec![TargetWorkflow::Ps4ItemzFlow]
    );
    assert_eq!(
        target_workflows(Platform::PlayStation4, UsageMode::Emulator),
        vec![TargetWorkflow::Ps4ShadPs4]
    );
}

#[test]
fn internal_console_storage_routes_to_installable_package_workflows() {
    assert_eq!(
        target_workflow_for_console_storage(Platform::PlayStation4, ConsoleStorage::Internal),
        Some(TargetWorkflow::Ps4InternalInstall)
    );
    assert_eq!(
        target_workflow_for_console_storage(Platform::PlayStation4, ConsoleStorage::External),
        Some(TargetWorkflow::Ps4ItemzFlow)
    );
    assert_eq!(
        target_workflow_for_console_storage(Platform::PlayStation5, ConsoleStorage::Internal),
        None
    );
    assert_eq!(
        target_workflow_for_console_storage(Platform::PlayStation5, ConsoleStorage::External),
        None
    );
    assert_eq!(
        TargetWorkflow::Ps4InternalInstall.forced_target(),
        Some(TargetFormat::Ps4FakePkg)
    );
    assert_eq!(
        target_workflows(Platform::PlayStation5, UsageMode::PhysicalConsole),
        vec![TargetWorkflow::Ps5ItemzFlow]
    );
    assert_eq!(
        target_workflows(Platform::PlayStation5, UsageMode::Emulator),
        vec![TargetWorkflow::Ps5AnyPS5]
    );
    assert_eq!(
        target_workflow_for_usage(Platform::PlayStation5, UsageMode::PhysicalConsole),
        Some(TargetWorkflow::Ps5ItemzFlow)
    );
    assert_eq!(
        target_workflow_for_usage(Platform::PlayStation5, UsageMode::Emulator),
        Some(TargetWorkflow::Ps5AnyPS5)
    );
}

#[test]
#[cfg(feature = "deferred-console-integration-tests")]
fn ps5_only_exposes_loose_folder_passthrough_and_anyps5_preparation() {
    let folder_rules =
        FormatMatrix::get_rules_for_source(Platform::PlayStation5, SourceFormat::Ps5LooseFolder);
    assert!(folder_rules.iter().any(|rule| {
        rule.target_format == TargetFormat::Ps5LooseFolder
            && rule.support_status == SupportStatus::PassThrough
    }));
    assert!(folder_rules.iter().any(|rule| {
        rule.target_format == TargetFormat::Ps5AnyPS5Output && rule.backend_id == "ps5_anyps5"
    }));

    let pkg_rules =
        FormatMatrix::get_rules_for_source(Platform::PlayStation5, SourceFormat::Ps5Pkg);
    assert!(pkg_rules.iter().any(|rule| {
        rule.target_format == TargetFormat::Ps5AnyPS5Output && rule.backend_id == "ps5_anyps5"
    }));

    for source_format in [
        SourceFormat::Ps5PkgManagerMultipart,
        SourceFormat::Ps5RawSplitPkg,
        SourceFormat::ShadowMountExfatImage,
        SourceFormat::ShadowMountFfPkg,
        SourceFormat::ShadowMountCompressedPfs,
    ] {
        assert!(
            FormatMatrix::get_rules_for_source(Platform::PlayStation5, source_format).is_empty()
        );
    }
}

#[test]
#[cfg(feature = "deferred-console-integration-tests")]
fn ps5_anyps5_backend_runs_tool_and_validates_real_output_layout() {
    struct TestRoot(std::path::PathBuf);
    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    use iso2god::forge::backend::ProgressUpdate;

    let tool = std::env::var_os("DIGITAL_FORGE_PKG_TO_ANYPS5")
        .map(std::path::PathBuf::from)
        .expect("DIGITAL_FORGE_PKG_TO_ANYPS5 must point to the verified helper");
    assert!(
        tool.is_file(),
        "AnyPS5 helper does not exist: {}",
        tool.display()
    );

    let temp_parent = std::env::var_os("DIGITAL_FORGE_TEST_TEMP_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&temp_parent).unwrap();
    let workspace_root = temp_parent.join(format!(
        "ps5_anyps5_tool_integration_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&workspace_root).unwrap();
    let _workspace = TestRoot(workspace_root.clone());
    let game = workspace_root.join("game");
    let output = workspace_root.join("output");
    fs::create_dir_all(game.join("sce_sys")).unwrap();
    fs::create_dir_all(&output).unwrap();

    let mut elf = [0u8; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16] = 2;
    elf[18] = 0x3e;
    elf[20] = 1;
    elf[52] = 64;
    elf[54] = 56;
    fs::write(game.join("eboot.bin"), elf).unwrap();
    fs::write(
        game.join("sce_sys/param.json"),
        br#"{"titleId":"PPSA00001","category":"gd"}"#,
    )
    .unwrap();
    fs::write(game.join("data.bin"), b"fixture").unwrap();

    let input = detection(
        Platform::PlayStation5,
        SourceFormat::Ps5LooseFolder,
        vec![game],
    );
    let backend = get_backend_for_id("ps5_anyps5").unwrap();
    let result = backend
        .convert(
            &input,
            TargetFormat::Ps5AnyPS5Output,
            &output,
            Arc::new(|_: ProgressUpdate| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    let report = backend.validate_output(&result).unwrap();
    assert!(report.is_valid);
    assert_eq!(report.game_id.as_deref(), Some("PPSA00001"));
    assert!(result.output_path.join("source/eboot.bin").is_file());
    assert!(result.output_path.join("app0/sce_sys/param.json").is_file());
    assert!(result.output_path.join("app0/data.bin").is_file());
}

#[test]
fn xenia_keeps_iso_god_and_extracted_games_in_native_formats() {
    for (source_format, expected) in [
        (SourceFormat::Xbox360Iso, TargetFormat::Xbox360Iso),
        (SourceFormat::Xbox360God, TargetFormat::Xbox360God),
        (SourceFormat::Xbox360Folder, TargetFormat::Xbox360Folder),
    ] {
        let source = detection(Platform::Xbox360, source_format, vec![]);
        let choices = ForgeUi::build_format_choices(
            &source,
            Platform::Xbox360,
            TargetProfile::NativePassthrough,
        );
        let choice = ForgeUi::choice_for_target(
            &source,
            Platform::Xbox360,
            TargetWorkflow::Xbox360Xenia,
            &choices,
        )
        .unwrap();
        assert_eq!(choice.target_format, expected);
        assert!(choice.is_passthrough);
    }
}

#[test]
fn ps4_pkg_transfer_names_output_file_using_title_id() {
    use iso2god::forge::backend::ConversionResult;
    use iso2god::forge::transfer::TransferManager;

    let workspace = TempWorkspace::new("ps4_pkg_name_test").unwrap();
    let target_dir = workspace.root.join("dest");
    let staging_pkg = workspace
        .root
        .join("L.A.Noire_CUSA09172_v1.04_[5.05]_OPOISS0893.pkg");
    fs::write(&staging_pkg, b"ps4_pkg_payload").unwrap();

    let result = ConversionResult {
        platform: Platform::PlayStation4,
        target_format: TargetFormat::Ps4FakePkg,
        output_path: staging_pkg.clone(),
        total_size: 15,
        was_passthrough: false,
        title_id: Some("CUSA09172".to_string()),
        title_name: Some("L.A. Noire".to_string()),
        aux_files: Vec::new(),
        tool_report: None,
    };

    let delivered = TransferManager::transfer_local(
        &result,
        &target_dir,
        Arc::new(|_| {}),
        Arc::new(AtomicBool::new(false)),
    )
    .unwrap();

    assert_eq!(delivered, target_dir.join("CUSA09172.pkg"));
    assert!(delivered.is_file());
    assert_eq!(fs::read(delivered).unwrap(), b"ps4_pkg_payload");
}
