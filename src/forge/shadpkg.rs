//! Managed ShadPKG v2.1.0 extraction. OrbisPkgTool remains the inspector/builder.
//! A successful process exit alone never establishes successful decryption.
use super::{FormatDetector, ProgressUpdate};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub const VERSION: &str = "2.1.0";
pub const WINDOWS_SHA256: &str = "31d932ba5129ed50ecd4738dcf720b9c47881d1cb1ea41817651387446d90068";
static NEXT_JOB: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct PackageIdentity {
    pub title_id: String,
    pub content_id: String,
    pub version: String,
    pub category: String,
    pub no_data_addon: bool,
}

impl PackageIdentity {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.title_id.len() == 9
                && self.title_id.starts_with("CUSA")
                && self.title_id[4..].bytes().all(|b| b.is_ascii_digit()),
            "invalid PS4 title ID {}",
            self.title_id
        );
        ensure!(
            self.content_id.len() == 36
                && self
                    .content_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid PS4 content ID {}",
            self.content_id
        );
        ensure!(
            super::detector::title_id_from_content_id(&self.content_id).as_deref()
                == Some(&self.title_id),
            "package content ID does not match title ID"
        );
        ensure!(
            matches!(self.category.as_str(), "gd" | "gdc" | "gda" | "gp" | "ac"),
            "unsupported PS4 package category {}",
            self.category
        );
        ensure!(!self.version.is_empty(), "package has no APP_VER");
        Ok(())
    }
}

pub fn inspect_package(source: &Path) -> Result<PackageIdentity> {
    let tool = super::backends::find_native_orbis_pkg_tool()
        .context("managed OrbisPkgTool inspector is missing")?;
    let output = Command::new(tool).arg("info").arg(source).output()?;
    ensure!(
        output.status.success(),
        "PKG metadata inspection failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let field = |name: &str| {
        text.lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.trim().to_owned())
            })
            .unwrap_or_default()
    };
    let kind = field("Type").to_ascii_lowercase();
    let identity = PackageIdentity {
        title_id: field("Title ID"),
        content_id: field("Content ID"),
        version: field("App version"),
        category: field("Category").to_ascii_lowercase(),
        no_data_addon: kind.contains("no data")
            || kind.contains("nodata")
            || kind.contains("no-data"),
    };
    identity.validate()?;
    Ok(identity)
}

pub fn find_tool() -> Result<PathBuf> {
    ensure!(
        cfg!(all(target_os = "windows", target_arch = "x86_64")),
        "managed ShadPKG v2.1.0 is currently provided for Windows x64"
    );
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("DIGITAL_FORGE_SHADPKG") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
    {
        candidates.push(dir.join("runtime/ps4/ShadPKG/shadpkg.exe"));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("consoles/PlayStation/FPKG/tools/ShadPKG/shadpkg.exe"),
    );
    let tool = candidates.into_iter().find(|path| path.is_file()).context(
        "managed ShadPKG v2.1.0 is missing; reinstall Digital Forge's Windows x64 runtime",
    )?;
    let actual = format!("{:x}", Sha256::digest(fs::read(&tool)?));
    ensure!(
        actual == WINDOWS_SHA256,
        "managed ShadPKG checksum mismatch at {}; refusing to execute",
        tool.display()
    );
    Ok(tool)
}

struct Stage(PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug)]
pub struct ExtractionReport {
    pub output_path: PathBuf,
    pub identity: PackageIdentity,
    pub payload_files: usize,
    pub payload_bytes: u64,
    pub diagnostics: String,
}

