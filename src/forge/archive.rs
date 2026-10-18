use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveKind {
    SingleVolume(PathBuf),
    MultiVolume {
        first_volume: PathBuf,
        all_volumes: Vec<PathBuf>,
        missing_volumes: Vec<String>,
    },
    ConsoleSplitImage {
        first_chunk: PathBuf,
        all_chunks: Vec<PathBuf>,
    },
    MultiDiscSet {
        discs: Vec<PathBuf>,
    },
    NotArchive(PathBuf),
}

pub struct TempWorkspace {
    pub root: PathBuf,
    preserve_on_drop: bool,
}

impl TempWorkspace {
    pub fn new(prefix: &str) -> Result<Self, Error> {
        let temp_base = env::temp_dir().join("digital_forge");
        fs::create_dir_all(&temp_base)?;
        let id = format!(
            "{prefix}_{}_{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        );
        let root = temp_base.join(id);
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            preserve_on_drop: false,
        })
    }

    pub fn preserve(&mut self) {
        self.preserve_on_drop = true;
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        if !self.preserve_on_drop && self.root.exists() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

pub struct ArchiveEngine;

pub type ArchiveProgress = Arc<dyn Fn(u8, &str) + Send + Sync>;

impl ArchiveEngine {
    pub fn classify(source: &Path) -> ArchiveKind {
        if source.is_dir() {
            return ArchiveKind::NotArchive(source.to_path_buf());
        }

        let name = source
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();

        // 1. Console split image check (.iso.0, .pkg.0, ul.00)
        if name.contains(".iso.") || name.contains(".pkg.") || name.starts_with("ul.") {
            let parent = source.parent().unwrap_or_else(|| Path::new("."));
            let mut chunks = Vec::new();
            if let Ok(entries) = fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let p_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if (name.contains(".iso.") && p_name.contains(".iso."))
                        || (name.contains(".pkg.") && p_name.contains(".pkg."))
                        || (name.starts_with("ul.") && p_name.starts_with("ul."))
                    {
                        chunks.push(path);
                    }
                }
            }
            chunks.sort();
            return ArchiveKind::ConsoleSplitImage {
                first_chunk: chunks
                    .first()
                    .cloned()
                    .unwrap_or_else(|| source.to_path_buf()),
                all_chunks: chunks,
            };
        }

        // 2. Multi-volume archive check (.7z.001, .part1.rar, PT1.rar, .zip.001)
        if name.ends_with(".001") {
            let prefix = &name[..name.len() - 4];
            let parent = source.parent().unwrap_or_else(|| Path::new("."));
            let mut all_vols = Vec::new();
            let mut missing = Vec::new();
            let mut index = 1;
            loop {
                let vol_name = format!("{prefix}.{:03}", index);
                let vol_path = parent.join(&vol_name);
                if vol_path.is_file() {
                    all_vols.push(vol_path);
                    index += 1;
                } else {
                    let next_path = parent.join(format!("{prefix}.{:03}", index + 1));
                    if next_path.is_file() {
                        missing.push(vol_name);
                        index += 1;
                    } else {
                        break;
                    }
                }
            }
            return ArchiveKind::MultiVolume {
                first_volume: all_vols
                    .first()
                    .cloned()
                    .unwrap_or_else(|| source.to_path_buf()),
                all_volumes: all_vols,
                missing_volumes: missing,
            };
        }

        let lower_name = name.to_ascii_lowercase();
        let part_info = if let Some(part_idx) = lower_name.find(".part") {
            Some((part_idx + 5, &name[..part_idx + 5]))
        } else if let Some(part_idx) = lower_name.find("_part") {
            Some((part_idx + 5, &name[..part_idx + 5]))
        } else if let Some(part_idx) = lower_name.find("-part") {
            Some((part_idx + 5, &name[..part_idx + 5]))
        } else if lower_name.starts_with("part") {
            Some((4, &name[..4]))
        } else if lower_name.starts_with("pt") {
            // Compact multipart naming accepted by Digital Forge: PT1.rar,
            // PT2.rar, PT3.rar, ... (case-insensitive).
            Some((2, &name[..2]))
        } else {
            None
        };
        if let Some((digits_start, prefix)) = part_info {
            let suffix = &name[digits_start..];
            if let Some(dot_rar) = suffix.to_ascii_lowercase().find(".rar") {
                let digits = &suffix[..dot_rar];
                if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                    let width = digits.len();
                    let parent = source.parent().unwrap_or_else(|| Path::new("."));
                    let mut all_vols = Vec::new();
                    let mut missing = Vec::new();
                    let mut index = 1;
                    loop {
                        let vol_name = format!("{prefix}{:0width$}.rar", index, width = width);
                        let candidate_normal = parent.join(&vol_name);
                        let candidate_dot = parent.join(format!(".{vol_name}"));
                        let found = if candidate_normal.is_file() {
                            Some(candidate_normal)
                        } else if candidate_dot.is_file() {
                            Some(candidate_dot)
                        } else {
                            None
                        };
                        if let Some(vol_path) = found {
                            all_vols.push(vol_path);
                            index += 1;
                        } else {
                            let next_name =
                                format!("{prefix}{:0width$}.rar", index + 1, width = width);
                            if parent.join(&next_name).is_file()
                                || parent.join(format!(".{next_name}")).is_file()
                            {
                                missing.push(vol_name);
                                index += 1;
                            } else {
                                break;
                            }
                        }
                    }
                    return ArchiveKind::MultiVolume {
                        first_volume: all_vols
                            .first()
                            .cloned()
                            .unwrap_or_else(|| source.to_path_buf()),
                        all_volumes: all_vols,
                        missing_volumes: missing,
                    };
                }
            }
        }

        // 3. Single archive check (.zip, .7z, .rar, .tar, .gz)
        if let Some(ext) = source.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("zip")
                || ext.eq_ignore_ascii_case("7z")
                || ext.eq_ignore_ascii_case("rar")
                || ext.eq_ignore_ascii_case("tar")
                || ext.eq_ignore_ascii_case("gz")
            {
                return ArchiveKind::SingleVolume(source.to_path_buf());
            }
        }

        ArchiveKind::NotArchive(source.to_path_buf())
    }

    pub fn list_archive_entries(archive_path: &Path) -> Result<Vec<String>, Error> {
        let tool = find_7zip_executable().context(
            "7-Zip or compatible archive extractor was not found. Please install 7-Zip.",
        )?;
        let output = Command::new(&tool)
            .arg("l")
            .arg("-ba")
            .arg("-slt")
            .arg("-p-")
            .arg(archive_path)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to inspect archive {}", archive_path.display()))?;
        if !output.status.success() {
            bail!("7-Zip listing failed for {}", archive_path.display());
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut entries = Vec::new();
        for line in text.lines() {
            if let Some(path) = line.strip_prefix("Path = ") {
                let p = path.trim();
                if !p.is_empty() {
                    entries.push(p.to_string());
                }
            }
        }
        Ok(entries)
    }

    /// Inspect encryption metadata without extracting content or waiting on
    /// 7-Zip's own interactive password reader. The UI owns password entry.
    pub fn requires_password(archive_path: &Path) -> Result<bool, Error> {
        let tool = find_7zip_executable().context("7-Zip was not found")?;
        let entry = match Self::classify(archive_path) {
            ArchiveKind::MultiVolume {
                first_volume,
                missing_volumes,
                ..
            } => {
                if !missing_volumes.is_empty() {
                    bail!("Missing archive parts: {}", missing_volumes.join(", "));
                }
                first_volume
            }
            _ => archive_path.to_path_buf(),
        };
        let output = Command::new(tool)
            .args(["l", "-slt", "-p-"])
            .arg(&entry)
            .stdin(Stdio::null())
            .output()
            .context("could not inspect archive encryption")?;
        let listing = String::from_utf8_lossy(&output.stdout);
        let errors = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        if listing.lines().any(|line| line.trim() == "Encrypted = +")
            || errors.contains("wrong password")
            || errors.contains("enter password")
            || errors.contains("password is incorrect")
        {
            return Ok(true);
        }
        if !output.status.success() {
            bail!(
                "Archive inspection failed for {}: {}",
                entry.display(),
                concise_7zip_error(&errors)
            );
        }
        Ok(false)
    }

    pub fn extract_archive(
        archive_path: &Path,
        dest_dir: &Path,
        password: Option<&str>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<PathBuf, Error> {
        Self::extract_archive_with_progress(archive_path, dest_dir, password, cancelled, None)
    }

    pub fn extract_archive_with_progress(
        archive_path: &Path,
        dest_dir: &Path,
        password: Option<&str>,
        cancelled: Arc<AtomicBool>,
        progress: Option<ArchiveProgress>,
    ) -> Result<PathBuf, Error> {
        fs::create_dir_all(dest_dir)?;
        // Read the original volume set in place. Never stage copies of the
        // compressed input: only the extracted payload needs output space.
        let mut entry = archive_path.to_path_buf();
        if let ArchiveKind::MultiVolume {
            first_volume,
            missing_volumes,
            ..
        } = Self::classify(archive_path)
        {
            if !missing_volumes.is_empty() {
                bail!("Missing archive parts: {}", missing_volumes.join(", "));
            }
            entry = first_volume;
        }
        let tool = find_7zip_executable().context(
            "7-Zip or compatible archive extractor was not found. Please install 7-Zip.",
        )?;

        let mut cmd = Command::new(&tool);
        cmd.arg("x")
            .arg("-y")
            .arg("-bso0")
            .arg("-bsp1")
            .arg("-bse2")
            .arg(format!("-o{}", dest_dir.display()))
            .arg(&entry)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(pw) = password {
            if !pw.is_empty() {
                cmd.arg(format!("-p{pw}"));
            }
        } else {
            cmd.arg("-p-");
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to launch 7-Zip from {}", tool.display()))?;

        let stdout = child
            .stdout
            .take()
            .context("could not read 7-Zip progress")?;
        let stderr = child.stderr.take().context("could not read 7-Zip errors")?;
        let (progress_tx, progress_rx) = mpsc::channel();
        let progress_reader = thread::spawn(move || {
            let mut reader = stdout;
            let mut buffer = [0u8; 1024];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let _ = progress_tx.send(String::from_utf8_lossy(&buffer[..count]).into_owned());
            }
        });
        let error_reader = thread::spawn(move || {
            let mut reader = stderr;
            let mut details = String::new();
            let _ = reader.read_to_string(&mut details);
            details
        });

        let display_name = archive_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("archive")
            .to_owned();
        let mut progress_text = String::new();
        let mut last_percent = u8::MAX;
        let mut last_item = display_name.clone();
        if let Some(progress) = progress.as_ref() {
            progress(0, &last_item);
        } else {
            render_extraction_progress(0, &last_item)?;
        }

        let status = loop {
            while let Ok(chunk) = progress_rx.try_recv() {
                progress_text.push_str(&chunk);
                if progress_text.len() > 8192 {
                    let keep_from = progress_text.len() - 4096;
                    progress_text.drain(..keep_from);
                }
                if let Some((percent, item)) = parse_7zip_progress(&progress_text) {
                    if let Some(item) = item {
                        last_item = item;
                    }
                    if percent != last_percent {
                        if let Some(progress) = progress.as_ref() {
                            progress(percent, &last_item);
                        } else {
                            render_extraction_progress(percent, &last_item)?;
                        }
                        last_percent = percent;
                    }
                }
            }

            if alt_c_pressed()? {
                cancelled.store(true, Ordering::Relaxed);
            }
            if cancelled.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = progress_reader.join();
                let _ = error_reader.join();
                clear_progress_line()?;
                remove_temporary_extraction(dest_dir)?;
                bail!("Extraction cancelled with Alt+C; temporary output was removed");
            }

            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(e) => bail!("error waiting for archive extraction: {e}"),
            }
        };

        let _ = progress_reader.join();
        let error_details = error_reader.join().unwrap_or_default();
        while let Ok(chunk) = progress_rx.try_recv() {
            progress_text.push_str(&chunk);
        }
        clear_progress_line()?;

        if !status.success() {
            remove_temporary_extraction(dest_dir)?;
            let details = concise_7zip_error(&error_details);
            bail!(
                "7-Zip extraction failed (exit code {:?}){}",
                status.code(),
                if details.is_empty() {
                    String::new()
                } else {
                    format!(": {details}")
                }
            );
        }

        if let Some(progress) = progress.as_ref() {
            progress(100, &last_item);
        } else {
            render_extraction_progress(100, &last_item)?;
        }
        if progress.is_none() {
            println!();
        }
        Self::inspect_extracted_root(dest_dir)
    }

    fn inspect_extracted_root(dest_dir: &Path) -> Result<PathBuf, Error> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(dest_dir)?.flatten() {
            entries.push(entry.path());
        }
        if entries.len() == 1 {
            Ok(entries.remove(0))
        } else {
            Ok(dest_dir.to_path_buf())
        }
    }
}

