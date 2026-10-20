use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Error, ensure};

use super::detector::FormatDetector;
use super::profile::TargetWorkflow;

pub fn validate_target_folder(path: &Path, workflow: TargetWorkflow) -> Result<(), Error> {
    ensure!(path.is_dir(), "PS5 loose target is not a directory");
    ensure!(
        path.join("eboot.bin").is_file(),
        "PS5 loose target lacks eboot.bin"
    );
    let sce_sys = path.join("sce_sys");
    ensure!(sce_sys.is_dir(), "PS5 loose target lacks sce_sys");

    let json_path = sce_sys.join("param.json");
    ensure!(
        json_path.is_file(),
        "PS5 loose target lacks sce_sys/param.json"
    );
    let json: serde_json::Value = serde_json::from_slice(
        &fs::read(&json_path).with_context(|| format!("could not read {}", json_path.display()))?,
    )
    .context("PS5 param.json is invalid")?;
    let title_id = json
        .get("titleId")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    ensure!(
        is_ps5_title_id(title_id),
        "PS5 param.json has invalid titleId {title_id}"
    );
    let category = json
        .get("category")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    ensure!(
        category != "gp" && category != "ac",
        "PS5 loose game root contains patch/add-on metadata"
    );

    if matches!(workflow, TargetWorkflow::Ps5ItemzFlow) {
        let modules = path.join("sce_module");
        ensure!(
            modules.is_dir(),
            "PS5 ItemzFlow game folder lacks sce_module"
        );
        ensure!(
            directory_contains_file(&modules)?,
            "PS5 ItemzFlow sce_module folder contains no game modules"
        );
        // Current ItemzFlow FG App discovery reads this compatibility SFO when
        // registering a loose application, while PS5 tooling also needs JSON.
        let sfo = sce_sys.join("param.sfo");
        ensure!(
            sfo.is_file(),
            "PS5 ItemzFlow FG App requires sce_sys/param.sfo in addition to PS5 param.json"
        );
        let (sfo_title_id, _) = FormatDetector::read_sfo(&sfo);
        ensure!(
            sfo_title_id.as_deref() == Some(title_id),
            "PS5 param.sfo TITLE_ID does not match param.json titleId"
        );
    }
    Ok(())
}

