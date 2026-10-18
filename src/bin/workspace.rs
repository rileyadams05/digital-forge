/// Digital Forge – Centralised Workspace Manager
///
/// Chooses a mounted scratch drive with enough free space and
/// allocates per-job subdirectories under `<drive>:\DigitalForge_Workspace\jobs\`.
///
/// All large temporary work (archive extraction, PKG expansion, staging,
/// merge, verification) must go through this manager. Internal drives are
/// preferred for large jobs, but external drives remain available by choice.
///
/// After success, failure, or cancellation the job directory is removed.
/// On next launch, stale job directories whose owning process is no longer
/// alive are swept automatically.
use anyhow::{Context, Error};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW};
#[cfg(windows)]
use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOVABLE};

// ─────────────────────────────────────────────────────────────────────────────
// Public constants
// ─────────────────────────────────────────────────────────────────────────────

/// Safety margin added on top of the estimated working-set size (20 %).
pub const SPACE_SAFETY_MARGIN: f64 = 0.20;

/// Root directory name created on the chosen workspace drive.
pub const WORKSPACE_ROOT_NAME: &str = "DigitalForge_Workspace";

/// Subdirectory inside the workspace root that holds per-job directories.
pub const JOBS_SUBDIR: &str = "jobs";

// ─────────────────────────────────────────────────────────────────────────────
// Drive enumeration
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DriveInfo {
    /// Drive letter, e.g. `C:`, `D:`, `F:`.
    pub letter: String,
    /// Total capacity in bytes.
    pub total_bytes: u64,
    /// Free / available bytes.
    pub free_bytes: u64,
    /// Whether this drive contains the Windows system directory.
    pub is_system: bool,
    /// Whether this is a fixed (non-removable) drive.
    pub is_fixed: bool,
    /// Optional volume label.
    pub label: String,
}

impl DriveInfo {
    /// Any mounted drive can be used when it has enough space for the job.
    pub fn is_workspace_eligible(&self) -> bool {
        self.total_bytes > 0
    }

    /// Human-readable total capacity string.
    pub fn total_display(&self) -> String {
        format_bytes(self.total_bytes)
    }

    /// Human-readable free space string.
    pub fn free_display(&self) -> String {
        format_bytes(self.free_bytes)
    }
}

/// Enumerate all drives visible to the operating system.
pub fn enumerate_drives() -> Vec<DriveInfo> {
    #[cfg(windows)]
    {
        enumerate_drives_windows()
    }
    #[cfg(not(windows))]
    {
        enumerate_drives_unix()
    }
}

#[cfg(windows)]
fn enumerate_drives_windows() -> Vec<DriveInfo> {
    let system_root = std::env::var("SystemRoot")
        .or_else(|_| std::env::var("WINDIR"))
        .unwrap_or_else(|_| "C:\\Windows".to_owned());
    let system_letter = system_root
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('C');

    let mut drives = Vec::new();

    for letter_byte in b'A'..=b'Z' {
        let letter = letter_byte as char;
        let root = format!("{letter}:\\");
        let root_wide: Vec<u16> = root.encode_utf16().chain([0]).collect();

        // SAFETY: Windows API call
        let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };

        let is_fixed = drive_type == DRIVE_FIXED;
        let is_removable = drive_type == DRIVE_REMOVABLE;
        if !is_fixed && !is_removable {
            continue;
        }

        // Get free/total space
        let mut free_to_caller: u64 = 0;
        let mut total: u64 = 0;
        let mut free_total: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                root_wide.as_ptr(),
                &mut free_to_caller,
                &mut total,
                &mut free_total,
            )
        };
        if ok == 0 || total == 0 {
            continue;
        }

        let is_system = letter == system_letter;

        // Volume label via WMI-free method: read the label from the root
        let label = read_volume_label(&root);

        drives.push(DriveInfo {
            letter: format!("{letter}:"),
            total_bytes: total,
            free_bytes: free_to_caller,
            is_system,
            is_fixed,
            label,
        });
    }

    drives
}

