use anyhow::{Context, Error};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::app_settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Downloading,
    ArchiveCheck,
    Extracting,
    IsoDetected,
    Converting,
    Verification,
    Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Waiting,
    InProgress,
    Complete,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pipeline {
    pub downloading: StageStatus,
    pub archive_check: StageStatus,
    pub extracting: StageStatus,
    pub iso_detected: StageStatus,
    pub converting: StageStatus,
    pub verification: StageStatus,
    pub delivery: StageStatus,
    pub converting_percent: u8,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            downloading: StageStatus::Waiting,
            archive_check: StageStatus::Waiting,
            extracting: StageStatus::Waiting,
            iso_detected: StageStatus::Waiting,
            converting: StageStatus::Waiting,
            verification: StageStatus::Waiting,
            delivery: StageStatus::Waiting,
            converting_percent: 0,
        }
    }
}

impl Pipeline {
    pub fn skip_download(&mut self) {
        self.downloading = StageStatus::Skipped;
    }

    pub fn set(&mut self, kind: StageKind, status: StageStatus) {
        match kind {
            StageKind::Downloading => self.downloading = status,
            StageKind::ArchiveCheck => self.archive_check = status,
            StageKind::Extracting => self.extracting = status,
            StageKind::IsoDetected => self.iso_detected = status,
            StageKind::Converting => self.converting = status,
            StageKind::Verification => self.verification = status,
            StageKind::Delivery => self.delivery = status,
        }
    }

    pub fn render(&self, delivery_label: &str) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}",
            format_row("Downloading", &self.downloading, None),
            format_row("Archive check", &self.archive_check, None),
            format_row("Extracting", &self.extracting, None),
            format_row("Game format", &self.iso_detected, None),
            format_row(
                "Converting",
                &self.converting,
                (self.converting == StageStatus::InProgress).then_some(self.converting_percent),
            ),
            format_row("Verification", &self.verification, None),
            format_row(delivery_label, &self.delivery, None),
        )
    }

    pub fn current_progress<'a>(&self, delivery_label: &'a str) -> (&'a str, u8) {
        if self.delivery == StageStatus::Complete {
            return ("Complete", 100);
        }
        if self.delivery == StageStatus::InProgress {
            return (delivery_label, 80);
        }
        if self.verification == StageStatus::InProgress {
            return ("Verifying prepared game", 72);
        }
        if self.converting == StageStatus::InProgress {
            return (
                "Converting game",
                52_u8.saturating_add(self.converting_percent.saturating_mul(20) / 100),
            );
        }
        if self.iso_detected == StageStatus::InProgress {
            return ("Detecting game format", 48);
        }
        if self.extracting == StageStatus::InProgress {
            return ("Extracting game", 18);
        }
        if self.archive_check == StageStatus::InProgress {
            return ("Checking downloaded files", 10);
        }
        if self.downloading == StageStatus::InProgress {
            return ("Downloading game", 0);
        }
        ("Preparing game", 0)
    }
}

fn format_row(label: &str, status: &StageStatus, percent: Option<u8>) -> String {
    let value = match (status, percent) {
        (StageStatus::Complete, _) => "Complete".to_owned(),
        (StageStatus::Waiting, _) => "Waiting".to_owned(),
        (StageStatus::Failed, _) => "Failed".to_owned(),
        (StageStatus::Skipped, _) => "Skipped".to_owned(),
        (StageStatus::InProgress, Some(percent)) => format!("{percent}%"),
        (StageStatus::InProgress, None) => "In progress".to_owned(),
    };
    format!("{label:<18}{value}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobWorkflow {
    Store,
    ManualImport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobDelivery {
    Usb,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobState {
    pub workflow: JobWorkflow,
    pub delivery: JobDelivery,
    pub game_name: String,
    pub work_dir: PathBuf,
    pub source_path: Option<PathBuf>,
    pub prepared_root: Option<PathBuf>,
    pub pipeline: Pipeline,
}

impl JobState {
    pub fn new(
        workflow: JobWorkflow,
        delivery: JobDelivery,
        game_name: String,
        work_dir: PathBuf,
    ) -> Self {
        Self {
            workflow,
            delivery,
            game_name,
            work_dir,
            source_path: None,
            prepared_root: None,
            pipeline: Pipeline::default(),
        }
    }

    pub fn delivery_label(&self) -> &'static str {
        match self.delivery {
            JobDelivery::Usb => "USB export",
            JobDelivery::Local => "Save to folder",
        }
    }

    pub fn conversion_complete(&self) -> bool {
        self.pipeline.verification == StageStatus::Complete && self.prepared_root.is_some()
    }

    pub fn save(&self) -> Result<(), Error> {
        let directory = app_settings::jobs_dir()?;
        fs::create_dir_all(&directory).context("could not create the job folder")?;
        let contents =
            serde_json::to_string_pretty(self).context("could not serialise job state")?;
        fs::write(job_path()?, contents).context("could not save job state")?;
        Ok(())
    }
}

pub fn job_path() -> Result<PathBuf, Error> {
    Ok(app_settings::jobs_dir()?.join("last-job.json"))
}

pub fn load_last_job() -> Result<Option<JobState>, Error> {
    let path = job_path()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("could not read the saved job"),
    };
    let state: JobState = serde_json::from_str(&contents).context("saved job state is invalid")?;
    if !state.work_dir.is_dir() {
        return Ok(None);
    }
    if let Some(prepared) = &state.prepared_root
        && !prepared.exists()
    {
        return Ok(None);
    }
    Ok(Some(state))
}

pub fn clear_last_job() {
    if let Ok(path) = job_path() {
        let _ = fs::remove_file(path);
    }
}

pub fn stage_is_complete(job: &JobState, kind: StageKind) -> bool {
    let status = match kind {
        StageKind::Downloading => &job.pipeline.downloading,
        StageKind::ArchiveCheck => &job.pipeline.archive_check,
        StageKind::Extracting => &job.pipeline.extracting,
        StageKind::IsoDetected => &job.pipeline.iso_detected,
        StageKind::Converting => &job.pipeline.converting,
        StageKind::Verification => &job.pipeline.verification,
        StageKind::Delivery => &job.pipeline.delivery,
    };
    *status == StageStatus::Complete
}

pub fn work_dir_for_game(game_name: &str) -> Result<PathBuf, Error> {
    let directory = app_settings::jobs_dir()?;
    fs::create_dir_all(&directory)?;
    let safe: String = game_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = directory.join(format!("work-{}-{}-{safe}", std::process::id(), stamp));
    fs::create_dir_all(&path)?;
    Ok(path)
}

pub fn is_inside(path: &Path, root: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_render_matches_requested_layout() {
        let mut pipeline = Pipeline::default();
        pipeline.downloading = StageStatus::Complete;
        pipeline.archive_check = StageStatus::Complete;
        pipeline.extracting = StageStatus::Complete;
        pipeline.iso_detected = StageStatus::Complete;
        pipeline.converting = StageStatus::InProgress;
        pipeline.converting_percent = 62;
        let rendered = pipeline.render("USB export");
        assert!(rendered.contains("Downloading       Complete"));
        assert!(rendered.contains("Archive check     Complete"));
        assert!(rendered.contains("Extracting        Complete"));
        assert!(rendered.contains("Game format       Complete"));
        assert!(rendered.contains("Converting        62%"));
        assert!(rendered.contains("Verification      Waiting"));
        assert!(rendered.contains("USB export        Waiting"));
    }
}
