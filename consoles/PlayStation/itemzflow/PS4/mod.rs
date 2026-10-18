//! Detection and metadata preflight for Itemzflow Dump All folders.
//!
//! This module only reads source dumps. Packaging and merging remain in
//! Digital Forge's existing PS4 workflow.

use anyhow::{Context, Error, bail, ensure};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpRole {
    BaseGame,
    Update,
}

#[derive(Debug, Clone)]
pub struct Dump {
    pub path: PathBuf,
    pub role: DumpRole,
    pub title_id: String,
    pub title_name: Option<String>,
    pub version: Option<String>,
    pub gp4_status: Gp4Status,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Gp4Status {
    #[default]
    Missing,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub root: PathBuf,
    pub base_marker_found: bool,
    pub update_marker_found: bool,
    pub logs_found: usize,
    pub base_gp4_status: Gp4Status,
    pub update_gp4_status: Option<Gp4Status>,
}

fn ps4_dump_sfo(directory: &Path) -> Option<PathBuf> {
    [
        directory.join("sce_sys").join("param.sfo"),
        directory.join("Image0").join("sce_sys").join("param.sfo"),
        directory.join("Sc0").join("param.sfo"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn gp4_status(directory: &Path, title_id: &str) -> Gp4Status {
    let path = directory.join(format!("{title_id}.gp4"));
    if !path.is_file() {
        return Gp4Status::Missing;
    }
    let Ok(xml) = fs::read_to_string(&path) else {
        return Gp4Status::Invalid;
    };
    let lower = xml.to_ascii_lowercase();
    if !lower.contains("<psproject") {
        return Gp4Status::Invalid;
    }
    let Some(package) = lower
        .split("<package")
        .nth(1)
        .and_then(|rest| rest.split('>').next())
    else {
        return Gp4Status::Invalid;
    };
    let Some(content_id) = xml_attribute(package, "content_id") else {
        return Gp4Status::Invalid;
    };
    let Some(project_title_id) = crate::forge::detector::title_id_from_content_id(&content_id)
    else {
        return Gp4Status::Invalid;
    };
    if !project_title_id.eq_ignore_ascii_case(title_id) {
        return Gp4Status::Invalid;
    }

    let mut reference_count = 0;
    for fragment in xml.split("<file").skip(1) {
        let Some(tag) = fragment.split('>').next() else {
            return Gp4Status::Invalid;
        };
        let Some(reference) = xml_attribute(tag, "orig_path") else {
            continue;
        };
        reference_count += 1;
        let source = PathBuf::from(&reference);
        let resolved = if source.is_absolute() {
            source
        } else {
            directory.join(source)
        };
        if !resolved.is_file() {
            return Gp4Status::Invalid;
        }
    }
    if reference_count == 0 {
        return Gp4Status::Invalid;
    }
    Gp4Status::Valid
}

fn xml_attribute(tag: &str, attribute: &str) -> Option<String> {
    let marker = format!("{attribute}=\"");
    let (_, value) = tag.split_once(&marker)?;
    let (value, _) = value.split_once('"')?;
    Some(
        value
            .replace("&amp;", "&")
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">"),
    )
}

fn inspect_dump(directory: &Path) -> Result<Option<Dump>, Error> {
    let Some(sfo) = ps4_dump_sfo(directory) else {
        return Ok(None);
    };
    let title_id = crate::forge::FormatDetector::read_sfo_key(&sfo, "TITLE_ID")
        .context("PS4 dump SFO is missing TITLE_ID")?;
    ensure!(
        crate::forge::detector::extract_ps4_title_id_from_str(&title_id)
            .is_some_and(|id| id.eq_ignore_ascii_case(&title_id)),
        "PS4 dump SFO has an invalid TITLE_ID: {title_id}"
    );
    let category = crate::forge::FormatDetector::read_sfo_category(&sfo)
        .context("PS4 dump SFO is missing CATEGORY")?;
    let role = match category.to_ascii_lowercase().as_str() {
        "gd" | "gdc" | "gda" => DumpRole::BaseGame,
        "gp" => DumpRole::Update,
        _ => bail!(
            "unsupported PS4 dump CATEGORY '{category}' in {}",
            sfo.display()
        ),
    };
    let eboot = directory.join("eboot.bin");
    ensure!(
        eboot.is_file() && fs::metadata(&eboot)?.len() > 0,
        "PS4 dump is missing a readable, non-empty eboot.bin: {}",
        directory.display()
    );
    ensure!(
        fs::metadata(&sfo)?.len() > 0,
        "PS4 dump has an empty param.sfo: {}",
        directory.display()
    );
    if let Some(content_id) = crate::forge::FormatDetector::read_sfo_content_id(&sfo) {
        let content_title_id = crate::forge::detector::title_id_from_content_id(&content_id)
            .context("PS4 dump SFO has an invalid CONTENT_ID")?;
        ensure!(
            content_title_id.eq_ignore_ascii_case(&title_id),
            "PS4 dump TITLE_ID {title_id} does not match CONTENT_ID {content_id}"
        );
    }
    let version = crate::forge::FormatDetector::read_sfo_key(&sfo, "APP_VER")
        .or_else(|| crate::forge::FormatDetector::read_sfo_key(&sfo, "VERSION"))
        .filter(|value| !value.trim().is_empty())
        .context("PS4 dump SFO is missing APP_VER/VERSION")?;
    ensure!(
        version.contains('.')
            && version
                .chars()
                .all(|character| character.is_ascii_digit() || character == '.'),
        "PS4 dump SFO has an invalid application version: {version}"
    );
    let has_payload_file = fs::read_dir(directory)?.flatten().any(|entry| {
        let path = entry.path();
        path.is_file()
            && !path
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("eboot.bin"))
            && path != sfo
            && path
                .extension()
                .is_none_or(|extension| !extension.eq_ignore_ascii_case("gp4"))
            && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0)
    });
    ensure!(
        has_payload_file,
        "PS4 dump contains no non-empty game or patch payload files: {}",
        directory.display()
    );
    let title_name = crate::forge::FormatDetector::read_sfo_key(&sfo, "TITLE");
    let gp4_status = gp4_status(directory, &title_id);
    Ok(Some(Dump {
        path: directory.to_path_buf(),
        role,
        title_id,
        title_name,
        version: Some(version),
        gp4_status,
    }))
}

/// Detect a dumped payload directly or payload directories immediately under a
/// parent folder. It intentionally does not recurse into game data.
pub fn discover(input: &Path) -> Result<Option<(Vec<Dump>, Evidence)>, Error> {
    if !input.is_dir() {
        return Ok(None);
    }

    let input_name = input
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let (root, mut dumps, direct_input) = if let Some(dump) = inspect_dump(input)? {
        let parent = input.parent().unwrap_or(input);
        let mut siblings = Vec::new();
        if let Ok(entries) = fs::read_dir(parent) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && path != input
                    && let Some(dump) = inspect_dump(&path)?
                {
                    siblings.push(dump);
                }
            }
        }
        let mut all = vec![dump];
        all.extend(siblings);
        (parent.to_path_buf(), all, true)
    } else {
        let mut found = Vec::new();
        if let Ok(entries) = fs::read_dir(input) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && let Some(dump) = inspect_dump(&path)?
                {
                    found.push(dump);
                }
            }
        }
        (input.to_path_buf(), found, false)
    };

    if dumps.is_empty() {
        return Ok(None);
    }
    dumps.sort_by(|a, b| a.path.cmp(&b.path));
    dumps.dedup_by(|a, b| a.path == b.path);

    let has_base_and_patch = dumps.iter().any(|dump| dump.role == DumpRole::BaseGame)
        && dumps.iter().any(|dump| dump.role == DumpRole::Update);
    let conventional_game_update_layout = dumps.iter().all(|dump| {
        dump.path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("game") || name.eq_ignore_ascii_case("update")
            })
    });
    let has_completion_marker = dumps.iter().any(|dump| {
        root.join(format!(
            "{}.complete",
            dump.path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
        ))
        .is_file()
    });
    let title_named_dump = dumps.iter().any(|dump| {
        dump.path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                name.eq_ignore_ascii_case(&dump.title_id)
                    || name.eq_ignore_ascii_case(&format!("{}-patch", dump.title_id))
            })
    });
    let has_gp4 = dumps
        .iter()
        .any(|dump| dump.gp4_status != Gp4Status::Missing);
    let named_itemzflow_parent = input_name.eq_ignore_ascii_case("itemzflow");
    let looks_like_itemzflow = named_itemzflow_parent
        || (has_base_and_patch && !conventional_game_update_layout)
        || has_completion_marker
        || title_named_dump
        || has_gp4;
    if !looks_like_itemzflow {
        return Ok(None);
    }
    if direct_input
        && !named_itemzflow_parent
        && !has_completion_marker
        && !has_gp4
        && !(has_base_and_patch && !conventional_game_update_layout)
        && !title_named_dump
    {
        return Ok(None);
    }

    let mut base_ids = BTreeSet::new();
    let mut update_ids = BTreeSet::new();
    for dump in &dumps {
        match dump.role {
            DumpRole::BaseGame => {
                base_ids.insert(dump.title_id.to_ascii_uppercase());
            }
            DumpRole::Update => {
                update_ids.insert(dump.title_id.to_ascii_uppercase());
            }
        }
    }
    ensure!(
        !base_ids.is_empty(),
        "Itemzflow folder contains a patch dump but no base-game dump"
    );
    ensure!(
        update_ids.iter().all(|id| base_ids.contains(id)),
        "Itemzflow patch title ID does not match any base game in {}",
        root.display()
    );
    ensure!(
        base_ids.len() == 1
            && dumps
                .iter()
                .filter(|d| d.role == DumpRole::BaseGame)
                .count()
                == 1,
        "multiple Itemzflow games were found in {}; import one game folder at a time",
        root.display()
    );
    ensure!(
        dumps.iter().filter(|d| d.role == DumpRole::Update).count() <= 1,
        "multiple patch dumps were found for the same game in {}; import one patch at a time",
        root.display()
    );

    let base = dumps.iter().find(|d| d.role == DumpRole::BaseGame).unwrap();
    let update = dumps.iter().find(|d| d.role == DumpRole::Update);
    let marker = |dump: &Dump| {
        root.join(format!(
            "{}.complete",
            dump.path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
        ))
        .is_file()
    };
    let logs_found = fs::read_dir(&root)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            entry.path().is_file()
                && entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
                && ["itemzflow", "dumper", "daemon"]
                    .iter()
                    .any(|hint| name.contains(hint))
        })
        .count();
    let base_marker_found = marker(base);
    let update_marker_found = update.is_some_and(marker);
    let evidence = Evidence {
        root,
        base_marker_found,
        update_marker_found,
        logs_found,
        base_gp4_status: base.gp4_status,
        update_gp4_status: update.map(|dump| dump.gp4_status),
    };
    Ok(Some((dumps, evidence)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("itemzflow-test-{nonce}"));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn dump(&self, name: &str, category: &[u8], version: &[u8]) -> PathBuf {
            let root = self.0.join(name);
            fs::create_dir_all(root.join("sce_sys")).unwrap();
            fs::write(root.join("eboot.bin"), b"synthetic eboot").unwrap();
            fs::write(root.join("payload.dat"), b"synthetic game data").unwrap();
            let content = b"EP0001-CUSA12345_00-TESTTITLE00000000\0";
            let entries: Vec<(&str, u16, &[u8])> = vec![
                ("APP_VER", 0x0204, version),
                ("CATEGORY", 0x0204, category),
                ("CONTENT_ID", 0x0204, content),
                ("TITLE", 0x0204, b"Synthetic Game\0"),
                ("TITLE_ID", 0x0204, b"CUSA12345\0"),
            ];
            fs::write(root.join("sce_sys/param.sfo"), sfo(&entries)).unwrap();
            root
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sfo(entries: &[(&str, u16, &[u8])]) -> Vec<u8> {
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let mut table = Vec::new();
        for (key, format, value) in entries {
            let key_offset = keys.len() as u16;
            keys.extend_from_slice(key.as_bytes());
            keys.push(0);
            let value_offset = values.len() as u32;
            values.extend_from_slice(value);
            while values.len() % 4 != 0 {
                values.push(0);
            }
            let max_len = values.len() as u32 - value_offset;
            table.extend_from_slice(&key_offset.to_le_bytes());
            table.extend_from_slice(&format.to_le_bytes());
            table.extend_from_slice(&(value.len() as u32).to_le_bytes());
            table.extend_from_slice(&max_len.to_le_bytes());
            table.extend_from_slice(&value_offset.to_le_bytes());
        }
        let key_offset = 20 + table.len() as u32;
        while keys.len() % 4 != 0 {
            keys.push(0);
        }
        let value_offset = key_offset + keys.len() as u32;
        let mut output = b"\0PSF".to_vec();
        output.extend_from_slice(&0x00000101u32.to_le_bytes());
        output.extend_from_slice(&key_offset.to_le_bytes());
        output.extend_from_slice(&value_offset.to_le_bytes());
        output.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        output.extend(table);
        output.extend(keys);
        output.extend(values);
        output
    }

    #[test]
    fn discovers_base_patch_and_optional_itemzflow_files() {
        let fixture = Fixture::new();
        let base = fixture.dump("CUSA12345", b"gd\0", b"01.00\0");
        let update = fixture.dump("CUSA12345-patch", b"gp\0", b"01.29\0");
        let gp4 = "<psproject><package content_id=\"EP0001-CUSA12345_00-TESTTITLE00000000\"/><file targ_path=\"eboot.bin\" orig_path=\"eboot.bin\"/></psproject>";
        fs::write(base.join("CUSA12345.gp4"), gp4).unwrap();
        fs::write(update.join("CUSA12345.gp4"), gp4).unwrap();
        fs::write(fixture.0.join("CUSA12345.complete"), []).unwrap();
        fs::write(fixture.0.join("CUSA12345-patch.complete"), []).unwrap();
        fs::write(fixture.0.join("itemzflow_app.log"), b"dump finished").unwrap();

        let (dumps, evidence) = discover(&fixture.0).unwrap().unwrap();
        assert_eq!(dumps.len(), 2);
        assert_eq!(dumps[0].role, DumpRole::BaseGame);
        assert_eq!(dumps[1].role, DumpRole::Update);
        assert_eq!(dumps[1].version.as_deref(), Some("01.29"));
        assert!(evidence.base_marker_found && evidence.update_marker_found);
        assert_eq!(evidence.logs_found, 1);
        assert_eq!(evidence.base_gp4_status, Gp4Status::Valid);
        assert_eq!(evidence.update_gp4_status, Some(Gp4Status::Valid));
    }

    #[test]
    fn accepts_manual_parent_without_optional_logs_or_markers() {
        let fixture = Fixture::new();
        fixture.dump("CUSA12345", b"gd\0", b"01.00\0");
        fixture.dump("CUSA12345-patch", b"gp\0", b"01.29\0");
        let (dumps, evidence) = discover(&fixture.0).unwrap().unwrap();
        assert_eq!(dumps.len(), 2);
        assert!(!evidence.base_marker_found && !evidence.update_marker_found);
        assert_eq!(evidence.logs_found, 0);
    }

    #[test]
    fn leaves_the_existing_game_update_folder_contract_alone() {
        let fixture = Fixture::new();
        fixture.dump("Game", b"gd\0", b"01.00\0");
        fixture.dump("Update", b"gp\0", b"01.29\0");
        assert!(discover(&fixture.0).unwrap().is_none());
    }

    #[test]
    fn rejects_mismatched_patch_ids_and_zero_byte_essential_files() {
        let fixture = Fixture::new();
        fixture.dump("CUSA12345", b"gd\0", b"01.00\0");
        let mismatched = fixture.dump("other-patch", b"gp\0", b"01.29\0");
        let sfo_path = mismatched.join("sce_sys/param.sfo");
        let raw = fs::read(&sfo_path).unwrap();
        // Change the fixed-width TITLE_ID value without changing table layout.
        let needle = b"CUSA12345\0";
        let offset = raw
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap();
        let mut changed = raw;
        changed[offset..offset + needle.len()].copy_from_slice(b"CUSA99999\0");
        fs::write(sfo_path, changed).unwrap();
        assert!(discover(&fixture.0).is_err());

        fs::remove_dir_all(&fixture.0).unwrap();
        fs::create_dir_all(fixture.0.join("CUSA12345/sce_sys")).unwrap();
        fs::write(fixture.0.join("CUSA12345/eboot.bin"), []).unwrap();
        fs::write(
            fixture.0.join("CUSA12345/sce_sys/param.sfo"),
            sfo(&[
                ("TITLE_ID", 0x0204, b"CUSA12345\0"),
                ("CATEGORY", 0x0204, b"gd\0"),
            ]),
        )
        .unwrap();
        assert!(discover(&fixture.0).is_err());
    }

    #[test]
    fn marks_gp4_with_missing_source_paths_for_workspace_regeneration() {
        let fixture = Fixture::new();
        let base = fixture.dump("CUSA12345", b"gd\0", b"01.00\0");
        fs::write(
            base.join("CUSA12345.gp4"),
            "<psproject><package content_id=\"EP0001-CUSA12345_00-TESTTITLE00000000\"/><file targ_path=\"missing.dat\" orig_path=\"missing.dat\"/></psproject>",
        )
        .unwrap();
        let (_, evidence) = discover(&fixture.0).unwrap().unwrap();
        assert_eq!(evidence.base_gp4_status, Gp4Status::Invalid);
    }
}
