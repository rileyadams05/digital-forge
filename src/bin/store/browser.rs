use anyhow::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const INCOMPLETE_SUFFIXES: &[&str] = &[
    ".crdownload",
    ".part",
    ".tmp",
    ".download",
    ".partial",
    ".!ut",
    ".opdownload",
];

const ARCHIVE_EXTENSIONS: &[&str] = &["iso", "zip", "rar", "7z", "001"];

pub fn default_downloads_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Downloads"));
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))
    }
}

pub fn is_incomplete_download(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    INCOMPLETE_SUFFIXES
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

pub fn looks_like_archive(path: &Path) -> bool {
    if is_incomplete_download(path) {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    ARCHIVE_EXTENSIONS.iter().any(|extension| {
        name.ends_with(&format!(".{extension}"))
            || name.contains(&format!(".{extension}."))
            || name.contains(".part")
    })
}

pub fn find_completed_download(
    folder: &Path,
    started_at: SystemTime,
    title_hint: &str,
) -> Result<Option<PathBuf>, Error> {
    if !folder.is_dir() {
        return Ok(None);
    }
    let hint = normalised_tokens(title_hint);
    let mut newest: Option<(SystemTime, u64, PathBuf)> = None;
    for entry in fs::read_dir(folder)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() || !looks_like_archive(&path) {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if modified < started_at {
            continue;
        }
        if metadata.len() < 64 * 1024 {
            continue;
        }
        if !hint.is_empty() {
            let name_tokens = normalised_tokens(
                &path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default(),
            );
            if !hint
                .iter()
                .any(|token| name_tokens.iter().any(|name| name.contains(token)))
            {
                continue;
            }
        }
        let candidate = (modified, metadata.len(), path);
        if newest
            .as_ref()
            .is_none_or(|(time, _, _)| candidate.0 >= *time)
        {
            newest = Some(candidate);
        }
    }
    Ok(newest.map(|(_, _, path)| path))
}

pub fn file_size_stable(
    path: &Path,
    previous: Option<u64>,
    wait: Duration,
) -> Result<Option<u64>, Error> {
    let size = fs::metadata(path)?.len();
    if previous == Some(size) {
        std::thread::sleep(wait);
        let again = fs::metadata(path)?.len();
        if again == size {
            return Ok(Some(size));
        }
    }
    Ok(None)
}

fn normalised_tokens(value: &str) -> Vec<String> {
    value
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| {
            token.len() > 2 && !matches!(*token, "the" | "and" | "for" | "iso" | "xbox" | "game")
        })
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_browser_temp_files() {
        assert!(is_incomplete_download(Path::new(
            "game.part01.rar.crdownload"
        )));
        assert!(is_incomplete_download(Path::new("game.zip.part")));
        assert!(is_incomplete_download(Path::new("game.7z.tmp")));
        assert!(!is_incomplete_download(Path::new("game.part01.rar")));
        assert!(looks_like_archive(Path::new("game.part01.rar")));
        assert!(!looks_like_archive(Path::new("game.part01.rar.crdownload")));
    }
}