#[cfg(windows)]
fn read_volume_label(root: &str) -> String {
    // Use GetVolumeInformationW
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    let root_wide: Vec<u16> = OsStr::new(root).encode_wide().chain([0]).collect();
    let mut name_buf = vec![0u16; 260];
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut flags: u32 = 0;
    let mut fs_buf = vec![0u16; 260];

    // SAFETY: Windows API
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW(
            root_wide.as_ptr(),
            name_buf.as_mut_ptr(),
            name_buf.len() as u32,
            &mut serial,
            &mut max_component,
            &mut flags,
            fs_buf.as_mut_ptr(),
            fs_buf.len() as u32,
        )
    };
    if ok != 0 {
        let end = name_buf.iter().position(|&c| c == 0).unwrap_or(0);
        String::from_utf16_lossy(&name_buf[..end])
    } else {
        String::new()
    }
}

#[cfg(not(windows))]
fn enumerate_drives_unix() -> Vec<DriveInfo> {
    use std::process::Command;
    let mut drives = Vec::new();
    if let Ok(output) = Command::new("df")
        .args(["-B1", "--output=target,size,avail"])
        .output()
    {
        for line in String::from_utf8_lossy(&output.stdout).lines().skip(1) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                continue;
            }
            let mount = parts[0];
            let total: u64 = parts[1].parse().unwrap_or(0);
            let free: u64 = parts[2].parse().unwrap_or(0);
            let is_system = mount == "/" || mount.starts_with("/boot");
            drives.push(DriveInfo {
                letter: mount.to_owned(),
                total_bytes: total,
                free_bytes: free,
                is_system,
                is_fixed: !mount.starts_with("/media") && !mount.starts_with("/run/media"),
                label: String::new(),
            });
        }
    }
    drives
}

// ─────────────────────────────────────────────────────────────────────────────
// Workspace settings (persisted in app-settings.txt)
// ─────────────────────────────────────────────────────────────────────────────

/// User preference for the workspace drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceDrivePreference {
    /// Automatically pick the best eligible drive.
    Auto,
    /// User has pinned a specific drive letter (e.g. `F:`).
    Pinned(String),
}

impl WorkspaceDrivePreference {
    pub fn from_setting(s: &str) -> Self {
        let s = s.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("auto") {
            Self::Auto
        } else {
            Self::Pinned(s.to_uppercase().trim_end_matches('\\').to_owned())
        }
    }

