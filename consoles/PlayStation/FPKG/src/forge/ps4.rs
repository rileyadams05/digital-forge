use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Error, bail, ensure};

use super::detector::{FormatDetector, SfoEntry, parse_sfo_entries};
use super::profile::TargetWorkflow;

const PS4_SELF_MAGIC: &[u8; 4] = b"\x4f\x15\x3d\x1d";
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const SELF_HEADER_SIZE: usize = 0x20;
const SELF_ENTRY_SIZE: usize = 0x20;
const SELF_PROGRAM_TYPE_FAKE: u64 = 1;
const SELF_PROGRAM_TYPE_NPDRM_DYNLIB: u64 = 5;
const ELF_TYPE_SCE_EXEC: u16 = 0xfe10;
const ELF_TYPE_SCE_DYNLIB: u16 = 0xfe18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ps4ExecutableState {
    FakeSignedPlaintext,
    SonySignedSystemModule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ps4ExecutableReport {
    pub relative_path: PathBuf,
    pub state: Ps4ExecutableState,
    pub program_type: u64,
    pub authority_id: u64,
    pub entry_count: u16,
    pub encrypted_entries: usize,
}

/// Create a standalone base-application SFO after overlaying a patch.
///
/// The base supplies identity and entitlement fields; the patch supplies
/// version requirements. Values are patched into the original base SFO slots
/// instead of rebuilding the file. PS4 sysfile parsers require aligned table
/// offsets and the old rebuild produced a file ItemzFlow could index but
/// sceLncUtil rejected with SCE_SYSFILE_ERROR_INVAL (0x80120003).
pub fn normalize_itemzflow_update_sfo(
    base_sfo: &Path,
    update_sfo: &Path,
    output_sfo: &Path,
) -> Result<(), Error> {
    let mut base_bytes = fs::read(base_sfo)
        .with_context(|| format!("could not read base SFO {}", base_sfo.display()))?;
    let update_bytes = fs::read(update_sfo)
        .with_context(|| format!("could not read update SFO {}", update_sfo.display()))?;
    validate_sfo_layout(&base_bytes).context("base param.sfo has an invalid PS4 layout")?;
    validate_sfo_layout(&update_bytes).context("update param.sfo has an invalid PS4 layout")?;

    let base_entries = parse_sfo_entries(&base_bytes).context("base param.sfo is invalid")?;
    let update_entries = parse_sfo_entries(&update_bytes).context("update param.sfo is invalid")?;
    let base_title = string_entry(&base_entries, "TITLE_ID").context("base SFO lacks TITLE_ID")?;
    let update_title =
        string_entry(&update_entries, "TITLE_ID").context("update SFO lacks TITLE_ID")?;
    ensure!(
        base_title.eq_ignore_ascii_case(&update_title),
        "update TITLE_ID {update_title} does not match base TITLE_ID {base_title}"
    );
    ensure!(
        string_entry(&base_entries, "CATEGORY").as_deref() == Some("gd"),
        "base param.sfo is not a PS4 base application (expected CATEGORY=gd)"
    );
    ensure!(
        string_entry(&update_entries, "CATEGORY").as_deref() == Some("gp"),
        "update param.sfo is not a PS4 patch (expected CATEGORY=gp)"
    );

    for key in ["APP_VER", "VERSION"] {
        if let Some(value) = string_entry(&update_entries, key) {
            patch_sfo_string(&mut base_bytes, key, &value)?;
        }
    }
    if let Some(update_system) = integer_entry(&update_entries, "SYSTEM_VER") {
        let chosen = integer_entry(&base_entries, "SYSTEM_VER")
            .unwrap_or(0)
            .max(update_system);
        patch_sfo_integer(&mut base_bytes, "SYSTEM_VER", chosen)?;
    }
    if integer_entry(&base_entries, "DOWNLOAD_DATA_SIZE").is_some() {
        patch_sfo_integer(&mut base_bytes, "DOWNLOAD_DATA_SIZE", 0)?;
    }

    validate_sfo_layout(&base_bytes).context("normalized param.sfo is invalid")?;
    fs::write(output_sfo, base_bytes)
        .with_context(|| format!("could not write normalized SFO {}", output_sfo.display()))?;
    Ok(())
}

pub fn validate_target_folder(path: &Path, workflow: TargetWorkflow) -> Result<(), Error> {
    let sfo = path.join("sce_sys").join("param.sfo");
    ensure!(sfo.is_file(), "target folder lacks sce_sys/param.sfo");
    validate_sfo_layout(&fs::read(&sfo)?).context("sce_sys/param.sfo is not PS4 sysfile-safe")?;
    let (title_id, _) = FormatDetector::read_sfo(&sfo);
    let title_id = title_id.context("param.sfo lacks TITLE_ID")?;
    ensure!(
        is_ps4_title_id(&title_id),
        "invalid PS4 TITLE_ID {title_id}"
    );
    ensure!(
        path.file_name().is_none_or(|name| {
            !name.to_string_lossy().starts_with("CUSA") || name == title_id.as_str()
        }),
        "application directory name does not match SFO TITLE_ID {title_id}"
    );
    let category = FormatDetector::read_sfo_category(&sfo).unwrap_or_default();
    ensure!(
        matches!(category.as_str(), "gd" | "gdc" | "gda"),
        "combined PS4 application must have a base-game CATEGORY, found {category}"
    );

    let eboot = path.join("eboot.bin");
    ensure!(eboot.is_file(), "target folder lacks eboot.bin");
    match workflow {
        TargetWorkflow::Ps4ItemzFlow => {
            validate_itemzflow_executables(path)?;
            ensure!(
                !path.join("addcont").exists(),
                "nested addcont is not an ItemzFlow-supported DLC layout"
            );
        }
        TargetWorkflow::Ps4ShadPs4 => {
            let header = read_prefix(&eboot, 16)?;
            ensure!(
                header.starts_with(PS4_SELF_MAGIC) || header.starts_with(ELF_MAGIC),
                "shadPS4 target eboot.bin is neither SELF/FSELF nor ELF"
            );
        }
        _ => bail!("PS4 folder validator was called for the wrong target workflow"),
    }
    Ok(())
}

/// Validate every executable container, not just the eboot magic. Fake game
/// code must be a plaintext fake SELF. Sony's special right.sprx is accepted
/// either in that form or as the genuine encrypted NPDRM system module used by
/// retail/FPKG layouts; it is not game code and is handled specially by PS4
/// publishing and loading tools.
pub fn validate_itemzflow_executables(root: &Path) -> Result<Vec<Ps4ExecutableReport>, Error> {
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    let mut reports = Vec::new();
    let mut saw_eboot = false;

    for path in files {
        let relative = path.strip_prefix(root)?.to_path_buf();
        let normalized = relative.to_string_lossy().replace('\\', "/");
        let extension_is_module = path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("prx") || extension.eq_ignore_ascii_case("sprx")
        });
        let prefix = read_file_prefix(&path, 4)?;
        let is_self = prefix.starts_with(PS4_SELF_MAGIC);
        let is_eboot = normalized.eq_ignore_ascii_case("eboot.bin");

        if !is_eboot && !extension_is_module && !is_self {
            continue;
        }
        ensure!(
            is_self,
            "{} is executable content but is not a SELF/FSELF container",
            relative.display()
        );

        let parsed = parse_self(&path)?;
        let is_right = normalized.eq_ignore_ascii_case("sce_sys/about/right.sprx");
        let expected_elf_type = if is_eboot {
            ELF_TYPE_SCE_EXEC
        } else {
            ELF_TYPE_SCE_DYNLIB
        };
        ensure!(
            parsed.elf_type == expected_elf_type,
            "{} has unexpected embedded ELF type 0x{:04x}",
            relative.display(),
            parsed.elf_type
        );

        let state = if parsed.program_type == SELF_PROGRAM_TYPE_FAKE {
            ensure!(
                parsed.key_type == 0x101,
                "{} claims to be fake-signed but does not use the fake SELF key type",
                relative.display()
            );
            ensure!(
                parsed.encrypted_entries == 0,
                "{} claims to be fake-signed but still has encrypted SELF entries",
                relative.display()
            );
            Ps4ExecutableState::FakeSignedPlaintext
        } else if is_right && parsed.program_type == SELF_PROGRAM_TYPE_NPDRM_DYNLIB {
            ensure!(
                parsed.key_type == 0x501,
                "sce_sys/about/right.sprx does not use the expected NPDRM SELF key type"
            );
            ensure!(
                parsed.encrypted_entries == parsed.entry_count as usize,
                "sce_sys/about/right.sprx has a mixed encrypted/plaintext signed state"
            );
            Ps4ExecutableState::SonySignedSystemModule
        } else {
            bail!(
                "{} is not externally fake-signed for ItemzFlow (SELF program type {}, {} encrypted entries)",
                relative.display(),
                parsed.program_type,
                parsed.encrypted_entries
            );
        };

        if is_eboot {
            saw_eboot = true;
        }
        reports.push(Ps4ExecutableReport {
            relative_path: relative,
            state,
            program_type: parsed.program_type,
            authority_id: parsed.authority_id,
            entry_count: parsed.entry_count,
            encrypted_entries: parsed.encrypted_entries,
        });
    }

    ensure!(
        saw_eboot,
        "ItemzFlow application has no validated eboot.bin"
    );
    ensure!(
        !reports.is_empty(),
        "ItemzFlow application has no executable containers"
    );
    Ok(reports)
}