/// Copies an already prepared native PS5 game folder into the selected
/// operation workspace. This preserves the source and refuses links or
/// special filesystem entries rather than following them outside the dump.
pub fn copy_prepared_itemzflow_folder(
    source: &Path,
    output_dir: &Path,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf, Error> {
    validate_target_folder(source, TargetWorkflow::Ps5ItemzFlow)?;
    let total_bytes = folder_file_bytes(source, cancelled)?;
    let destination = output_dir.join("Itemzflow");
    ensure!(
        !destination.exists(),
        "PS5 Itemzflow output already exists in the operation workspace"
    );
    fs::create_dir_all(&destination)?;
    let result = (|| {
        let mut bytes_copied = 0u64;
        progress(0, total_bytes);
        copy_folder_contents(
            source,
            &destination,
            cancelled,
            &mut bytes_copied,
            total_bytes,
            &mut progress,
        )?;
        ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
        validate_target_folder(&destination, TargetWorkflow::Ps5ItemzFlow)
            .context("copied PS5 Itemzflow folder failed validation")?;
        progress(total_bytes, total_bytes);
        Ok(destination.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&destination);
    }
    result
}

/// Safely extracts a plain TAR dump. It accepts only regular files and
/// directories, validates each header checksum, and rejects links, traversal,
/// duplicate paths, and truncated entries. Returns the unique native game root.
pub fn extract_itemzflow_tar(
    archive: &Path,
    staging_root: &Path,
    cancelled: &AtomicBool,
) -> Result<PathBuf, Error> {
    fs::create_dir_all(staging_root)?;
    let mut input = File::open(archive)?;
    let archive_len = input.metadata()?.len();
    let mut seen = std::collections::HashSet::new();
    let mut header = [0u8; 512];
    let mut entries = 0usize;
    loop {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "TAR extraction cancelled"
        );
        let offset = input.stream_position()?;
        if offset >= archive_len {
            break;
        }
        input
            .read_exact(&mut header)
            .context("truncated TAR header")?;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        validate_tar_header(&header)?;
        let name = tar_header_string(&header[0..100])?;
        let prefix = tar_header_string(&header[345..500])?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let relative = safe_tar_path(&relative)?;
        ensure!(
            seen.insert(relative.clone()),
            "TAR contains a duplicate path: {}",
            relative.display()
        );
        entries = entries.checked_add(1).context("TAR entry count overflow")?;
        ensure!(entries <= 500_000, "TAR contains too many entries");
        let size = parse_tar_octal(&header[124..136])?;
        let kind = header[156];
        let target = staging_root.join(&relative);
        if kind == 0 || kind == b'0' {
            let parent = target.parent().context("invalid TAR member path")?;
            fs::create_dir_all(parent)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .with_context(|| format!("could not create TAR member {}", relative.display()))?;
            let mut limited = (&mut input).take(size);
            let copied = std::io::copy(&mut limited, &mut output)?;
            ensure!(
                copied == size,
                "TAR member was truncated: {}",
                relative.display()
            );
            output.sync_all()?;
            let padded = (size.checked_add(511).context("TAR member size overflow")? / 512)
                .checked_mul(512)
                .context("TAR padded size overflow")?;
            input.seek(SeekFrom::Current((padded - size) as i64))?;
        } else if kind == b'5' {
            ensure!(size == 0, "TAR directory entry has unexpected data");
            fs::create_dir_all(&target)?;
        } else {
            anyhow::bail!(
                "TAR contains unsupported link or special entry type {kind:#x}: {}",
                relative.display()
            );
        }
        ensure!(
            input.stream_position()? <= archive_len,
            "TAR entry exceeds archive length"
        );
    }
    ensure!(entries > 0, "TAR archive is empty");
    let mut candidates = Vec::new();
    find_native_game_roots(staging_root, &mut candidates, 0)?;
    ensure!(
        candidates.len() == 1,
        "expected one PS5 game root in TAR; found {}",
        candidates.len()
    );
    Ok(candidates.remove(0))
}

fn validate_tar_header(header: &[u8; 512]) -> Result<(), Error> {
    let stored = parse_tar_octal(&header[148..156])?;
    let actual: u64 = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                b' ' as u64
            } else {
                *byte as u64
            }
        })
        .sum();
    ensure!(stored == actual, "TAR header checksum mismatch");
    Ok(())
}

fn tar_header_string(field: &[u8]) -> Result<String, Error> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    Ok(std::str::from_utf8(&field[..end])?.to_owned())
}

fn parse_tar_octal(field: &[u8]) -> Result<u64, Error> {
    let text = field
        .iter()
        .copied()
        .skip_while(|byte| *byte == b' ' || *byte == 0)
        .take_while(|byte| *byte != b' ' && *byte != 0)
        .collect::<Vec<_>>();
    ensure!(!text.is_empty(), "TAR numeric field is empty");
    let text = std::str::from_utf8(&text)?;
    u64::from_str_radix(text, 8).context("invalid TAR octal value")
}

fn safe_tar_path(raw: &str) -> Result<PathBuf, Error> {
    ensure!(
        !raw.is_empty() && !raw.contains('\\') && !raw.starts_with('/'),
        "TAR member path is absolute or empty"
    );
    let path = Path::new(raw);
    for component in path.components() {
        ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "TAR member path contains traversal or a platform prefix: {raw}"
        );
    }
    Ok(path.to_path_buf())
}

fn find_native_game_roots(
    root: &Path,
    found: &mut Vec<PathBuf>,
    depth: usize,
) -> Result<(), Error> {
    ensure!(
        depth <= 20,
        "PS5 TAR directory nesting exceeds the supported limit"
    );
    if root.join("eboot.bin").is_file() && root.join("sce_sys/param.json").is_file() {
        found.push(root.to_path_buf());
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "TAR staging unexpectedly contains a symbolic link"
        );
        if metadata.is_dir() {
            find_native_game_roots(&entry.path(), found, depth + 1)?;
        }
    }
    Ok(())
}