fn read_log_tail(path: &Path) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(16384)));
    let mut bytes = Vec::new();
    let _ = file.take(16384).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn scan_payload(root: &Path, path: &Path, depth: usize) -> Result<(usize, u64)> {
    ensure!(
        depth <= 64,
        "extracted directory nesting exceeds safety limit"
    );
    let mut count = 0;
    let mut bytes = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "extracted output contains a link: {}",
            entry.path().display()
        );
        let relative = entry.path().strip_prefix(root)?.to_path_buf();
        let metadata = relative
            .components()
            .next()
            .is_some_and(|name| name.as_os_str().eq_ignore_ascii_case("sce_sys"));
        if kind.is_dir() {
            let (child_count, child_bytes) = scan_payload(root, &entry.path(), depth + 1)?;
            if !metadata {
                count += child_count;
                bytes += child_bytes;
            }
        } else if kind.is_file() && !metadata {
            let size = entry.metadata()?.len();
            // Unknown raw package entries and extractor logs are not game files.
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if size > 0 && !name.starts_with("entry_0x") && !name.ends_with(".log") {
                count += 1;
                bytes += size;
            }
        }
    }
    Ok((count, bytes))
}

pub fn validate_output(root: &Path, expected: &PackageIdentity) -> Result<(usize, u64)> {
    expected.validate()?;
    let sfo = root.join("sce_sys/param.sfo");
    for (key, value) in [
        ("TITLE_ID", &expected.title_id),
        ("CONTENT_ID", &expected.content_id),
        ("CATEGORY", &expected.category),
        ("APP_VER", &expected.version),
    ] {
        let found = FormatDetector::read_sfo_key(&sfo, key)
            .with_context(|| format!("ShadPKG output lacks readable {key} metadata"))?;
        ensure!(
            found.eq_ignore_ascii_case(value),
            "ShadPKG output {key} mismatch: expected {value}, found {found}"
        );
    }
    let payload = scan_payload(root, root, 0)?;
    if expected.category == "ac" && expected.no_data_addon {
        return Ok(payload);
    }
    ensure!(
        payload.0 > 0 && payload.1 > 0,
        "ShadPKG produced metadata only, without actual game payload; the update/game is not decrypted or ready for FPKG creation"
    );
    if matches!(expected.category.as_str(), "gd" | "gdc" | "gda") {
        ensure!(
            fs::metadata(root.join("eboot.bin")).is_ok_and(|m| m.len() > 0),
            "ShadPKG base-game output has no non-empty eboot.bin"
        );
    }
    Ok(payload)
}