pub fn validate_patch_folder(path: &Path, expected_title_id: &str) -> Result<(), Error> {
    let sfo = path.join("sce_sys").join("param.sfo");
    ensure!(sfo.is_file(), "shadPS4 patch lacks sce_sys/param.sfo");
    validate_sfo_layout(&fs::read(&sfo)?).context("patch param.sfo has an invalid layout")?;
    let (title_id, _) = FormatDetector::read_sfo(&sfo);
    ensure!(
        title_id.as_deref() == Some(expected_title_id),
        "patch TITLE_ID does not match the base game"
    );
    ensure!(
        FormatDetector::read_sfo_category(&sfo).as_deref() == Some("gp"),
        "shadPS4 patch must retain CATEGORY=gp"
    );
    Ok(())
}

pub fn validate_shad_addons(path: &Path, expected_title_id: &str) -> Result<(), Error> {
    ensure!(
        path.is_dir(),
        "shadPS4 additional-content output is missing"
    );
    let mut found = 0usize;
    for entry in fs::read_dir(path)? {
        let addon = entry?.path();
        if !addon.is_dir() {
            continue;
        }
        let sfo = addon.join("sce_sys").join("param.sfo");
        ensure!(
            sfo.is_file(),
            "DLC folder {} lacks sce_sys/param.sfo",
            addon.display()
        );
        validate_sfo_layout(&fs::read(&sfo)?).context("DLC param.sfo has an invalid layout")?;
        let (title_id, _) = FormatDetector::read_sfo(&sfo);
        ensure!(
            title_id.as_deref() == Some(expected_title_id),
            "DLC TITLE_ID does not match the base game"
        );
        ensure!(
            FormatDetector::read_sfo_category(&sfo).as_deref() == Some("ac"),
            "DLC folder is not CATEGORY=ac"
        );
        found += 1;
    }
    ensure!(
        found > 0,
        "additional-content output contains no DLC folders"
    );
    Ok(())
}

