use anyhow::{Context, Error};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::app_settings;

pub struct OperationLog {
    path: PathBuf,
    share_path: PathBuf,
    file: File,
}

impl OperationLog {
    pub fn create(workflow: &str) -> Result<Self, Error> {
        let directory = app_settings::logs_dir()?;
        fs::create_dir_all(&directory).context("could not create the log folder")?;
        let path = directory.join(format!("iso2god-{}.log", filename_stamp()));
        let share_path = path.with_extension("share.txt");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("could not create the operation log")?;
        writeln!(
            file,
            "DIGITAL FORGE — ISO2GOD {}\nStarted: {}\nWorkflow: {}\nOperating system: {} {}\nLog: {}\n",
            env!("CARGO_PKG_VERSION"),
            timestamp_stamp(),
            redact(workflow),
            std::env::consts::OS,
            std::env::consts::ARCH,
            path.display()
        )
        .context("could not write the operation log")?;
        Ok(Self {
            path,
            share_path,
            file,
        })
    }

    pub fn info(&mut self, message: &str) -> Result<(), Error> {
        self.write_line("INFO", message)
    }

    pub fn stage(&mut self, stage: &str, detail: &str) -> Result<(), Error> {
        self.write_line("STAGE", &format!("{stage}: {detail}"))
    }

    pub fn error(&mut self, error: &Error) -> Result<(), Error> {
        self.write_line("ERROR", &format!("{error:#}"))?;
        self.write_line("INFO", &format!("Log file: {}", self.path.display()))?;
        let complete =
            fs::read_to_string(&self.path).context("could not read the operation log")?;
        fs::write(
            &self.share_path,
            format!(
                "DIGITAL FORGE — SHAREABLE FAILURE REPORT\n\
                 Review this report before sharing it. It contains redacted diagnostic text, not game data.\n\n{}",
                redact(&complete)
            ),
        )
        .context("could not create the shareable diagnostic report")
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn share_path(&self) -> &std::path::Path {
        &self.share_path
    }

    fn write_line(&mut self, level: &str, message: &str) -> Result<(), Error> {
        writeln!(
            self.file,
            "{} [{level}] {}",
            timestamp_stamp(),
            redact(message)
        )
        .context("could not write the operation log")?;
        self.file.flush().ok();
        Ok(())
    }
}

fn timestamp_stamp() -> String {
    let datetime = unix_to_utc(unix_secs());
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        datetime.0, datetime.1, datetime.2, datetime.3, datetime.4, datetime.5
    )
}

fn filename_stamp() -> String {
    let datetime = unix_to_utc(unix_secs());
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}Z",
        datetime.0, datetime.1, datetime.2, datetime.3, datetime.4, datetime.5
    )
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_to_utc(mut secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let second = secs % 60;
    secs /= 60;
    let minute = secs % 60;
    secs /= 60;
    let hour = secs % 24;
    let mut days = secs / 24;
    let mut year = 1970;
    loop {
        let length = if is_leap(year) { 366 } else { 365 };
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let month_lengths = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 1;
    for length in month_lengths {
        if days < length {
            break;
        }
        days -= length;
        month += 1;
    }
    (year, month, days + 1, hour, minute, second)
}

fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn redact(text: &str) -> String {
    let text = redact_user_paths(text);
    let mut output = String::with_capacity(text.len());
    for line in text.lines() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&redact_line(line));
    }
    if text.ends_with('\n') {
        output.push('\n');
    }
    output
}

fn redact_user_paths(text: &str) -> String {
    let mut redacted = text.to_owned();
    for (name, replacement) in [
        ("USERPROFILE", "<user-home>"),
        ("APPDATA", "<app-data>"),
        ("LOCALAPPDATA", "<local-app-data>"),
        ("TEMP", "<temp>"),
        ("TMP", "<temp>"),
    ] {
        if let Some(path) = std::env::var_os(name) {
            let path = path.to_string_lossy();
            if !path.is_empty() {
                redacted = redacted.replace(path.as_ref(), replacement);
                if let Some(windows_path) = path.strip_prefix("\\\\?\\") {
                    redacted = redacted.replace(windows_path, replacement);
                }
            }
        }
    }
    redacted
}

fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if contains_secret_keyword(&lower) {
        return "[redacted]".to_owned();
    }
    line.to_owned()
}

fn contains_secret_keyword(lower: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "password",
        "passwd",
        "authorization:",
        "cookie:",
        "set-cookie:",
        "credential",
        "secret-tool",
        "website username",
        "browser session",
    ];
    KEYWORDS.iter().any(|keyword| lower.contains(keyword))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_passwords_and_cookies_from_log_text() {
        assert_eq!(redact("password=xbox"), "[redacted]");
        assert_eq!(redact("Cookie: session=abc"), "[redacted]");
        assert_eq!(redact("Authorization: Basic abc"), "[redacted]");
        assert_eq!(
            redact("Archive part 6 of 8 is missing"),
            "Archive part 6 of 8 is missing"
        );
    }

    #[test]
    fn log_filenames_are_valid_on_windows() {
        let stamp = filename_stamp();
        assert!(
            !stamp
                .chars()
                .any(|character| r#"<>:"/\|?*"#.contains(character))
        );
        assert!(!stamp.contains(' '));
        assert!(stamp.ends_with('Z'));
    }
}
