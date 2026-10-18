use std::fs::{self, File};
use std::io::{Read, Write};
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
    let staging = output_dir.join(".Itemzflow.partial");
    ensure!(
        !destination.exists() && !staging.exists(),
        "PS5 Itemzflow output already exists in the operation workspace"
    );
    fs::create_dir_all(&staging)?;
    let result = (|| {
        let mut bytes_copied = 0u64;
        progress(0, total_bytes);
        copy_folder_contents(
            source,
            &staging,
            cancelled,
            &mut bytes_copied,
            total_bytes,
            &mut progress,
        )?;
        ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
        validate_target_folder(&staging, TargetWorkflow::Ps5ItemzFlow)
            .context("copied PS5 Itemzflow folder failed validation")?;
        fs::rename(&staging, &destination)
            .context("could not finalize the copied PS5 Itemzflow folder")?;
        progress(total_bytes, total_bytes);
        Ok(destination)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
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