#[derive(Debug)]
struct ParsedSelf {
    key_type: u32,
    authority_id: u64,
    program_type: u64,
    elf_type: u16,
    entry_count: u16,
    encrypted_entries: usize,
}

fn parse_self(path: &Path) -> Result<ParsedSelf, Error> {
    let bytes = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    ensure!(
        bytes.len() >= SELF_HEADER_SIZE,
        "{} is too short for a SELF header",
        path.display()
    );
    ensure!(
        bytes.starts_with(PS4_SELF_MAGIC),
        "{} has invalid SELF magic",
        path.display()
    );
    ensure!(
        bytes[4] == 0,
        "{} uses unsupported SELF version {}",
        path.display(),
        bytes[4]
    );
    ensure!(
        bytes[5] == 1 && bytes[6] == 1,
        "{} has invalid SELF mode/byte order",
        path.display()
    );
    let key_type = le_u32(&bytes, 8)?;
    ensure!(
        matches!(key_type, 0x101 | 0x501),
        "{} has invalid SELF key type",
        path.display()
    );

    let header_size = le_u16(&bytes, 12)? as usize;
    let meta_size = le_u16(&bytes, 14)? as usize;
    let entry_count = le_u16(&bytes, 24)?;
    ensure!(
        entry_count > 0 && entry_count % 2 == 0,
        "{} has an invalid SELF entry count",
        path.display()
    );
    ensure!(
        header_size
            .checked_add(meta_size)
            .is_some_and(|end| end <= bytes.len()),
        "{} has SELF metadata outside the file",
        path.display()
    );

    let entry_table_end = SELF_HEADER_SIZE
        .checked_add(entry_count as usize * SELF_ENTRY_SIZE)
        .context("SELF entry table overflow")?;
    ensure!(
        entry_table_end <= bytes.len(),
        "{} has a truncated SELF entry table",
        path.display()
    );
    let elf_offset = entry_table_end;
    ensure!(
        elf_offset + 64 <= bytes.len(),
        "{} has no embedded ELF header",
        path.display()
    );
    ensure!(
        &bytes[elf_offset..elf_offset + 4] == ELF_MAGIC,
        "{} has invalid embedded ELF magic",
        path.display()
    );
    ensure!(
        bytes[elf_offset + 4] == 2 && bytes[elf_offset + 5] == 1,
        "{} is not a little-endian ELF64 SELF",
        path.display()
    );
    let elf_type = le_u16(&bytes, elf_offset + 16)?;
    ensure!(
        le_u16(&bytes, elf_offset + 18)? == 62,
        "{} embedded ELF is not x86-64",
        path.display()
    );
    ensure!(
        le_u16(&bytes, elf_offset + 52)? == 64,
        "{} has an invalid ELF header size",
        path.display()
    );
    let ph_offset: usize = le_u64(&bytes, elf_offset + 32)?
        .try_into()
        .context("ELF program-header offset overflow")?;
    let ph_entry_size = le_u16(&bytes, elf_offset + 54)? as usize;
    let ph_count = le_u16(&bytes, elf_offset + 56)? as usize;
    ensure!(
        ph_entry_size == 56 && ph_count > 0,
        "{} has an invalid ELF program-header table",
        path.display()
    );
    let ph_end = elf_offset
        .checked_add(ph_offset)
        .and_then(|start| start.checked_add(ph_entry_size * ph_count))
        .context("ELF program-header table overflow")?;
    ensure!(
        ph_end <= bytes.len(),
        "{} has a truncated ELF program-header table",
        path.display()
    );
    let extended_offset = align_up(ph_end, 16)?;
    ensure!(
        extended_offset + 64 <= header_size,
        "{} has a truncated SELF extended-info block",
        path.display()
    );
    let authority_id = le_u64(&bytes, extended_offset)?;
    let program_type = le_u64(&bytes, extended_offset + 8)?;
    ensure!(
        bytes[extended_offset + 32..extended_offset + 64]
            .iter()
            .any(|byte| *byte != 0),
        "{} has no extended-info digest",
        path.display()
    );

    let mut encrypted_entries = 0usize;
    let mut data_segments = HashMap::new();
    for index in 0..entry_count as usize {
        let offset = SELF_HEADER_SIZE + index * SELF_ENTRY_SIZE;
        let properties = le_u64(&bytes, offset)?;
        let file_offset: usize = le_u64(&bytes, offset + 8)?
            .try_into()
            .context("SELF entry offset overflow")?;
        let file_size: usize = le_u64(&bytes, offset + 16)?
            .try_into()
            .context("SELF entry size overflow")?;
        ensure!(
            (properties >> 2) & 1 == 1,
            "{} SELF entry {index} is not signed",
            path.display()
        );
        if (properties >> 1) & 1 == 1 {
            encrypted_entries += 1;
        }
        ensure!(
            file_offset
                .checked_add(file_size)
                .is_some_and(|end| end <= bytes.len()),
            "{} SELF entry {index} points outside the file",
            path.display()
        );
        if (properties >> 11) & 1 == 1 {
            let segment = ((properties >> 20) & 0xffff) as usize;
            ensure!(
                segment < ph_count,
                "{} SELF entry {index} maps to an invalid ELF segment",
                path.display()
            );
            ensure!(
                data_segments.insert(segment, file_size).is_none(),
                "{} maps ELF segment {segment} more than once",
                path.display()
            );
        }
    }

    for index in 0..ph_count {
        let offset = elf_offset + ph_offset + index * ph_entry_size;
        let kind = le_u32(&bytes, offset)?;
        let file_size: usize = le_u64(&bytes, offset + 32)?
            .try_into()
            .context("ELF segment size overflow")?;
        if file_size > 0 && matches!(kind, 1 | 0x6100_0000 | 0x6100_0010 | 0x6fff_ff00) {
            ensure!(
                data_segments.get(&index).copied() == Some(file_size),
                "{} is missing the SELF data entry for ELF segment {index}",
                path.display()
            );
        }
    }

    Ok(ParsedSelf {
        key_type,
        authority_id,
        program_type,
        elf_type,
        entry_count,
        encrypted_entries,
    })
}