fn render_extraction_progress(percent: u8, item: &str) -> Result<(), Error> {
    const WIDTH: usize = 20;
    let percent = percent.min(100);
    let filled = (usize::from(percent) * WIDTH / 100).min(WIDTH);
    let empty = WIDTH.saturating_sub(filled);
    let item_clean = item.rsplit(['/', '\\']).next().unwrap_or(item).trim();
    let display_item = if item_clean.len() > 20 {
        format!("...{}", &item_clean[item_clean.len() - 17..])
    } else {
        item_clean.to_string()
    };
    print!(
        "\r\x1b[2KExtracting  \x1b[90m[\x1b[38;5;208m{}\x1b[0m{}\x1b[90m]\x1b[0m {:>3}%  {:<20}  \x1b[90mAlt+C Cancel\x1b[0m",
        "█".repeat(filled),
        " ".repeat(empty),
        percent,
        display_item
    );
    io::stdout()
        .flush()
        .context("could not draw extraction progress")
}

fn clear_progress_line() -> Result<(), Error> {
    print!("\r\x1b[2K");
    io::stdout()
        .flush()
        .context("could not clear extraction progress")
}

fn alt_c_pressed() -> Result<bool, Error> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    while event::poll(Duration::from_millis(0)).context("could not poll extraction controls")? {
        let Event::Key(key) = event::read().context("could not read extraction controls")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if let KeyCode::Char(character) = key.code
            && character.eq_ignore_ascii_case(&'c')
            && key.modifiers.contains(KeyModifiers::ALT)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn remove_temporary_extraction(dest_dir: &Path) -> Result<(), Error> {
    if dest_dir.exists() {
        fs::remove_dir_all(dest_dir).with_context(|| {
            format!(
                "could not remove temporary extraction folder {}",
                dest_dir.display()
            )
        })?;
    }
    Ok(())
}

fn parse_7zip_progress(text: &str) -> Option<(u8, Option<String>)> {
    let percent_position = text.rfind('%')?;
    let before = &text[..percent_position];
    let digit_start = before
        .rfind(|character: char| !character.is_ascii_digit())
        .map_or(0, |position| position + 1);
    let percent = before[digit_start..].parse::<u8>().ok()?.min(100);
    let after = &text[percent_position + 1..];
    let item = after
        .split(['\r', '\n'])
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_start_matches('-').trim().to_owned())
        .filter(|value| !value.is_empty());
    Some((percent, item))
}