    pub fn to_setting(&self) -> &str {
        match self {
            Self::Auto => "auto",
            Self::Pinned(d) => d.as_str(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Drive selection
// ─────────────────────────────────────────────────────────────────────────────

/// Select the best workspace drive given optional source/destination paths and
/// a required number of free bytes (including the 20 % safety margin).
///
/// Selection criteria (in order):
/// 1. Does not contain `source_path` (prefer a different physical disk).
/// 2. Does not contain `destination_path`.
/// 3. Prefer fixed internal drives, then the most free space.
pub fn select_workspace_drive(
    preference: &WorkspaceDrivePreference,
    required_free_bytes: u64,
    source_path: Option<&Path>,
    destination_path: Option<&Path>,
) -> Result<DriveInfo, WorkspaceError> {
    let drives = enumerate_drives();

    // If pinned, verify that specific drive first
    if let WorkspaceDrivePreference::Pinned(letter) = preference {
        let norm = normalize_drive_letter(letter);
        let drive = drives
            .iter()
            .find(|d| normalize_drive_letter(&d.letter) == norm)
            .ok_or_else(|| WorkspaceError::PinnedDriveNotFound(letter.clone()))?;
        if !drive.is_workspace_eligible() {
            return Err(WorkspaceError::PinnedDriveIneligible {
                letter: letter.clone(),
                reason: eligibility_reason(drive),
            });
        }
        if drive.free_bytes < required_free_bytes {
            return Err(WorkspaceError::InsufficientSpace {
                letter: letter.clone(),
                free_bytes: drive.free_bytes,
                required_bytes: required_free_bytes,
            });
        }
        return Ok(drive.clone());
    }

    // Auto selection
    let source_letter = source_path.and_then(drive_letter_of);
    let dest_letter = destination_path.and_then(drive_letter_of);

    // Gather eligible candidates
    let mut candidates: Vec<&DriveInfo> = drives
        .iter()
        .filter(|d| d.is_workspace_eligible() && d.free_bytes >= required_free_bytes)
        .collect();

    if candidates.is_empty() {
        // Build a useful diagnostic
        let ineligible: Vec<String> = drives
            .iter()
            .map(|d| {
                format!(
                    "{}  {}  {}  {} free  eligible={}  reason={}",
                    d.letter,
                    if d.label.is_empty() {
                        "(no label)"
                    } else {
                        &d.label
                    },
                    d.total_display(),
                    d.free_display(),
                    d.is_workspace_eligible(),
                    eligibility_reason(d)
                )
            })
            .collect();
        return Err(WorkspaceError::NoEligibleDrive {
            required_bytes: required_free_bytes,
            drive_summary: ineligible,
        });
    }

    // Prefer a separate internal workspace when available, while keeping external
    // and system drives eligible when the user selects them or no alternative fits.
    candidates.sort_by(|a, b| {
        let a_is_src = source_letter.as_deref() == Some(&a.letter[..2]);
        let b_is_src = source_letter.as_deref() == Some(&b.letter[..2]);
        let a_is_dst = dest_letter.as_deref() == Some(&a.letter[..2]);
        let b_is_dst = dest_letter.as_deref() == Some(&b.letter[..2]);

        let a_penalty = (a_is_src as u8) + (a_is_dst as u8);
        let b_penalty = (b_is_src as u8) + (b_is_dst as u8);

        a_penalty
            .cmp(&b_penalty)
            .then_with(|| b.is_fixed.cmp(&a.is_fixed))
            .then_with(|| b.free_bytes.cmp(&a.free_bytes))
    });

    Ok(candidates[0].clone())
}

fn normalize_drive_letter(s: &str) -> String {
    s.trim().trim_end_matches(['\\', '/']).to_uppercase()
}

fn drive_letter_of(path: &Path) -> Option<String> {
    path.to_str()
        .and_then(|s| s.get(..2))
        .map(|s| s.to_uppercase())
}

fn eligibility_reason(d: &DriveInfo) -> String {
    if d.total_bytes == 0 {
        "drive capacity could not be determined".to_owned()
    } else {
        "eligible; fixed internal drives are preferred automatically".to_owned()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Error types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum WorkspaceError {
    PinnedDriveNotFound(String),
    PinnedDriveIneligible {
        letter: String,
        reason: String,
    },
    InsufficientSpace {
        letter: String,
        free_bytes: u64,
        required_bytes: u64,
    },
    NoEligibleDrive {
        required_bytes: u64,
        drive_summary: Vec<String>,
    },
    Io(io::Error),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PinnedDriveNotFound(l) => write!(f, "Pinned workspace drive {l} was not found"),
            Self::PinnedDriveIneligible { letter, reason } => {
                write!(
                    f,
                    "Pinned workspace drive {letter} is not eligible: {reason}"
                )
            }
            Self::InsufficientSpace {
                letter,
                free_bytes,
                required_bytes,
            } => write!(
                f,
                "Drive {letter} has only {} free but {} is required",
                format_bytes(*free_bytes),
                format_bytes(*required_bytes)
            ),
            Self::NoEligibleDrive {
                required_bytes,
                drive_summary,
            } => {
                write!(
                    f,
                    "No suitable Digital Forge workspace drive is available.\n\
                     Requirements: Mounted drive · Free space ≥ {}\n\n\
                     Detected drives:\n{}",
                    format_bytes(*required_bytes),
                    drive_summary.join("\n")
                )
            }
            Self::Io(e) => write!(f, "Workspace I/O error: {e}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<io::Error> for WorkspaceError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Job workspace
// ─────────────────────────────────────────────────────────────────────────────

/// Reliably remove a directory tree on Windows, recursively stripping
/// read-only attributes and retrying on transient file locks.
pub fn remove_dir_all_robust(path: &Path) -> Result<(), io::Error> {
    if !path.exists() {
        return Ok(());
    }
    clear_readonly_recursive(path);
    let mut last_err = None;
    for attempt in 0..5 {
        match fs::remove_dir_all(path) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                clear_readonly_recursive(path);
                std::thread::sleep(std::time::Duration::from_millis(50 * (attempt + 1)));
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "rmdir", "/S", "/Q"])
            .arg(path)
            .output();
        if !path.exists() {
            return Ok(());
        }
    }
    if let Some(e) = last_err {
        Err(e)
    } else {
        Ok(())
    }
}

/// Move the process away from a removable workspace volume before cleanup or
/// returning control to Windows Explorer. This prevents the process current
/// directory from keeping a drive busy after a conversion finishes.
pub fn release_workspace_drive() {
    let fallback = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .or_else(|| std::env::temp_dir().canonicalize().ok());
    if let Some(path) = fallback {
        let _ = std::env::set_current_dir(path);
    }
}

fn clear_readonly_recursive(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        let mut perms = metadata.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            let _ = fs::set_permissions(path, perms);
        }
    }
    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                clear_readonly_recursive(&entry.path());
            }
        }
    }
}

/// Metadata written into each job directory so stale-job detection works across
/// restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobMarker {
    pub job_id: String,
    pub pid: u32,
    pub started_at_unix: u64,
    pub workspace_path: String,
}

