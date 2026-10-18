use super::*;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
pub struct ConsoleSettings {
    pub host: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default = "ftp_port")]
    pub port: u16,
    #[serde(default = "background_port")]
    pub background_port: u16,
}
fn ftp_port() -> u16 {
    21
}
fn background_port() -> u16 {
    7564
}

pub fn configured_ftp() -> Result<Option<crate::FtpSettings>, Error> {
    if let Some(path) = env::var_os("DIGITAL360_PC_FTP_SETTINGS") {
        let mut settings = crate::load_ftp_settings_from(Path::new(&path))?
            .context("Complete FTP setup on your PC first.")?;
        settings.password = crate::load_local_saved_password()?
            .context("Unlock your PC password keyring and reopen the PC app.")?;
        return Ok(Some(settings));
    }
    let Some(path) = env::var_os("ISO2GOD_WORKER_CONSOLE") else {
        return Ok(None);
    };
    let settings: ConsoleSettings = serde_json::from_slice(&fs::read(path)?)?;
    Ok(Some(crate::FtpSettings {
        host: settings.host,
        username: settings.username,
        password: settings.password,
        port: settings.port,
        background_port: Some(settings.background_port),
        destination_path: crate::DEFAULT_FTP_DESTINATION.to_owned(),
    }))
}

#[derive(Clone, Serialize, Deserialize)]
struct Job {
    id: String,
    listing: StoreListing,
    option: crate::store::DownloadOption,
    drive: String,
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    notified: bool,
}