fn validate_sfo_layout(bytes: &[u8]) -> Result<(), Error> {
    ensure!(
        bytes.len() >= 20 && bytes.starts_with(b"\x00PSF"),
        "invalid SFO header"
    );
    ensure!(le_u32(bytes, 4)? == 0x0000_0101, "unsupported SFO version");
    let key_offset = le_u32(bytes, 8)? as usize;
    let value_offset = le_u32(bytes, 12)? as usize;
    let count = le_u32(bytes, 16)? as usize;
    let descriptor_end = 20usize
        .checked_add(count.checked_mul(16).context("SFO descriptor overflow")?)
        .context("SFO descriptor overflow")?;
    ensure!(
        descriptor_end <= key_offset && key_offset <= value_offset && value_offset <= bytes.len(),
        "invalid SFO table offsets"
    );
    ensure!(
        key_offset % 4 == 0 && value_offset % 4 == 0,
        "SFO key/value tables are not 4-byte aligned"
    );

    let mut keys = HashSet::new();
    for index in 0..count {
        let descriptor = 20 + index * 16;
        let key_relative = le_u16(bytes, descriptor)? as usize;
        let format = le_u16(bytes, descriptor + 2)?;
        let length = le_u32(bytes, descriptor + 4)? as usize;
        let maximum = le_u32(bytes, descriptor + 8)? as usize;
        let value_relative = le_u32(bytes, descriptor + 12)? as usize;
        let key_start = key_offset
            .checked_add(key_relative)
            .context("SFO key offset overflow")?;
        ensure!(
            key_start < value_offset,
            "SFO key {index} starts outside the key table"
        );
        let key_end = bytes[key_start..value_offset]
            .iter()
            .position(|byte| *byte == 0)
            .map(|position| key_start + position)
            .context("SFO key is not NUL terminated")?;
        let key =
            std::str::from_utf8(&bytes[key_start..key_end]).context("SFO key is not UTF-8")?;
        ensure!(
            !key.is_empty() && keys.insert(key.to_owned()),
            "SFO contains an empty or duplicate key"
        );
        ensure!(
            length <= maximum,
            "SFO value {key} exceeds its declared maximum"
        );
        ensure!(
            value_relative % 4 == 0,
            "SFO value {key} is not 4-byte aligned"
        );
        let value_start = value_offset
            .checked_add(value_relative)
            .context("SFO value offset overflow")?;
        let value_end = value_start
            .checked_add(maximum)
            .context("SFO value size overflow")?;
        ensure!(
            value_end <= bytes.len(),
            "SFO value {key} points outside the file"
        );
        match format {
            0x0204 => ensure!(
                length > 0 && bytes[value_start + length - 1] == 0,
                "SFO string {key} is not NUL terminated"
            ),
            0x0404 => ensure!(
                length == 4 && maximum >= 4,
                "SFO integer {key} has an invalid size"
            ),
            _ => bail!("SFO key {key} uses unsupported format 0x{format:04x}"),
        }
    }
    Ok(())
}