/// RAII guard: removes the job directory on drop unless `keep()` is called.
///
/// Use `keep()` when you want to leave the directory behind intentionally
/// (e.g. to pass it to the delivery step that will clean it after transfer).
pub struct JobWorkspace {
    pub root: PathBuf,
    pub tmp: PathBuf,
    pub job_id: String,
    remove_on_drop: bool,
    saved_temp: Option<std::ffi::OsString>,
    saved_tmp: Option<std::ffi::OsString>,
}

impl JobWorkspace {
    /// Create a new job directory under `<drive>:\DigitalForge_Workspace\jobs\`.
    ///
    /// `label` is used in the directory name (e.g. `CUSA09172_LANoire`).
    /// Does **not** verify free space — call [`preflight`] or
    /// [`JobWorkspaceBuilder::build`] which performs the check for you.
    pub fn create(drive_root: &Path, label: &str) -> Result<Self, Error> {
        let ws_root = drive_root.join(WORKSPACE_ROOT_NAME).join(JOBS_SUBDIR);
        fs::create_dir_all(&ws_root).context("could not create workspace jobs directory")?;

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let safe_label: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(48)
            .collect();
        let job_id = format!("{stamp}_{safe_label}");
        let root = ws_root.join(&job_id);
        fs::create_dir_all(&root).context("could not create job workspace directory")?;

        let tmp = root.join("tmp");
        fs::create_dir_all(&tmp).context("could not create workspace scratch directory")?;

        // Redirect process-level TEMP and TMP to the dedicated workspace scratch directory
        let saved_temp = std::env::var_os("TEMP");
        let saved_tmp = std::env::var_os("TMP");
        unsafe {
            std::env::set_var("TEMP", &tmp);
            std::env::set_var("TMP", &tmp);
        }

        // Write job marker
        let marker = JobMarker {
            job_id: job_id.clone(),
            pid: std::process::id(),
            started_at_unix: stamp,
            workspace_path: root.to_string_lossy().into_owned(),
        };
        let marker_path = root.join(".df_job_marker.json");
        if let Ok(json) = serde_json::to_string_pretty(&marker) {
            let _ = fs::write(marker_path, json);
        }

        Ok(Self {
            root,
            tmp,
            job_id,
            remove_on_drop: true,
            saved_temp,
            saved_tmp,
        })
    }

    /// Prevent the workspace from being deleted on drop.
    /// Call this **only** when a longer-lived component takes ownership of cleanup.
    pub fn keep(&mut self) {
        self.remove_on_drop = false;
    }

    /// Explicitly remove the workspace now (e.g. after success/failure).
    /// Safe to call multiple times.
    pub fn cleanup(&mut self) {
        release_workspace_drive();
        if let Some(val) = self.saved_temp.take() {
            unsafe {
                std::env::set_var("TEMP", val);
            }
        }
        if let Some(val) = self.saved_tmp.take() {
            unsafe {
                std::env::set_var("TMP", val);
            }
        }
        if self.root.exists() {
            let _ = remove_dir_all_robust(&self.root);
        }
        release_workspace_drive();
        self.remove_on_drop = false;
    }