/// Extract into a fresh sibling stage, verify identity/payload, then publish.
/// The source package and any existing prepared folder are never overwritten.
pub fn extract_package(
    source: &Path,
    destination: &Path,
    cancelled: Arc<AtomicBool>,
    progress: impl Fn(ProgressUpdate),
) -> Result<ExtractionReport> {
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        "ShadPKG extraction cancelled"
    );
    // Avoid Windows verbatim paths: OrbisPkgTool uses ':' for entry selectors.
    ensure!(source.is_file(), "PS4 source package is missing");
    let source = std::path::absolute(source)?;
    let identity = inspect_package(&source)?;
    super::ps4_extraction_keys::validate_package(&source, identity.no_data_addon)?;
    let tool = find_tool()?;
    ensure!(
        !destination.exists()
            || (destination.is_dir() && fs::read_dir(destination)?.next().is_none()),
        "refusing to overwrite existing extraction {}",
        destination.display()
    );
    let parent = destination
        .parent()
        .context("extraction destination has no parent")?;
    fs::create_dir_all(parent)?;
    let stage = loop {
        let path = parent.join(format!(
            ".shadpkg-{}-{}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&path) {
            Ok(()) => break Stage(std::path::absolute(path)?),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let output_root = stage.0.join("output");
    let mut child = Command::new(&tool)
        .arg("extract")
        .arg(&source)
        .arg(&output_root)
        .current_dir(&stage.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not launch managed ShadPKG")?;
    let (send, receive) = mpsc::channel();
    for mut stream in [
        Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>,
        Box::new(child.stderr.take().unwrap()),
    ] {
        let send = send.clone();
        thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(size) => {
                        if send
                            .send(String::from_utf8_lossy(&buffer[..size]).into_owned())
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }
    drop(send);
    let started = Instant::now();
    let mut last = Instant::now() - Duration::from_secs(1);
    let mut diagnostics = String::new();
    let status = loop {
        for chunk in receive.try_iter() {
            diagnostics.push_str(&chunk);
            if diagnostics.len() > 32768 {
                let mut cut = diagnostics.len() - 24576;
                while !diagnostics.is_char_boundary(cut) {
                    cut += 1
                }
                diagnostics.drain(..cut);
            }
        }
        if last.elapsed() >= Duration::from_millis(250) {
            let detail = diagnostics
                .split(['\r', '\n'])
                .rfind(|line| !line.trim().is_empty())
                .unwrap_or("Reading and decrypting package")
                .to_owned();
            progress(ProgressUpdate {
                stage: format!("ShadPKG {VERSION} extraction"),
                percent: 0,
                details: Some(format!("{}s: {}", started.elapsed().as_secs(), detail)),
                bytes_processed: 0,
                total_bytes: 0,
            });
            last = Instant::now();
        }
        if cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("ShadPKG extraction cancelled; partial output removed");
        }
        if started.elapsed() > Duration::from_secs(30 * 60) {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "ShadPKG extraction exceeded 30 minutes; partial output removed. Diagnostics:\n{diagnostics}"
            );
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("could not monitor ShadPKG extraction");
            }
        }
        thread::sleep(Duration::from_millis(40));
    };
    for chunk in receive {
        diagnostics.push_str(&chunk);
    }
    // Preserve backend diagnostics before the private staging directory is removed.
    for path in [
        stage.0.join("pkg_extraction.log"),
        stage.0.join("user/log/pkg_extraction.log"),
    ] {
        let log = read_log_tail(&path);
        if !log.is_empty() {
            diagnostics.push_str("\nShadPKG internal log:\n");
            diagnostics.push_str(&log);
        }
    }
    let failure = match status.code().map(|code| code as u32) {
        Some(0xC0000005) => {
            "Windows access violation (0xC0000005): the ShadPKG process crashed. No decrypted payload has been accepted"
        }
        Some(0xC0000374) => {
            "Windows heap corruption (0xC0000374): the ShadPKG process crashed. No decrypted payload has been accepted"
        }
        _ => "the extraction process did not complete successfully",
    };
    ensure!(
        status.success(),
        "ShadPKG {VERSION} extraction failed (exit {:?}) for {}: {}. Backend diagnostics:\n{}",
        status.code(),
        identity.content_id,
        failure,
        diagnostics
    );
    let root = if output_root.join(&identity.content_id).is_dir() {
        output_root.join(&identity.content_id)
    } else {
        output_root
    };
    let (payload_files,payload_bytes)=validate_output(&root,&identity).with_context(||format!("ShadPKG reported completion but output validation failed. Backend diagnostics:\n{diagnostics}"))?;
    if destination.exists() {
        fs::remove_dir(destination)?;
    }
    fs::rename(&root, destination).context("could not publish validated ShadPKG output")?;
    progress(ProgressUpdate {
        stage: "ShadPKG extraction validated".into(),
        percent: 100,
        details: Some(format!(
            "{payload_files} payload files; {payload_bytes} bytes"
        )),
        bytes_processed: payload_bytes,
        total_bytes: payload_bytes,
    });
    Ok(ExtractionReport {
        output_path: destination.to_path_buf(),
        identity,
        payload_files,
        payload_bytes,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(category: &str) -> PackageIdentity {
        PackageIdentity {
            title_id: "CUSA05986".into(),
            content_id: "EP0006-CUSA05986_00-NFS1800000000001".into(),
            version: "01.10".into(),
            category: category.into(),
            no_data_addon: false,
        }
    }
    fn sfo(root: &Path, id: &PackageIdentity) {
        let pairs = [
            ("TITLE_ID", id.title_id.as_str()),
            ("CONTENT_ID", id.content_id.as_str()),
            ("APP_VER", id.version.as_str()),
            ("CATEGORY", id.category.as_str()),
        ];
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let mut entries = Vec::new();
        for (key, value) in pairs {
            let offset = keys.len() as u16;
            keys.extend(key.bytes());
            keys.push(0);
            entries.extend(offset.to_le_bytes());
            entries.extend(0x0204u16.to_le_bytes());
            entries.extend((value.len() as u32 + 1).to_le_bytes());
            entries.extend((value.len() as u32 + 1).to_le_bytes());
            entries.extend((values.len() as u32).to_le_bytes());
            values.extend(value.bytes());
            values.push(0);
        }
        let mut bytes = b"\0PSF".to_vec();
        bytes.extend(0x101u32.to_le_bytes());
        bytes.extend((20u32 + entries.len() as u32).to_le_bytes());
        bytes.extend((20u32 + entries.len() as u32 + keys.len() as u32).to_le_bytes());
        bytes.extend(4u32.to_le_bytes());
        bytes.extend(entries);
        bytes.extend(keys);
        bytes.extend(values);
        fs::create_dir_all(root.join("sce_sys")).unwrap();
        fs::write(root.join("sce_sys/param.sfo"), bytes).unwrap();
    }
    #[test]
    fn managed_release_matches_official_checksum_and_cli() {
        let tool = find_tool().expect("bundled ShadPKG must be available");
        let result = Command::new(tool)
            .arg("--help")
            .current_dir(std::env::temp_dir())
            .output()
            .unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("sfo-info"));
    }
    #[test]
    fn metadata_only_patch_is_rejected() {
        let work = super::super::TempWorkspace::new("shad_metadata").unwrap();
        let id = identity("gp");
        sfo(&work.root, &id);
        assert!(
            validate_output(&work.root, &id)
                .unwrap_err()
                .to_string()
                .contains("metadata only")
        );
        fs::write(work.root.join("entry_0x123.bin"), b"raw package entry").unwrap();
        assert!(validate_output(&work.root, &id).is_err());
    }
    #[test]
    fn base_update_and_dlc_payload_validation() {
        for category in ["gd", "gp", "ac"] {
            let work = super::super::TempWorkspace::new("shad_payload").unwrap();
            let id = identity(category);
            sfo(&work.root, &id);
            fs::write(work.root.join("eboot.bin"), b"synthetic payload").unwrap();
            assert_eq!(validate_output(&work.root, &id).unwrap(), (1, 17));
        }
    }
    #[test]
    fn exact_title_content_category_and_version_are_required() {
        let work = super::super::TempWorkspace::new("shad_identity").unwrap();
        let id = identity("gp");
        sfo(&work.root, &id);
        fs::write(work.root.join("data.bin"), b"payload").unwrap();
        for index in 0..4 {
            let mut wrong = id.clone();
            match index {
                0 => wrong.version = "01.05".into(),
                1 => wrong.category = "ac".into(),
                2 => wrong.content_id = "EP0006-CUSA05986_00-OTHER00000000000".into(),
                _ => wrong.title_id = "CUSA05999".into(),
            };
            assert!(validate_output(&work.root, &wrong).is_err());
        }
    }
    #[test]
    fn no_data_addon_is_distinguished_from_failed_patch() {
        let work = super::super::TempWorkspace::new("shad_nodata").unwrap();
        let mut id = identity("ac");
        sfo(&work.root, &id);
        assert!(validate_output(&work.root, &id).is_err());
        id.no_data_addon = true;
        assert_eq!(validate_output(&work.root, &id).unwrap(), (0, 0));
    }
    #[test]
    fn unsafe_content_identity_is_rejected() {
        let mut id = identity("gp");
        id.content_id = "../../outside".into();
        assert!(id.validate().is_err());
    }
    #[test]
    fn cancelled_job_never_creates_output() {
        let work = super::super::TempWorkspace::new("shad_cancel").unwrap();
        let target = work.root.join("output");
        let error = extract_package(
            &work.root.join("absent.pkg"),
            &target,
            Arc::new(AtomicBool::new(true)),
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!target.exists());
    }
}