fn patch_sfo_string(bytes: &mut [u8], key: &str, value: &str) -> Result<(), Error> {
    let mut data = value.as_bytes().to_vec();
    data.push(0);
    patch_sfo_value(bytes, key, &data, 0x0204)
}

fn patch_sfo_integer(bytes: &mut [u8], key: &str, value: u32) -> Result<(), Error> {
    patch_sfo_value(bytes, key, &value.to_le_bytes(), 0x0404)
}

fn patch_sfo_value(
    bytes: &mut [u8],
    target: &str,
    data: &[u8],
    expected_format: u16,
) -> Result<(), Error> {
    validate_sfo_layout(bytes)?;
    let key_offset = le_u32(bytes, 8)? as usize;
    let value_offset = le_u32(bytes, 12)? as usize;
    let count = le_u32(bytes, 16)? as usize;
    for index in 0..count {
        let descriptor = 20 + index * 16;
        let key_start = key_offset + le_u16(bytes, descriptor)? as usize;
        let key_end = bytes[key_start..value_offset]
            .iter()
            .position(|byte| *byte == 0)
            .map(|position| key_start + position)
            .context("SFO key is not NUL terminated")?;
        if &bytes[key_start..key_end] != target.as_bytes() {
            continue;
        }
        let format = le_u16(bytes, descriptor + 2)?;
        ensure!(
            format == expected_format,
            "SFO key {target} has the wrong value type"
        );
        let maximum = le_u32(bytes, descriptor + 8)? as usize;
        ensure!(
            data.len() <= maximum,
            "SFO value {target} does not fit its original slot"
        );
        let start = value_offset + le_u32(bytes, descriptor + 12)? as usize;
        bytes[start..start + maximum].fill(0);
        bytes[start..start + data.len()].copy_from_slice(data);
        bytes[descriptor + 4..descriptor + 8].copy_from_slice(&(data.len() as u32).to_le_bytes());
        return Ok(());
    }
    bail!("base SFO lacks {target}")
}