    /// Convenience sub-directories. Created lazily.
    pub fn subdir(&self, name: &str) -> Result<PathBuf, Error> {
        let p = self.root.join(name);
        fs::create_dir_all(&p)
            .with_context(|| format!("could not create workspace subdir {name}"))?;
        Ok(p)
    }
}

impl Drop for JobWorkspace {
    fn drop(&mut self) {
        release_workspace_drive();
        if let Some(val) = self.saved_temp.take() {
            unsafe {
                std::env::set_var("TEMP", val);
            }
        }
        if let Some(val) = self.saved_tmp.take() {
            unsafe {
                std::env::set_var("TMP", val);
            }
        }
        if self.remove_on_drop
            && self.root.exists()
            && !self.root.join(".df_preserve_diagnostics").is_file()
        {
            let _ = remove_dir_all_robust(&self.root);
        }
        release_workspace_drive();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Builder with space preflight and user-facing plan display
// ─────────────────────────────────────────────────────────────────────────────

pub struct JobWorkspaceBuilder {
    label: String,
    estimated_bytes: u64,
    source_path: Option<PathBuf>,
    destination_path: Option<PathBuf>,
    preference: WorkspaceDrivePreference,
    silent: bool,
}

impl JobWorkspaceBuilder {
    pub fn new(label: impl Into<String>, estimated_bytes: u64) -> Self {
        Self {
            label: label.into(),
            estimated_bytes,
            source_path: None,
            destination_path: None,
            preference: WorkspaceDrivePreference::Auto,
            silent: false,
        }
    }

    pub fn source(mut self, path: impl Into<PathBuf>) -> Self {
        self.source_path = Some(path.into());
        self
    }

    pub fn destination(mut self, path: impl Into<PathBuf>) -> Self {
        self.destination_path = Some(path.into());
        self
    }

    pub fn preference(mut self, pref: WorkspaceDrivePreference) -> Self {
        self.preference = pref;
        self
    }

    /// Don't print the workspace plan to stdout (for non-interactive callers).
    pub fn silent(mut self) -> Self {
        self.silent = true;
        self
    }

    /// Add the safety margin and select a drive, then create and return the job workspace.
    pub fn build(self) -> Result<JobWorkspace, Error> {
        let margin = (self.estimated_bytes as f64 * SPACE_SAFETY_MARGIN) as u64;
        let required = self.estimated_bytes + margin;

        let drive = select_workspace_drive(
            &self.preference,
            required,
            self.source_path.as_deref(),
            self.destination_path.as_deref(),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        let drive_root = PathBuf::from(format!("{}\\", drive.letter));

        let ws = JobWorkspace::create(&drive_root, &self.label)
            .context("could not create job workspace")?;

        if !self.silent {
            print_workspace_plan(&drive, &ws, self.estimated_bytes, required);
        }

        Ok(ws)
    }
}

/// Print the workspace plan to stdout before a large operation begins.
pub fn print_workspace_plan(
    drive: &DriveInfo,
    ws: &JobWorkspace,
    estimated: u64,
    required_with_margin: u64,
) {
    println!();
    println!("Digital Forge — Workspace Plan");
    println!("========================================================");
    println!("Job directory:        {}", ws.root.display());
    println!("Drive:                {}", drive.letter);
    if !drive.label.is_empty() {
        println!("Volume label:         {}", drive.label);
    }
    println!("Drive capacity:       {}", drive.total_display());
    println!("Drive free:           {}", drive.free_display());
    println!("Working estimate:     {}", format_bytes(estimated));
    let margin = required_with_margin.saturating_sub(estimated);
    println!("Safety margin (20%):  {}", format_bytes(margin));
    println!(
        "Required (total):     {}",
        format_bytes(required_with_margin)
    );
    println!(
        "System drive:         {}",
        if drive.is_system { "Yes" } else { "No" }
    );
    println!();
}

// ─────────────────────────────────────────────────────────────────────────────
// Stale job cleanup on startup
// ─────────────────────────────────────────────────────────────────────────────

/// Scan all workspace roots across eligible drives and remove job directories
/// whose owning process is no longer alive.
///
/// Only touches directories inside `<drive>:\DigitalForge_Workspace\jobs\`.
/// Never deletes anything outside of that path.
pub fn sweep_stale_jobs(preference: &WorkspaceDrivePreference) {
    let drives = enumerate_drives();
    let current_pid = std::process::id();

    for drive in &drives {
        let ws_root =
            PathBuf::from(format!("{}\\{}", drive.letter, WORKSPACE_ROOT_NAME)).join(JOBS_SUBDIR);
        sweep_stale_in_dir(&ws_root, current_pid);
    }

    // Also check the pinned drive if set
    if let WorkspaceDrivePreference::Pinned(letter) = preference {
        let ws_root =
            PathBuf::from(format!("{}\\{}", letter, WORKSPACE_ROOT_NAME)).join(JOBS_SUBDIR);
        sweep_stale_in_dir(&ws_root, current_pid);
    }

    // Sweep any orphaned temp artifacts in %TEMP% from previous crashes or runs
    sweep_system_temp();

    // Sweep any orphaned jobs in APPDATA
    sweep_appdata_jobs();
}

fn sweep_system_temp() {
    let temp_dir = std::env::temp_dir();
    let Ok(entries) = fs::read_dir(&temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("pkgbuild_")
            || name.starts_with("pkg-")
            || name == "digital_forge"
            || name == "digital-forge"
            || name == "test_rdr2_extract"
            || name == "VortexPrimeXAnyPS4"
        {
            let _ = remove_dir_all_robust(&path);
        }
    }
}

fn sweep_appdata_jobs() {
    if let Ok(jobs) = super::app_settings::jobs_dir() {
        if let Ok(entries) = fs::read_dir(&jobs) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let _ = remove_dir_all_robust(&path);
                }
            }
        }
    }
    if let Ok(cfg) = super::app_settings::config_dir() {
        let worker = cfg.join("worker");
        if let Ok(entries) = fs::read_dir(&worker) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let _ = remove_dir_all_robust(&path);
                }
            }
        }
    }
}