pub struct Runtime {
    root: PathBuf,
    jobs: Mutex<Vec<Job>>,
    listings: Mutex<BTreeMap<String, StoreListing>>,
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), Error> {
    let pending = path.with_extension("pending");
    fs::write(&pending, serde_json::to_vec(value)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&pending, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(pending, path)?;
    Ok(())
}

// Pairing supplies the console address. Reuse its saved login and the installed
// background service before asking the user for dashboard credentials.
fn console_connection() -> Result<crate::RcloneFtp, Error> {
    let settings = configured_ftp()?
        .context("Console pairing is missing. Reinstall this Xbox app from your configured PC.")?;
    let ftp = crate::RcloneFtp::new(&settings)?;
    let primary_result = ftp.test_connection();
    if primary_result.is_ok() {
        return Ok(ftp);
    }
    if let Some(port) = settings
        .background_port
        .filter(|port| *port != settings.port)
    {
        let background = ftp.with_port(port)?;
        if background.test_connection().is_ok() {
            return Ok(background);
        }
    }
    // Keep the dashboard authentication error even if the background endpoint
    // is offline; otherwise an incorrect saved login looks like a network error.
    primary_result.map(|_| ftp).map_err(|error| {
        let detail = format!("{error:#}").to_lowercase();
        if settings.username.is_empty() || settings.password.is_empty()
            || detail.contains("530") || detail.contains("incorrect password")
            || detail.contains("login incorrect") || detail.contains("authentication failed")
        {
            anyhow::anyhow!("FTP_LOGIN_REQUIRED: Enter your dashboard FTP details once. They will be remembered automatically.")
        } else {
            anyhow::anyhow!("Cannot connect to your Xbox. Keep it powered on and check the network, then retry.")
        }
    })
}

impl Runtime {
    pub fn start() -> Result<Arc<Self>, Error> {
        let root = app_settings::config_dir()?.join("worker");
        fs::create_dir_all(&root)?;
        let mut jobs: Vec<Job> = match fs::read(root.join("queue.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        for job in &mut jobs {
            if job.status == "running" {
                job.status = "queued".into();
            }
        }
        let runtime = Arc::new(Self {
            root,
            jobs: Mutex::new(jobs),
            listings: Mutex::new(BTreeMap::new()),
        });
        runtime.persist(&runtime.jobs.lock().unwrap())?;
        let runner = runtime.clone();
        thread::spawn(move || runner.schedule());
        Ok(runtime)
    }

    fn persist(&self, jobs: &[Job]) -> Result<(), Error> {
        atomic_json(&self.root.join("queue.json"), &jobs)
    }

    fn schedule(&self) {
        let mut active: Option<(String, Child)> = None;
        loop {
            let result = (|| -> Result<(), Error> {
                let mut jobs = self.jobs.lock().unwrap();
                if let Some((id, child)) = active.as_mut() {
                    if let Some(status) = child.try_wait()? {
                        if let Some(job) = jobs.iter_mut().find(|job| &job.id == id) {
                            job.status = if status.success() {
                                "complete"
                            } else {
                                "failed"
                            }
                            .into();
                            job.error = if status.success() {
                                None
                            } else {
                                Some("Job failed; files retained. Retry after correcting the problem.".into())
                            };
                        }
                        active = None;
                        self.persist(&jobs)?;
                    }
                }
                if active.is_none() {
                    if let Some(job) = jobs.iter_mut().find(|job| job.status == "queued") {
                        let directory = self.root.join(&job.id);
                        fs::create_dir_all(&directory)?;
                        let request = directory.join("request.json");
                        atomic_json(&request, job)?;
                        let mut command = Command::new(env::current_exe()?);
                        command
                            .arg("--store-worker-job")
                            .arg(&request)
                            .env("ISO2GOD_QUEUE_JOB", "1")
                            .env("ISO2GOD_FTP_DRIVE", &job.drive)
                            .env("ISO2GOD_WORKER_PROGRESS", directory.join("progress.json"))
                            .env("XDG_CONFIG_HOME", &directory)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null());
                        command.env("DIGITAL360_STAY_OPEN", "1");
                        #[cfg(windows)]
                        {
                            use std::os::windows::process::CommandExt;
                            command
                                .creation_flags(0x08000000)
                                .env("APPDATA", &directory);
                        }
                        match command.spawn() {
                            Ok(child) => {
                                job.status = "running".into();
                                active = Some((job.id.clone(), child));
                            }
                            Err(_) => {
                                job.status = "failed".into();
                                job.error = Some("Could not start background job.".into());
                            }
                        }
                        self.persist(&jobs)?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("Worker scheduler: {error}");
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    pub fn console(&self, request: ConsoleRequest) -> Result<String, Error> {
        match request.action.as_str() {
            "activity" => {
                let jobs = self.jobs.lock().unwrap();
                let job = jobs
                    .iter()
                    .rev()
                    .find(|job| matches!(job.status.as_str(), "queued" | "running" | "failed"))
                    .or_else(|| jobs.iter().rev().find(|job| job.status != "cancelled"));
                let Some(job) = job else {
                    return Ok("Idle\n".into());
                };
                let progress = fs::read(self.root.join(&job.id).join("progress.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                let state = match job.status.as_str() {
                    "complete" => "Complete. Rescan your Xbox library to see the game.".into(),
                    "failed" => "Could not finish. Files are retained; select Retry below.".into(),
                    "running" => progress
                        .as_ref()
                        .map(|p| p["stage"].as_str().unwrap_or("Preparing").to_owned())
                        .unwrap_or("Starting automatically...".into()),
                    _ => "Starting automatically when the current game finishes...".into(),
                };
                let mut rows = vec![format!("{} - {}", clean(&job.listing.title), state)];
                if job.status == "running" {
                    if let Some(progress) = progress.as_ref() {
                        rows.push(format!(
                            "@progress\t{}",
                            progress["percent"].as_u64().unwrap_or(0).min(100)
                        ));
                        rows.push(format!(
                            "@detail\t{}",
                            clean(
                                progress["detail"]
                                    .as_str()
                                    .unwrap_or("Calculating time remaining...")
                            )
                        ));
                    }
                } else if job.status == "complete" {
                    rows.push("@progress\t100".into());
                    rows.push("@detail\tTransfer complete and verified.".into());
                }
                if job.status == "failed" {
                    rows.push(format!("retry:{}\tRetry", job.id));
                }
                if matches!(job.status.as_str(), "complete" | "failed" | "cancelled") {
                    rows.push("store\tReturn to main menu".into());
                }
                Ok(rows.join("\n"))
            }
            "leave" => {
                let jobs = self.jobs.lock().unwrap();
                if jobs
                    .iter()
                    .any(|job| matches!(job.status.as_str(), "queued" | "running"))
                {
                    anyhow::bail!(
                        "Installation is still running. Keep Digital 360 open until FTP upload and verification finish."
                    );
                }
                Ok("Returning to dashboard.\n".into())
            }
            "notifications" => {
                let jobs = self.jobs.lock().unwrap();
                match jobs.iter().find(|job| {
                    !job.notified && matches!(job.status.as_str(), "complete" | "failed")
                }) {
                    Some(job) => {
                        let title: String = clean(&job.listing.title).chars().take(60).collect();
                        let message = if job.status == "complete" {
                            "Ready. Rescan your Xbox library."
                        } else {
                            "Could not finish. Open Digital 360 to retry."
                        };
                        Ok(format!("{}\n{}: {}\n", job.id, title, message))
                    }
                    None => Ok("\n".into()),
                }
            }
            "ack_notification" => {
                let mut jobs = self.jobs.lock().unwrap();
                let job = jobs
                    .iter_mut()
                    .find(|job| {
                        job.id == request.value
                            && matches!(job.status.as_str(), "complete" | "failed")
                    })
                    .context("Notification not found.")?;
                job.notified = true;
                self.persist(&jobs)?;
                Ok("OK\n".into())
            }
            "connection" => {
                console_connection()?;
                Ok("Connected\n".into())
            }
            "search" => {
                let query = request.value.trim();
                if query.is_empty() || query.len() > 200 {
                    anyhow::bail!("Enter a game name (maximum 200 characters).");
                }
                let results = store()?.search(query)?;
                let mut cache = self.listings.lock().unwrap();
                cache.clear();
                let mut rows = vec!["Search results".to_owned()];
                for listing in results.into_iter().take(100) {
                    rows.push(format!("{}\t{}", clean(&listing.id), clean(&listing.title)));
                    cache.insert(listing.id.clone(), listing);
                }
                Ok(rows.join("\n"))
            }
            "options" => {
                let listing = self
                    .listings
                    .lock()
                    .unwrap()
                    .get(&request.value)
                    .cloned()
                    .context("Search for the game again.")?;
                let mut listing = store()?.inspect(&listing)?;
                listing
                    .download_options
                    .retain(|option| !crate::store::is_onefile_option(option));
                let mut rows = vec![clean(&listing.title)];
                for (index, option) in listing.download_options.iter().enumerate() {
                    rows.push(format!("{index}\t{}", clean(&option.label)));
                }
                if rows.len() == 1 {
                    anyhow::bail!("No download links were found on this source page.");
                }
                self.listings
                    .lock()
                    .unwrap()
                    .insert(listing.id.clone(), listing);
                Ok(rows.join("\n"))
            }
            "drives" => {
                let ftp = console_connection()?;
                let drives = crate::detect_storage_drives(&ftp)?;
                let mut rows = vec!["Choose a drive connected to your Xbox".into()];
                rows.extend(drives.into_iter().map(|drive| {
                    format!("{}\t{}", clean(&drive.remote_name), clean(&drive.label))
                }));
                Ok(rows.join("\n"))
            }
            "queue" => {
                let listing = self
                    .listings
                    .lock()
                    .unwrap()
                    .get(&request.value)
                    .cloned()
                    .context("Search for the game again.")?;
                let option = listing
                    .download_options
                    .get(request.index)
                    .cloned()
                    .context("Choose an available host.")?;
                let settings = configured_ftp()?.context("FTP details are missing.")?;
                let ftp = crate::RcloneFtp::new(&settings)?.connect_available()?;
                if !crate::detect_storage_drives(&ftp)?
                    .iter()
                    .any(|drive| drive.remote_name == request.drive)
                {
                    anyhow::bail!("The selected Xbox drive is no longer connected.");
                }
                let mut jobs = self.jobs.lock().unwrap();
                if jobs.iter().any(|job| {
                    job.listing.id == listing.id
                        && job.drive == request.drive
                        && matches!(job.status.as_str(), "queued" | "running")
                }) {
                    return Ok("Already processing. Keep your PC and Xbox on. Keep Digital 360 open until installation finishes.\n".into());
                }
                if jobs.iter().filter(|job| job.status != "complete").count() >= 20 {
                    anyhow::bail!("The queue is full (20 unfinished jobs).");
                }
                let id = format!(
                    "{}",
                    SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
                );
                jobs.push(Job {
                    id,
                    listing,
                    option,
                    drive: request.drive,
                    status: "queued".into(),
                    error: None,
                    notified: false,
                });
                self.persist(&jobs)?;
                Ok("Starting automatically. Keep your PC and Xbox on. Keep Digital 360 open until installation finishes.\n".into())
            }
            "jobs" => {
                let jobs = self.jobs.lock().unwrap();
                let mut rows = vec!["Downloads - select a failed job to retry".into()];
                for job in jobs.iter().rev().take(100) {
                    let progress = fs::read(self.root.join(&job.id).join("progress.json"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                    let state = if job.status == "running" {
                        progress
                            .map(|p| {
                                format!(
                                    "{}% {}",
                                    p["percent"],
                                    p["stage"].as_str().unwrap_or("Working")
                                )
                            })
                            .unwrap_or("Starting".into())
                    } else {
                        job.status.clone()
                    };
                    rows.push(format!(
                        "{}\t{} - {}",
                        job.id,
                        clean(&job.listing.title),
                        clean(&state)
                    ));
                }
                Ok(rows.join("\n"))
            }
            "retry" => {
                let mut jobs = self.jobs.lock().unwrap();
                let job = jobs
                    .iter_mut()
                    .find(|job| job.id == request.value)
                    .context("Job not found.")?;
                if job.status != "failed" {
                    anyhow::bail!("Only failed jobs need retrying.");
                }
                job.status = "queued".into();
                job.error = None;
                job.notified = false;
                let release = self.root.join(&job.id).join("dashboard-release.json");
                for extension in ["ready", "handled"] {
                    let marker = release.with_extension(extension);
                    if marker.exists() {
                        fs::remove_file(marker)?;
                    }
                }
                if release.exists() {
                    fs::remove_file(release)?;
                }
                self.persist(&jobs)?;
                Ok(
                    "Retry queued. Existing downloads will be reused and delivery verified.\n"
                        .into(),
                )
            }
            "credentials" => {
                if env::var_os("DIGITAL360_PC_FTP_SETTINGS").is_some() {
                    anyhow::bail!(
                        "Update your saved FTP details through Manual Import on your PC, then retry."
                    );
                }
                if !self
                    .jobs
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|job| !matches!(job.status.as_str(), "queued" | "running"))
                {
                    anyhow::bail!("Wait for active jobs before changing FTP credentials.");
                }
                let path = env::var_os("ISO2GOD_WORKER_CONSOLE")
                    .context("This console has not been paired by the host administrator.")?;
                let mut settings: ConsoleSettings = serde_json::from_slice(&fs::read(&path)?)?;
                if request.value.is_empty()
                    || request.password.is_empty()
                    || request.value.contains(['\r', '\n'])
                    || request.password.contains(['\r', '\n'])
                {
                    anyhow::bail!("Enter the dashboard FTP username and password.");
                }
                settings.username = request.value;
                settings.password = request.password;
                // Validate the supplied dashboard login itself: a successful
                // fallback to FTPdll must not save an incorrect dashboard password.
                let ftp = crate::FtpSettings {
                    host: settings.host.clone(),
                    port: settings.port,
                    background_port: None,
                    username: settings.username.clone(),
                    password: settings.password.clone(),
                    destination_path: crate::DEFAULT_FTP_DESTINATION.into(),
                };
                crate::RcloneFtp::new(&ftp)?.connect_available().map_err(|_| anyhow::anyhow!("FTP_LOGIN_REQUIRED: Login failed. Check your dashboard details and try again."))?;
                atomic_json(Path::new(&path), &settings)?;
                Ok("FTP details verified and saved.\n".into())
            }
            _ => anyhow::bail!("Unknown store action."),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleRequest {
    pub action: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub drive: String,
    #[serde(default)]
    pub password: String,
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

pub fn progress(stage: &str, percent: u8, detail: Option<&str>) {
    if let Some(path) = env::var_os("ISO2GOD_WORKER_PROGRESS") {
        let _ = atomic_json(
            Path::new(&path),
            &json!({"stage":stage,"percent":percent.min(100),"detail":detail}),
        );
    }
}

// Console installs keep Digital 360 open and use the installed FTP service.
pub fn delivery_connection(settings: &crate::FtpSettings) -> Result<crate::RcloneFtp, Error> {
    let ftp = crate::RcloneFtp::new(settings)?;
    if env::var_os("DIGITAL360_STAY_OPEN").is_none() {
        return ftp.connect_available();
    }
    let port = settings
        .background_port
        .context("Install the background FTP plugin before using Digital 360.")?;
    let endpoint = ftp.with_port(port)?;
    for _ in 0..180 {
        progress(
            "Connecting to Xbox FTP",
            0,
            Some("Keep Digital 360 open until installation finishes."),
        );
        if endpoint.test_connection().is_ok() {
            return Ok(endpoint);
        }
        progress(
            "Waiting for Xbox FTP",
            0,
            Some("Check the installed FTP plugin and network. Prepared files are retained."),
        );
        thread::sleep(Duration::from_secs(2));
    }
    anyhow::bail!(
        "Xbox FTP is unavailable. Check the installed FTP plugin and retry; prepared files are retained."
    )
}

pub fn run_job(path: &Path) -> i32 {
    let result = (|| -> Result<(), Error> {
        let job: Job = serde_json::from_slice(&fs::read(path)?)?;
        let mut log = crate::app_log::OperationLog::create("store_worker")?;
        crate::workflow::process_store_listing(
            &job.listing,
            crate::pipeline::JobDelivery::Ftp,
            job.option,
            &mut log,
        )
    })();
    match result {
        Ok(()) => {
            progress("Complete - transferred and verified", 100, None);
            0
        }
        Err(error) => {
            // Full diagnostics stay in the private host job folder, not the console UI.
            if let Some(parent) = path.parent() {
                let _ = fs::write(parent.join("failure.txt"), format!("{error:#}"));
            }
            progress(
                "Failed - files retained; reopen the Store to retry",
                0,
                crate::store::mega_download::failure_detail(&error),
            );
            1
        }
    }
}