fn folder_file_bytes(path: &Path, cancelled: &AtomicBool) -> Result<u64, Error> {
    ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("could not inspect {}", path.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "PS5 Itemzflow dump cannot contain symbolic links: {}",
        path.display()
    );
    if metadata.is_file() {
        return Ok(metadata.len());
    }
    ensure!(
        metadata.is_dir(),
        "PS5 Itemzflow dump contains an unsupported filesystem entry: {}",
        path.display()
    );
    let mut bytes = 0u64;
    for entry in fs::read_dir(path).with_context(|| format!("could not read {}", path.display()))? {
        bytes = bytes
            .checked_add(folder_file_bytes(&entry?.path(), cancelled)?)
            .context("PS5 Itemzflow folder size overflowed")?;
    }
    Ok(bytes)
}

fn copy_folder_contents(
    source_dir: &Path,
    destination_dir: &Path,
    cancelled: &AtomicBool,
    bytes_copied: &mut u64,
    total_bytes: u64,
    progress: &mut impl FnMut(u64, u64),
) -> Result<(), Error> {
    fs::create_dir_all(destination_dir)?;
    for entry in fs::read_dir(source_dir)
        .with_context(|| format!("could not read {}", source_dir.display()))?
    {
        ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
        let entry = entry?;
        let source = entry.path();
        let metadata = fs::symlink_metadata(&source)?;
        ensure!(
            !metadata.file_type().is_symlink(),
            "PS5 Itemzflow dump cannot contain symbolic links: {}",
            source.display()
        );
        let destination = destination_dir.join(entry.file_name());
        if metadata.is_dir() {
            copy_folder_contents(
                &source,
                &destination,
                cancelled,
                bytes_copied,
                total_bytes,
                progress,
            )?;
        } else {
            ensure!(
                metadata.is_file(),
                "PS5 Itemzflow dump contains an unsupported filesystem entry: {}",
                source.display()
            );
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut input = File::open(&source)?;
            let mut output = File::create(&destination)?;
            let mut buffer = [0u8; 256 * 1024];
            loop {
                ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
                let read = input.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                output.write_all(&buffer[..read])?;
                *bytes_copied = bytes_copied
                    .checked_add(read as u64)
                    .context("PS5 Itemzflow copy progress overflowed")?;
                progress(*bytes_copied, total_bytes);
            }
            output.sync_all()?;
        }
    }
    Ok(())
}