fn sweep_stale_in_dir(jobs_dir: &Path, current_pid: u32) {
    let Ok(entries) = fs::read_dir(jobs_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.join(".df_preserve_diagnostics").is_file() {
            continue;
        }
        let marker_path = path.join(".df_job_marker.json");
        if !marker_path.is_file() {
            // Directory without a marker – check if it's an old abandoned folder
            if let Ok(meta) = path.metadata() {
                if let Ok(modified) = meta.modified() {
                    if let Ok(elapsed) = modified.elapsed() {
                        if elapsed.as_secs() > 3600 {
                            let _ = remove_dir_all_robust(&path);
                            continue;
                        }
                    }
                }
            }
            continue;
        }
        let Ok(contents) = fs::read_to_string(&marker_path) else {
            continue;
        };
        let Ok(marker): Result<JobMarker, _> = serde_json::from_str(&contents) else {
            continue;
        };

        if marker.pid == current_pid {
            // This is our own session – never delete it
            continue;
        }

        if process_is_alive(marker.pid) {
            // Another Digital Forge instance is using this workspace
            continue;
        }

        // Stale job: process is gone, clean it up
        println!(
            "[Workspace] Removing stale job workspace (PID {} no longer alive): {}",
            marker.pid,
            path.display()
        );
        let _ = remove_dir_all_robust(&path);
    }
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // STILL_ACTIVE = 259 = STATUS_PENDING
        const STILL_ACTIVE: u32 = 259;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code: u32 = 0;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            ok != 0 && code == STILL_ACTIVE
        }
    }
    #[cfg(not(windows))]
    {
        // On Linux, send signal 0 to check existence
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Space estimation helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Estimate workspace bytes for a PS4 Base + Update extraction workflow.
///
/// Accounts for:
/// - Base PKG expansion (stored-data PKGs expand to ~same size)
/// - Base staging tree
/// - Update PKG expansion
/// - Merged output
///
/// The 20 % safety margin is added by `JobWorkspaceBuilder`.
pub fn estimate_ps4_base_update_workspace(
    base_compressed_bytes: u64,
    update_compressed_bytes: u64,
) -> u64 {
    // Base: compressed archive -> extracted PKG -> staged loose tree
    // The extraction of a RAR holding a PKG reproduces the PKG at the same size.
    // OrbisPkgTool then extracts the PKG contents (also ~same size for uncompressed data games).
    let base_extract = base_compressed_bytes; // PKG on disk
    let base_stage = base_compressed_bytes; // loose game tree
    // Update: compressed -> PKG -> extracted, overlaid
    let upd_extract = update_compressed_bytes; // update PKG on disk
    // The merged game is approximately the same size as the base plus incremental update delta
    // (conservatively assume full base size as merged output)
    let merged = base_compressed_bytes;

    base_extract + base_stage + upd_extract + merged
}

/// Probe archive uncompressed sizes using 7-Zip's list output.
/// Returns `(base_bytes, update_bytes)` where available; falls back to
/// the on-disk compressed size otherwise.
pub fn probe_archive_sizes(base_archive: &Path, update_archive: Option<&Path>) -> (u64, u64) {
    let base_size = probe_single_archive_size(base_archive)
        .or_else(|| probe_path_size(base_archive))
        .unwrap_or(0);
    let update_size = update_archive
        .and_then(|p| probe_single_archive_size(p).or_else(|| probe_path_size(p)))
        .unwrap_or(0);
    (base_size, update_size)
}

fn probe_single_archive_size(archive: &Path) -> Option<u64> {
    // Avoid asking 7-Zip to recursively list an extracted game directory.
    if archive.is_dir() {
        return None;
    }
    use iso2god::forge::archive::find_7zip_executable;
    let tool = find_7zip_executable()?;
    let output = std::process::Command::new(&tool)
        .args(["l", "-ba", "-slt"])
        .arg(archive)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut total: u64 = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Size = ") {
            if let Ok(n) = rest.trim().parse::<u64>() {
                total += n;
            }
        }
    }
    if total > 0 { Some(total) } else { None }
}