fn concise_7zip_error(details: &str) -> String {
    details
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("error") || lower.contains("password") || lower.contains("cannot")
        })
        .take(3)
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn find_7zip_executable() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        for path in &[
            r"C:\Program Files\7-Zip\7z.exe",
            r"C:\Program Files (x86)\7-Zip\7z.exe",
            r"C:\ProgramData\chocolatey\bin\7z.exe",
        ] {
            let p = Path::new(path);
            if p.is_file() {
                return Some(p.to_path_buf());
            }
        }
        find_in_path("7z.exe").or_else(|| find_in_path("7z"))
    }
    #[cfg(not(windows))]
    {
        find_in_path("7z").or_else(|| find_in_path("7za"))
    }
}

fn find_in_path(executable: &str) -> Option<PathBuf> {
    if let Some(paths) = env::var_os("PATH") {
        for path in env::split_paths(&paths) {
            let candidate = path.join(executable);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::{
        ArchiveEngine, ArchiveKind, TempWorkspace, find_7zip_executable, parse_7zip_progress,
        remove_temporary_extraction,
    };

    #[test]
    fn parses_7zip_percentage_and_current_file() {
        let progress = "  8% 1 - release/game.pkg\r 42% 1 - release/game.pkg\r";
        let (percent, item) = parse_7zip_progress(progress).unwrap();
        assert_eq!(percent, 42);
        assert_eq!(item.as_deref(), Some("1 - release/game.pkg"));
    }

    #[test]
    fn multipart_discovery_is_not_limited_to_five_parts() {
        let workspace = TempWorkspace::new("archive_parts").unwrap();
        for part in 1..=12 {
            fs::write(
                workspace.root.join(format!("game.part{part}.rar")),
                format!("part {part}"),
            )
            .unwrap();
        }
        match ArchiveEngine::classify(&workspace.root.join("game.part1.rar")) {
            ArchiveKind::MultiVolume {
                all_volumes,
                missing_volumes,
                ..
            } => {
                assert_eq!(all_volumes.len(), 12);
                assert!(missing_volumes.is_empty());
            }
            other => panic!("expected multi-volume archive, got {other:?}"),
        }
    }

    #[test]
    fn compact_pt_rar_names_are_ordered_and_complete() {
        let workspace = TempWorkspace::new("compact_pt_archive_parts").unwrap();
        for part in 1..=4 {
            fs::write(
                workspace.root.join(format!("PT{part}.rar")),
                format!("part {part}"),
            )
            .unwrap();
        }

        match ArchiveEngine::classify(&workspace.root.join("PT1.rar")) {
            ArchiveKind::MultiVolume {
                all_volumes,
                missing_volumes,
                ..
            } => {
                assert_eq!(all_volumes.len(), 4);
                assert!(missing_volumes.is_empty());
                assert_eq!(
                    all_volumes
                        .iter()
                        .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
                        .collect::<Vec<_>>(),
                    vec!["PT1.rar", "PT2.rar", "PT3.rar", "PT4.rar"]
                );
            }
            other => panic!("expected compact PT multipart archive, got {other:?}"),
        }
    }

    #[test]
    fn cancellation_cleanup_removes_only_temporary_extraction() {
        let workspace = TempWorkspace::new("cancel_cleanup").unwrap();
        let extraction = workspace.root.join("extracted");
        fs::create_dir_all(&extraction).unwrap();
        fs::write(extraction.join("partial.pkg"), b"partial").unwrap();
        let source = workspace.root.join("source.part1.rar");
        fs::write(&source, b"source").unwrap();

        remove_temporary_extraction(&extraction).unwrap();

        assert!(!extraction.exists());
        assert!(source.is_file());
    }

    #[test]
    fn encryption_detection_uses_real_plain_encrypted_and_hidden_header_archives() {
        let tool = find_7zip_executable().expect("7-Zip required for encryption test");
        let workspace = TempWorkspace::new("encryption_detection").unwrap();
        let payload = workspace.root.join("payload.txt");
        fs::write(&payload, b"archive encryption test").unwrap();
        for (name, options, expected) in [
            ("plain.7z", vec![], false),
            ("encrypted.7z", vec!["-ptest-password"], true),
            ("hidden.7z", vec!["-ptest-password", "-mhe=on"], true),
            ("encrypted.zip", vec!["-ptest-password"], true),
        ] {
            let archive = workspace.root.join(name);
            assert!(
                Command::new(&tool)
                    .arg("a")
                    .args(options)
                    .arg(&archive)
                    .arg(&payload)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            assert_eq!(
                ArchiveEngine::requires_password(&archive).unwrap(),
                expected,
                "{name}"
            );
        }
        let corrupt = workspace.root.join("corrupt.7z");
        fs::write(&corrupt, b"not an archive").unwrap();
        assert!(ArchiveEngine::requires_password(&corrupt).is_err());
    }

    #[test]
    fn encrypted_multipart_extracts_every_volume_and_preserves_bytes() {
        let seven_zip =
            find_7zip_executable().expect("7-Zip required for multipart integration test");
        let workspace = TempWorkspace::new("encrypted_multipart").unwrap();
        let source = workspace.root.join("payload.bin");
        let bytes: Vec<u8> = (0..32768).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &bytes).unwrap();
        let archive = workspace.root.join("payload.7z");
        assert!(
            Command::new(seven_zip)
                .args(["a", "-t7z", "-mx=0", "-v4096b", "-pfixture-secret"])
                .arg(&archive)
                .arg(&source)
                .status()
                .unwrap()
                .success()
        );
        let first = workspace.root.join("payload.7z.001");
        assert!(ArchiveEngine::requires_password(&first).unwrap());
        let ArchiveKind::MultiVolume {
            all_volumes,
            missing_volumes,
            ..
        } = ArchiveEngine::classify(&first)
        else {
            panic!("multipart archive not detected")
        };
        assert!(all_volumes.len() > 4);
        assert!(missing_volumes.is_empty());
        let destination = workspace.root.join("extracted");
        let error = ArchiveEngine::extract_archive(
            &first,
            &destination,
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}")
                .to_ascii_lowercase()
                .contains("password")
        );
        let output = ArchiveEngine::extract_archive(
            &first,
            &destination,
            Some("fixture-secret"),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(fs::read(output).unwrap(), bytes);
        assert!(all_volumes.iter().all(|path| path.is_file()));
    }

    #[test]
    fn password_archive_extracts_through_the_progress_runner() {
        let Some(seven_zip) = find_7zip_executable() else {
            return;
        };
        let workspace = TempWorkspace::new("password_extract").unwrap();
        let payload = workspace.root.join("payload.pkg");
        fs::write(&payload, b"valid package payload").unwrap();
        let archive = workspace.root.join("payload.7z");
        let status = Command::new(seven_zip)
            .current_dir(&workspace.root)
            .args(["a", "-y", "-phako"])
            .arg(&archive)
            .arg("payload.pkg")
            .status()
            .unwrap();
        assert!(status.success());
        fs::remove_file(&payload).unwrap();

        let extracted = ArchiveEngine::extract_archive(
            &archive,
            &workspace.root.join("extracted"),
            Some("hako"),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert!(extracted.is_file());
        assert_eq!(fs::read(extracted).unwrap(), b"valid package payload");
    }

    #[test]
    fn cancelled_progress_runner_removes_partial_output_and_keeps_archive() {
        let Some(seven_zip) = find_7zip_executable() else {
            return;
        };
        let workspace = TempWorkspace::new("cancelled_runner").unwrap();
        fs::write(workspace.root.join("payload.pkg"), vec![7u8; 1024 * 1024]).unwrap();
        let archive = workspace.root.join("payload.7z");
        let status = Command::new(seven_zip)
            .current_dir(&workspace.root)
            .args(["a", "-y"])
            .arg(&archive)
            .arg("payload.pkg")
            .status()
            .unwrap();
        assert!(status.success());
        fs::remove_file(workspace.root.join("payload.pkg")).unwrap();
        let extraction = workspace.root.join("extracted");

        let error = ArchiveEngine::extract_archive(
            &archive,
            &extraction,
            None,
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("temporary output was removed"));
        assert!(!extraction.exists());
        assert!(archive.is_file());
    }
}
