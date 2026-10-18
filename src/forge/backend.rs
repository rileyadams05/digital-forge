use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Error;
use serde::{Deserialize, Serialize};

use super::detector::DetectionResult;
use super::format::TargetFormat;
use super::platform::Platform;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressUpdate {
    pub stage: String,
    pub percent: u8,
    pub details: Option<String>,
    pub bytes_processed: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub is_valid: bool,
    pub issues: Vec<String>,
    pub warnings: Vec<String>,
    pub game_title: Option<String>,
    pub game_id: Option<String>,
}

impl ValidationReport {
    pub fn ok() -> Self {
        Self {
            is_valid: true,
            issues: Vec::new(),
            warnings: Vec::new(),
            game_title: None,
            game_id: None,
        }
    }

    pub fn failed(issue: impl Into<String>) -> Self {
        Self {
            is_valid: false,
            issues: vec![issue.into()],
            warnings: Vec::new(),
            game_title: None,
            game_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversionResult {
    pub platform: Platform,
    pub target_format: TargetFormat,
    pub output_path: PathBuf,
    pub total_size: u64,
    pub was_passthrough: bool,
    pub title_id: Option<String>,
    pub title_name: Option<String>,
    pub aux_files: Vec<PathBuf>,
    pub tool_report: Option<ToolReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolReport {
    pub backend_name: String,
    pub upstream_project: String,
    pub upstream_version: String,
    pub operation: String,
    pub command: Vec<String>,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub resulting_files: Vec<PathBuf>,
    pub validation_result: String,
}

pub trait ConversionBackend: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn platform(&self) -> Platform;

    fn validate_input(&self, input: &DetectionResult) -> Result<ValidationReport, Error>;

    fn available_outputs(&self, input: &DetectionResult) -> Vec<TargetFormat>;

    fn convert(
        &self,
        input: &DetectionResult,
        target: TargetFormat,
        output_dir: &Path,
        progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ConversionResult, Error>;

    fn validate_output(&self, result: &ConversionResult) -> Result<ValidationReport, Error>;
}