// ─────────────────────────────────────────────────────────────────────────────
// Settings integration helpers
// ─────────────────────────────────────────────────────────────────────────────

fn probe_path_size(path: &Path) -> Option<u64> {
    let metadata = path.metadata().ok()?;
    if !metadata.is_dir() {
        return Some(metadata.len());
    }

    // Use filesystem metadata only; never read the large game payload to size it.
    let mut total = 0u64;
    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                if let Ok(metadata) = entry.metadata() {
                    total = total.saturating_add(metadata.len());
                }
            }
        }
    }
    Some(total)
}

/// Read the workspace drive preference from the app-settings file.
/// Returns `Auto` if not set.
pub fn load_workspace_preference() -> WorkspaceDrivePreference {
    use super::app_settings::load_app_settings;
    // We extend AppSettings with an optional `workspace_drive` field read directly
    if let Ok(settings) = load_app_settings() {
        WorkspaceDrivePreference::from_setting(&settings.workspace_drive)
    } else {
        WorkspaceDrivePreference::Auto
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Formatting helpers
// ─────────────────────────────────────────────────────────────────────────────

pub fn format_bytes(bytes: u64) -> String {
    const TB: u64 = 1_000_000_000_000;
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    if bytes >= TB {
        format!("{:.2} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0} MB", bytes as f64 / MB as f64)
    } else {
        format!("{bytes} B")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn format_bytes_covers_all_scales() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(5_000_000), "5 MB");
        assert_eq!(format_bytes(53_700_000_000), "53.7 GB");
        assert_eq!(format_bytes(1_819_000_000_000), "1.82 TB");
    }

    #[test]
    fn workspace_preference_round_trips() {
        assert_eq!(
            WorkspaceDrivePreference::from_setting("auto"),
            WorkspaceDrivePreference::Auto
        );
        assert_eq!(
            WorkspaceDrivePreference::from_setting(""),
            WorkspaceDrivePreference::Auto
        );
        assert_eq!(
            WorkspaceDrivePreference::from_setting("F:"),
            WorkspaceDrivePreference::Pinned("F:".to_owned())
        );
        assert_eq!(
            WorkspaceDrivePreference::from_setting("f:\\"),
            WorkspaceDrivePreference::Pinned("F:".to_owned())
        );
    }

    #[test]
    fn job_workspace_cleans_up_on_drop() {
        let tmp = std::env::temp_dir().join(format!("df_ws_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        {
            let ws = JobWorkspace::create(&tmp, "test_job").unwrap();
            assert!(ws.root.is_dir());
        }
        // After drop, directory should be gone
        assert!(
            !tmp.join(WORKSPACE_ROOT_NAME).join(JOBS_SUBDIR).exists()
                || fs::read_dir(tmp.join(WORKSPACE_ROOT_NAME).join(JOBS_SUBDIR))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true)
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn job_workspace_keep_prevents_deletion() {
        let tmp = std::env::temp_dir().join(format!("df_ws_keep_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let root_copy;
        {
            let mut ws = JobWorkspace::create(&tmp, "keep_test").unwrap();
            root_copy = ws.root.clone();
            ws.keep();
        }
        // Should still exist
        assert!(root_copy.is_dir(), "kept workspace should survive drop");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn estimate_ps4_workspace_is_reasonable() {
        let base = 16_864_247_808u64; // ~15.7 GB LA Noire base
        let upd = 13_762_953_216u64; // ~12.8 GB LA Noire update
        let est = estimate_ps4_base_update_workspace(base, upd);
        // Should be > 50 GB and < 150 GB
        assert!(
            est > 50 * 1_000_000_000,
            "estimate too small: {}",
            format_bytes(est)
        );
        assert!(
            est < 150 * 1_000_000_000,
            "estimate too large: {}",
            format_bytes(est)
        );
    }

    #[test]
    fn system_drive_remains_available_when_it_has_space() {
        let drives = enumerate_drives();
        for d in &drives {
            if d.is_system {
                assert!(
                    d.is_workspace_eligible(),
                    "system drive {} should remain an optional workspace",
                    d.letter
                );
            }
        }
    }

    #[test]
    fn external_and_sub_1tb_drives_remain_eligible() {
        let fake = DriveInfo {
            letter: "Z:".to_owned(),
            total_bytes: 500_000_000_000, // 500 GB
            free_bytes: 400_000_000_000,
            is_system: false,
            is_fixed: true,
            label: "small".to_owned(),
        };
        assert!(fake.is_workspace_eligible());
    }

    #[test]
    fn stale_sweep_skips_current_process() {
        // Create a fake job marker for the current PID and ensure it survives the sweep
        let tmp = std::env::temp_dir().join(format!("df_sweep_test_{}", std::process::id()));
        let jobs_dir = tmp.join(WORKSPACE_ROOT_NAME).join(JOBS_SUBDIR);
        let fake_job = jobs_dir.join("fake_live_job");
        fs::create_dir_all(&fake_job).unwrap();
        let marker = JobMarker {
            job_id: "fake_live_job".to_owned(),
            pid: std::process::id(),
            started_at_unix: 0,
            workspace_path: fake_job.to_string_lossy().into_owned(),
        };
        fs::write(
            fake_job.join(".df_job_marker.json"),
            serde_json::to_string(&marker).unwrap(),
        )
        .unwrap();

        sweep_stale_in_dir(&jobs_dir, std::process::id());

        // Current-PID job must still be there
        assert!(fake_job.is_dir(), "current-process job must not be swept");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn stale_sweep_preserves_failed_ps4_diagnostics() {
        let tmp = std::env::temp_dir().join(format!("df_diagnostics_test_{}", std::process::id()));
        let job = tmp.join("failed_ps4");
        fs::create_dir_all(&job).unwrap();
        fs::write(job.join(".df_preserve_diagnostics"), "retained failure").unwrap();
        fs::write(job.join("project.gp4"), "diagnostic evidence").unwrap();
        sweep_stale_in_dir(&tmp, std::process::id());
        assert!(job.join("project.gp4").is_file());
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn failed_ps4_diagnostics_survive_workspace_drop() {
        let tmp =
            std::env::temp_dir().join(format!("df_drop_diagnostics_test_{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join(".df_preserve_diagnostics"), "retained failure").unwrap();
        let job = JobWorkspace {
            root: tmp.clone(),
            tmp: tmp.clone(),
            job_id: "diagnostic-test".into(),
            remove_on_drop: true,
            saved_temp: None,
            saved_tmp: None,
        };
        drop(job);
        assert!(tmp.join(".df_preserve_diagnostics").is_file());
        fs::remove_dir_all(&tmp).unwrap();
    }
}