fn collect_files(root: &Path, output: &mut Vec<PathBuf>) -> Result<(), Error> {
    for entry in fs::read_dir(root).with_context(|| format!("could not read {}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files(&path, output)?;
        } else if path.is_file() {
            output.push(path);
        }
    }
    Ok(())
}

fn read_prefix(path: &Path, length: usize) -> Result<Vec<u8>, Error> {
    let mut file =
        File::open(path).with_context(|| format!("could not read {}", path.display()))?;
    let mut bytes = vec![0u8; length];
    file.read_exact(&mut bytes)
        .with_context(|| format!("{} is too short", path.display()))?;
    Ok(bytes)
}

fn read_file_prefix(path: &Path, length: usize) -> Result<Vec<u8>, Error> {
    let mut file =
        File::open(path).with_context(|| format!("could not read {}", path.display()))?;
    let mut bytes = vec![0u8; length];
    let read = file.read(&mut bytes)?;
    bytes.truncate(read);
    Ok(bytes)
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let data = bytes
        .get(offset..offset + 2)
        .context("truncated 16-bit field")?;
    Ok(u16::from_le_bytes(data.try_into().unwrap()))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let data = bytes
        .get(offset..offset + 4)
        .context("truncated 32-bit field")?;
    Ok(u32::from_le_bytes(data.try_into().unwrap()))
}

fn le_u64(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let data = bytes
        .get(offset..offset + 8)
        .context("truncated 64-bit field")?;
    Ok(u64::from_le_bytes(data.try_into().unwrap()))
}

fn align_up(value: usize, alignment: usize) -> Result<usize, Error> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
        .context("alignment overflow")
}

fn is_ps4_title_id(value: &str) -> bool {
    value.len() == 9
        && value.starts_with("CUSA")
        && value[4..]
            .chars()
            .all(|character| character.is_ascii_digit())
}

fn string_entry(entries: &[SfoEntry], key: &str) -> Option<String> {
    let entry = entries
        .iter()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))?;
    Some(
        String::from_utf8_lossy(&entry.data)
            .trim_end_matches('\0')
            .to_owned(),
    )
}

fn integer_entry(entries: &[SfoEntry], key: &str) -> Option<u32> {
    let entry = entries
        .iter()
        .find(|entry| entry.key.eq_ignore_ascii_case(key))?;
    (entry.data.len() >= 4).then(|| u32::from_le_bytes(entry.data[..4].try_into().unwrap()))
}

pub fn find_sfo(root: &Path) -> Option<PathBuf> {
    [
        root.join("Image0").join("sce_sys").join("param.sfo"),
        root.join("Sc0").join("param.sfo"),
        root.join("sce_sys").join("param.sfo"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}