fn directory_contains_file(path: &Path) -> Result<bool, Error> {
    for entry in fs::read_dir(path).with_context(|| format!("could not read {}", path.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        ensure!(
            !file_type.is_symlink(),
            "PS5 game modules cannot contain symbolic links: {}",
            entry.path().display()
        );
        if file_type.is_file() || (file_type.is_dir() && directory_contains_file(&entry.path())?) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn validate_anyps5_output(
    path: &Path,
) -> Result<crate::forge::backend::ValidationReport, Error> {
    let source_eboot = path.join("source").join("eboot.bin");
    let app0 = path.join("app0");
    let app0_param = app0.join("sce_sys").join("param.json");
    ensure!(
        source_eboot.is_file(),
        "AnyPS5 output lacks source/eboot.bin"
    );
    ensure!(app0.is_dir(), "AnyPS5 output lacks app0/");
    ensure!(
        app0_param.is_file(),
        "AnyPS5 output lacks app0/sce_sys/param.json"
    );
    let mut file = fs::File::open(&source_eboot)?;
    let mut magic = [0u8; 4];
    std::io::Read::read_exact(&mut file, &mut magic)
        .context("AnyPS5 source/eboot.bin is too short")?;
    ensure!(
        magic == *b"\x7fELF",
        "AnyPS5 source/eboot.bin is not an ELF file"
    );
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&app0_param)?)
        .context("AnyPS5 app0/sce_sys/param.json is invalid")?;
    let title_id = json
        .get("titleId")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    ensure!(
        is_ps5_title_id(title_id),
        "AnyPS5 output has an invalid titleId"
    );
    let mut report = crate::forge::backend::ValidationReport::ok();
    report.game_id = Some(title_id.to_owned());
    report.game_title = json
        .get("localizedParameters")
        .and_then(|value| value.get("defaultLanguage"))
        .and_then(|value| value.get("titleName"))
        .and_then(|value| value.as_str())
        .map(str::to_owned);
    report.warnings.push(
        "This is AnyPS5 preparation output; it still requires AnyPS5's relinker and is not a standalone runnable game.".to_owned(),
    );
    Ok(report)
}

fn is_ps5_title_id(value: &str) -> bool {
    value.len() == 9
        && (value.starts_with("PPSA") || value.starts_with("CUSA"))
        && value[4..]
            .chars()
            .all(|character| character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "digital-forge-anyps5-test-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("temporary directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_sfo(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let mut table = Vec::new();
        for (key, value) in entries {
            let key_offset = keys.len() as u16;
            keys.extend_from_slice(key.as_bytes());
            keys.push(0);
            let value_offset = values.len() as u32;
            values.extend_from_slice(value);
            while values.len() % 4 != 0 {
                values.push(0);
            }
            table.extend_from_slice(&key_offset.to_le_bytes());
            table.extend_from_slice(&0x0204u16.to_le_bytes());
            table.extend_from_slice(&((*value).len() as u32).to_le_bytes());
            table.extend_from_slice(&((values.len() as u32) - value_offset).to_le_bytes());
            table.extend_from_slice(&value_offset.to_le_bytes());
        }
        let key_table_offset = 20u32 + table.len() as u32;
        while keys.len() % 4 != 0 {
            keys.push(0);
        }
        let value_table_offset = key_table_offset + keys.len() as u32;
        let mut output = b"\0PSF".to_vec();
        output.extend_from_slice(&0x00000101u32.to_le_bytes());
        output.extend_from_slice(&key_table_offset.to_le_bytes());
        output.extend_from_slice(&value_table_offset.to_le_bytes());
        output.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        output.extend_from_slice(&table);
        output.extend_from_slice(&keys);
        output.extend_from_slice(&values);
        output
    }

    fn make_itemzflow_folder(root: &Path, sfo_title_id: &str, category: &str) {
        fs::create_dir_all(root.join("sce_sys")).expect("metadata folder");
        fs::create_dir_all(root.join("sce_module")).expect("module folder");
        fs::write(root.join("sce_module/libSceTest.sprx"), b"test module").expect("module");
        fs::write(root.join("eboot.bin"), b"prepared executable").expect("eboot");
        fs::write(
            root.join("sce_sys/param.json"),
            br#"{"titleId":"PPSA12345","category":"gd"}"#,
        )
        .expect("param.json");
        let mut title = sfo_title_id.as_bytes().to_vec();
        title.push(0);
        let mut category = category.as_bytes().to_vec();
        category.push(0);
        fs::write(
            root.join("sce_sys/param.sfo"),
            test_sfo(&[("TITLE_ID", &title), ("CATEGORY", &category)]),
        )
        .expect("param.sfo");
    }

    fn append_tar_file(tar: &mut Vec<u8>, name: &str, contents: &[u8]) {
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[108..116].copy_from_slice(b"0000000\0");
        header[116..124].copy_from_slice(b"0000000\0");
        let size = format!("{:011o}\0", contents.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[136..148].copy_from_slice(b"00000000000\0");
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        let checksum: u64 = header.iter().map(|byte| *byte as u64).sum();
        let checksum_text = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum_text.as_bytes());
        tar.extend_from_slice(&header);
        tar.extend_from_slice(contents);
        tar.resize(tar.len().div_ceil(512) * 512, 0);
    }

    fn build_fixture_tar(root: &Path, prefix: &str) -> PathBuf {
        let mut tar = Vec::new();
        append_tar_file(&mut tar, &format!("{prefix}/eboot.bin"), b"native eboot");
        append_tar_file(
            &mut tar,
            &format!("{prefix}/sce_sys/param.json"),
            br#"{"titleId":"PPSA12345","category":"gd"}"#,
        );
        let mut title = b"PPSA12345".to_vec();
        title.push(0);
        let mut category = b"gd".to_vec();
        category.push(0);
        let sfo = test_sfo(&[("TITLE_ID", &title), ("CATEGORY", &category)]);
        append_tar_file(&mut tar, &format!("{prefix}/sce_sys/param.sfo"), &sfo);
        append_tar_file(
            &mut tar,
            &format!("{prefix}/sce_module/test.sprx"),
            b"module",
        );
        tar.extend_from_slice(&[0u8; 1024]);
        let path = root.join("fixture.tar");
        fs::write(&path, tar).expect("fixture TAR");
        path
    }

    #[test]
    fn ps5_tar_extraction_accepts_native_tree_and_rejects_traversal() {
        let temp = TestDirectory::new();
        let archive = build_fixture_tar(&temp.0, "PPSA12345-app0");
        let extracted = temp.0.join("extracted");
        let game = extract_itemzflow_tar(&archive, &extracted, &AtomicBool::new(false))
            .expect("extract valid TAR");
        assert!(game.join("eboot.bin").is_file());
        assert!(game.join("sce_module/test.sprx").is_file());

        let malicious = temp.0.join("traversal.tar");
        let mut tar = Vec::new();
        append_tar_file(&mut tar, "../outside", b"bad");
        tar.extend_from_slice(&[0u8; 1024]);
        fs::write(&malicious, tar).unwrap();
        assert!(
            extract_itemzflow_tar(&malicious, &temp.0.join("unsafe"), &AtomicBool::new(false))
                .is_err()
        );
        assert!(!temp.0.join("outside").exists());
    }

    #[test]
    fn prepared_tar_dump_is_validated_and_exported_as_itemzflow_folder() {
        let temp = TestDirectory::new();
        let archive = build_fixture_tar(&temp.0, "PPSA12345-app0");
        let extraction = temp.0.join("extracted");
        let source = extract_itemzflow_tar(&archive, &extraction, &AtomicBool::new(false))
            .expect("extract dump");
        let output = temp.0.join("output");
        let result =
            copy_prepared_itemzflow_folder(&source, &output, &AtomicBool::new(false), |_, _| {})
                .expect("validate and export Itemzflow directory");
        assert_eq!(result, output.join("Itemzflow"));
        validate_target_folder(&result, TargetWorkflow::Ps5ItemzFlow).unwrap();
    }

    #[test]
    fn itemzflow_folder_validator_accepts_matching_game_metadata() {
        let temp = TestDirectory::new();
        make_itemzflow_folder(&temp.0, "PPSA12345", "gd");

        validate_target_folder(&temp.0, TargetWorkflow::Ps5ItemzFlow)
            .expect("valid prepared folder");
    }

    #[test]
    fn prepared_itemzflow_folder_is_copied_to_output_and_source_is_preserved() {
        let temp = TestDirectory::new();
        let source = temp.0.join("source");
        let output = temp.0.join("output");
        make_itemzflow_folder(&source, "PPSA12345", "gd");
        fs::create_dir_all(source.join("sce_module/subdir")).expect("nested module directory");
        fs::write(
            source.join("sce_module/subdir/extra.sprx"),
            b"second module",
        )
        .expect("nested module");
        let cancelled = AtomicBool::new(false);
        let mut updates = Vec::new();

        let copied = copy_prepared_itemzflow_folder(&source, &output, &cancelled, |done, total| {
            updates.push((done, total));
        })
        .expect("copy prepared folder");

        assert_eq!(copied, output.join("Itemzflow"));
        assert_eq!(
            fs::read(copied.join("sce_module/subdir/extra.sprx")).unwrap(),
            b"second module"
        );
        assert!(source.join("sce_sys/param.json").is_file());
        validate_target_folder(&copied, TargetWorkflow::Ps5ItemzFlow)
            .expect("copied folder passes validation");
        let (done, total) = updates.last().copied().expect("final progress update");
        assert_eq!(done, total);
        assert!(total > 0);
    }

    #[test]
    fn cancelled_itemzflow_copy_removes_partial_output_and_preserves_source() {
        let temp = TestDirectory::new();
        let source = temp.0.join("source");
        let output = temp.0.join("output");
        make_itemzflow_folder(&source, "PPSA12345", "gd");
        let cancelled = AtomicBool::new(false);

        let error = copy_prepared_itemzflow_folder(&source, &output, &cancelled, |done, _| {
            if done > 0 {
                cancelled.store(true, Ordering::Relaxed);
            }
        })
        .expect_err("copy should stop after cancellation");

        assert!(error.to_string().contains("cancelled"));
        assert!(!output.join(".Itemzflow.partial").exists());
        assert!(!output.join("Itemzflow").exists());
        assert!(source.join("eboot.bin").is_file());
    }

    #[test]
    fn itemzflow_folder_validator_rejects_mismatched_sfo_and_json_ids() {
        let temp = TestDirectory::new();
        make_itemzflow_folder(&temp.0, "PPSA54321", "gd");

        let error = validate_target_folder(&temp.0, TargetWorkflow::Ps5ItemzFlow)
            .expect_err("mismatched IDs are rejected");
        assert!(error.to_string().contains("does not match"));
    }

    #[test]
    fn itemzflow_folder_validator_requires_game_modules() {
        let temp = TestDirectory::new();
        make_itemzflow_folder(&temp.0, "PPSA12345", "gd");
        fs::remove_file(temp.0.join("sce_module/libSceTest.sprx")).expect("remove fixture module");
        let error = validate_target_folder(&temp.0, TargetWorkflow::Ps5ItemzFlow)
            .expect_err("missing modules are rejected");
        assert!(error.to_string().contains("contains no game modules"));
    }

    #[test]
    #[cfg(feature = "deferred-console-integration-tests")]
    fn anyps5_output_requires_expected_layout_and_valid_metadata() {
        let temp = TestDirectory::new();
        let root = &temp.0;
        fs::create_dir_all(root.join("source")).expect("source directory");
        fs::create_dir_all(root.join("app0/sce_sys")).expect("app0 metadata directory");
        fs::write(root.join("source/eboot.bin"), b"\x7fELF\x02\x01\x01\0").expect("ELF header");
        fs::write(
            root.join("app0/sce_sys/param.json"),
            br#"{"titleId":"PPSA12345","localizedParameters":{"defaultLanguage":{"titleName":"Test"}}}"#,
        )
        .expect("param.json");

        let report = validate_anyps5_output(root).expect("valid AnyPS5 output");
        assert_eq!(report.game_id.as_deref(), Some("PPSA12345"));
        assert_eq!(report.game_title.as_deref(), Some("Test"));
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("relinker"))
        );
    }

    #[test]
    #[cfg(feature = "deferred-console-integration-tests")]
    fn anyps5_output_rejects_non_elf_eboot() {
        let temp = TestDirectory::new();
        let root = &temp.0;
        fs::create_dir_all(root.join("source")).expect("source directory");
        fs::create_dir_all(root.join("app0/sce_sys")).expect("app0 metadata directory");
        fs::write(root.join("source/eboot.bin"), b"not an ELF").expect("eboot");
        fs::write(
            root.join("app0/sce_sys/param.json"),
            br#"{"titleId":"PPSA12345"}"#,
        )
        .expect("param.json");

        let error = validate_anyps5_output(root).expect_err("invalid ELF is rejected");
        assert!(error.to_string().contains("not an ELF"));
    }
}
