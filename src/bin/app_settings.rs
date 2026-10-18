use anyhow::{Context, Error};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::store::DEFAULT_STORE_BASE;

#[derive(Debug, Clone)]
pub struct AppSettings {
    pub auto_cleanup: bool,
    pub store_base_url: String,
    /// User-pinned workspace drive letter, e.g. `"F:"`, or `"auto"` / `""` for
    /// automatic selection (see `workspace::WorkspaceDrivePreference`).
    pub workspace_drive: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            auto_cleanup: true,
            store_base_url: DEFAULT_STORE_BASE.to_owned(),
            workspace_drive: String::new(),
        }
    }
}

pub fn config_dir() -> Result<PathBuf, Error> {
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(target_os = "linux")]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    #[cfg(all(not(windows), not(target_os = "linux")))]
    let base: Option<PathBuf> = None;

    let base = base
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf))
        })
        .context("could not determine where to save application settings")?;
    Ok(base.join("iso2god"))
}

pub fn logs_dir() -> Result<PathBuf, Error> {
    Ok(config_dir()?.join("logs"))
}

pub fn jobs_dir() -> Result<PathBuf, Error> {
    Ok(config_dir()?.join("jobs"))
}

fn settings_path() -> Result<PathBuf, Error> {
    Ok(config_dir()?.join("app-settings.txt"))
}

pub fn load_app_settings() -> Result<AppSettings, Error> {
    let path = settings_path()?;
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(AppSettings::default()),
        Err(error) => return Err(error).context("could not read application settings"),
    };
    let mut settings = AppSettings::default();
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("auto_cleanup=") {
            settings.auto_cleanup = matches!(value.trim(), "1" | "true" | "yes");
        } else if let Some(value) = line.strip_prefix("store_base_url=") {
            let value = value.trim();
            if !value.is_empty() {
                settings.store_base_url = value.trim_end_matches('/').to_owned();
            }
        } else if let Some(value) = line.strip_prefix("workspace_drive=") {
            settings.workspace_drive = value.trim().to_owned();
        }
    }
    Ok(settings)
}

pub fn save_app_settings(settings: &AppSettings) -> Result<(), Error> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("could not create the settings folder")?;
    }
    let contents = format!(
        "auto_cleanup={}\nstore_base_url={}\nworkspace_drive={}\n",
        if settings.auto_cleanup {
            "true"
        } else {
            "false"
        },
        settings.store_base_url,
        settings.workspace_drive
    );
    fs::write(&path, contents).context("could not save application settings")
}
