//! Headless app control. MCP owns the control pipe; workers own progress output.

#[cfg(test)]
mod tests {
    use super::*;

    fn local_request(source: &Path, destination: &Path) -> Request {
        serde_json::from_value(json!({
            "action": "process_games", "sources": [source],
            "destination": "local", "destination_path": destination
        }))
        .unwrap()
    }

    #[test]
    fn rejects_unsupported_actions_and_credentials_on_transfer_jobs() {
        assert!(serde_json::from_value::<Request>(json!({"action": "run_shell"})).is_err());
        let request: Request =
            serde_json::from_value(json!({"action":"check_updates","password":"secret"})).unwrap();
        assert!(validate(&request).is_err());
        assert!(
            serde_json::from_value::<Request>(json!({"action":"check_updates","unexpected":true}))
                .is_err()
        );
    }

    #[test]
    fn requires_explicit_paths_and_does_not_enable_source_deletion_by_default() {
        let request: Request = serde_json::from_value(json!({"action":"process_games"})).unwrap();
        assert!(!request.remove_archive_parts);
        assert!(validate(&request).is_err());
    }

    #[test]
    fn copies_prepared_game_without_changing_source_and_rejects_existing_destination() {
        let mut root = TempDir::new().unwrap();
        let source = root.path().join("input").join("Example Game");
        let destination = root.path().join("output");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(
            source.join("default.xex"),
            crate::tests::test_xex(0x4D53_0AB5),
        )
        .unwrap();
        fs::write(source.join("data.bin"), b"synthetic game content").unwrap();
        let request = local_request(&source, &destination);
        validate(&request).unwrap();
        let result = process_game(&request, &source).unwrap();
        assert_eq!(result["verified"], true);
        assert_eq!(result["removedArchiveParts"], 0);
        assert_eq!(
            fs::read(destination.join("Content/0000000000000000/4D530AB5/data.bin")).unwrap(),
            b"synthetic game content"
        );
        assert!(source.join("default.xex").is_file());
        assert!(process_game(&request, &source).is_err());
        root.cleanup = true;
    }

    #[test]
    fn rejects_source_destination_overlap() {
        let mut root = TempDir::new().unwrap();
        let destination = root.path().join("output");
        fs::create_dir(&destination).unwrap();
        assert!(validate(&local_request(root.path(), &destination)).is_err());
        assert!(validate(&local_request(&destination, root.path())).is_err());
        root.cleanup = true;
    }

    #[test]
    fn usb_job_never_silently_falls_back_to_a_local_directory() {
        let mut root = TempDir::new().unwrap();
        let source = root.path().join("input");
        let destination = root.path().join("output");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(source.join("default.xex"), b"test").unwrap();
        let mut request = local_request(&source, &destination);
        request.destination = Destination::Usb;
        assert!(process_game(&request, &source).is_err());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
        root.cleanup = true;
    }
}

use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Child;
use std::sync::OnceLock;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Action {
    ProcessGames,
    VerifyGame,
    ConfigureFtp,
    TestFtp,
    InstallArchiveTool,
    CheckUpdates,
    InstallUpdate,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Destination {
    #[default]
    Local,
    Usb,
    Ftp,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    action: Action,
    #[serde(default)]
    sources: Vec<PathBuf>,
    #[serde(default)]
    destination: Destination,
    destination_path: Option<PathBuf>,
    #[serde(default)]
    remove_archive_parts: bool,
    host: Option<String>,
    username: Option<String>,
    port: Option<u16>,
    password: Option<String>,
}

struct Job {
    child: Child,
    directory: PathBuf,
    secret: Option<String>,
}

static JOBS: OnceLock<Mutex<BTreeMap<String, Job>>> = OnceLock::new();

pub(super) fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "list_usb_drives",
            "description": "List currently detected removable USB drives. Use an exact returned root as destination_path for USB jobs.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true}
        }),
        json!({
            "name": "start_job",
            "description": "Run an explicitly user-authorized app operation in a background worker without terminal prompts. Returns a job ID, not completion. One job at a time per bridge. Poll job_status. process_games handles explicit ISO/archive/multipart/prepared-folder sources sequentially; source parts are preserved unless remove_archive_parts is true and delivery succeeds. local and usb require destination_path. configure_ftp requires host and password, tests then saves credentials. verify_game checks prepared folders structurally, not gameplay. check_updates does not install; install_update requires explicit update approval.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "action": {"type": "string", "enum": ["process_games", "verify_game", "configure_ftp", "test_ftp", "install_archive_tool", "check_updates", "install_update"]},
                    "sources": {"type": "array", "items": {"type": "string"}, "description": "Explicit user-approved inputs; a folder with multiple archive sets must be split into individual Part 1 inputs."},
                    "destination": {"type": "string", "enum": ["local", "usb", "ftp"], "default": "local"},
                    "destination_path": {"type": "string", "description": "Local output root or an exact root returned by list_usb_drives. Not used for FTP."},
                    "remove_archive_parts": {"type": "boolean", "default": false, "description": "Only true when the user explicitly requests permanent deletion of successfully delivered multipart source volumes."},
                    "host": {"type": "string"},
                    "username": {"type": "string", "default": "xbox"},
                    "port": {"type": "integer", "minimum": 1, "maximum": 65535, "default": 21},
                    "password": {"type": "string", "description": "For configure_ftp only; sent to worker over stdin, never stored in job request files or returned."}
                },
                "required": ["action"]
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": true}
        }),
        json!({
            "name": "job_status",
            "description": "Read a job's running/completed/failed state, structured result and last 16 KiB of progress output. Job IDs belong to the current bridge session.",
            "inputSchema": {"type": "object", "properties": {"job_id": {"type": "string"}}, "required": ["job_id"], "additionalProperties": false},
            "annotations": {"readOnlyHint": true}
        }),
    ]
}

pub(super) fn call_tool(name: &str, arguments: &Value) -> Result<Value, Error> {
    match name {
        "list_usb_drives" => Ok(json!({"drives": detect_removable_drives()?.iter().map(|d|
            json!({"root": portable_path_text(&d.root), "label": d.label, "freeBytes": d.free_bytes})
        ).collect::<Vec<_>>()})),
        "start_job" => start_job(serde_json::from_value(arguments.clone())?),
        "job_status" => job_status(
            arguments
                .get("job_id")
                .and_then(Value::as_str)
                .context("job_id is required")?,
        ),
        _ => anyhow::bail!("unknown AI tool: {name}"),
    }
}

fn validate(request: &Request) -> Result<(), Error> {
    if request.action != Action::ConfigureFtp
        && (request.password.is_some()
            || request.host.is_some()
            || request.username.is_some()
            || request.port.is_some())
    {
        anyhow::bail!(
            "FTP credentials belong only to configure_ftp; other jobs use saved settings"
        );
    }
    match request.action {
        Action::ProcessGames | Action::VerifyGame => {
            if request.sources.is_empty() {
                anyhow::bail!("sources must contain at least one explicitly selected input");
            }
            for path in &request.sources {
                if !path.is_absolute() || !path.exists() {
                    anyhow::bail!(
                        "source must be an existing absolute path: {}",
                        path.display()
                    );
                }
            }
            if request.action == Action::ProcessGames {
                match request.destination {
                    Destination::Ftp if request.destination_path.is_some() => anyhow::bail!(
                        "FTP destination is selected automatically; omit destination_path"
                    ),
                    Destination::Local | Destination::Usb => {
                        let path = request
                            .destination_path
                            .as_ref()
                            .context("destination_path is required for local/USB delivery")?;
                        if !path.is_absolute() || !path.is_dir() {
                            anyhow::bail!(
                                "destination_path must be an existing absolute directory"
                            );
                        }
                        let target = fs::canonicalize(path)?;
                        for source in &request.sources {
                            let source = fs::canonicalize(source)?;
                            if source.is_dir()
                                && (target.starts_with(&source) || source.starts_with(&target))
                            {
                                anyhow::bail!("source and destination folders must not overlap");
                            }
                        }
                    }
                    Destination::Ftp => {}
                }
            }
        }
        Action::ConfigureFtp => {
            normalise_ftp_host(request.host.as_deref().context("host is required")?)?;
            if request.password.as_deref().is_none_or(str::is_empty) {
                anyhow::bail!("password is required");
            }
            let username = request.username.as_deref().unwrap_or(DEFAULT_FTP_USERNAME);
            if username.trim().is_empty() || username.contains(['\r', '\n']) {
                anyhow::bail!("invalid FTP username");
            }
            if request
                .password
                .as_deref()
                .is_some_and(|s| s.contains(['\r', '\n']))
            {
                anyhow::bail!("password must not contain a line break");
            }
            if request.port == Some(0) {
                anyhow::bail!("port must be from 1 to 65535");
            }
        }
        _ => {}
    }
    if request.remove_archive_parts && request.action != Action::ProcessGames {
        anyhow::bail!("remove_archive_parts applies only to process_games");
    }
    Ok(())
}

fn start_job(request: Request) -> Result<Value, Error> {
    validate(&request)?;
    let mut jobs = JOBS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("job registry unavailable"))?;
    for job in jobs.values_mut() {
        if job.child.try_wait()?.is_none() {
            anyhow::bail!("a job is already running; wait for it to finish");
        }
    }
    let id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let directory = env::temp_dir().join(format!("iso2god-job-{id}"));
    fs::create_dir(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let log = File::create(directory.join("progress.log"))?;
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--automation-worker")
        .arg(directory.join("result.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW; no terminal UI.
    }
    let mut child = command
        .spawn()
        .context("could not start background worker")?;
    let write_request = (|| -> Result<(), Error> {
        let mut stdin = child.stdin.take().context("worker input unavailable")?;
        serde_json::to_writer(&mut stdin, &request)?;
        stdin.flush()?;
        Ok(())
    })();
    if let Err(error) = write_request {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    jobs.insert(
        id.clone(),
        Job {
            child,
            directory: directory.clone(),
            secret: request.password,
        },
    );
    Ok(
        json!({"job_id": id, "state": "running", "logDirectory": directory, "message": "Poll job_status for actual completion."}),
    )
}

fn job_status(id: &str) -> Result<Value, Error> {
    let mut jobs = JOBS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("job registry unavailable"))?;
    let job = jobs
        .get_mut(id)
        .context("unknown job ID for this bridge session")?;
    let exit = job.child.try_wait()?;
    let mut log = File::open(job.directory.join("progress.log"))?;
    let length = log.metadata()?.len();
    log.seek(SeekFrom::Start(length.saturating_sub(16 * 1024)))?;
    let mut bytes = Vec::new();
    log.take(16 * 1024).read_to_end(&mut bytes)?;
    let mut tail = String::from_utf8_lossy(&bytes).into_owned();
    if let Some(secret) = job.secret.as_ref().filter(|s| !s.is_empty()) {
        tail = tail.replace(secret, "[REDACTED]");
    }
    let result = if exit.is_some() {
        fs::read(job.directory.join("result.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    } else {
        None
    };
    let state = match exit {
        None => "running",
        Some(status)
            if status.success() && result.as_ref().is_some_and(|r| r["success"] == true) =>
        {
            "completed"
        }
        _ => "failed",
    };
    Ok(
        json!({"job_id": id, "state": state, "result": result, "progress": tail, "logDirectory": job.directory}),
    )
}

pub(super) fn run_worker(result_path: &Path) -> i32 {
    let mut input = String::new();
    let request = io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut input)
        .map_err(Error::from)
        .and_then(|_| serde_json::from_str::<Request>(&input).map_err(Error::from));
    let result = match request.as_ref() {
        Ok(request) => validate(request).and_then(|_| execute(request)),
        Err(error) => Err(anyhow::anyhow!("invalid worker request: {error}")),
    };
    let succeeded = result.is_ok();
    let output = match result {
        Ok(value) => json!({"success": true, "output": value}),
        Err(error) => {
            let mut message = format!("{error:#}");
            if let Ok(request) = &request
                && let Some(secret) = request.password.as_ref().filter(|s| !s.is_empty())
            {
                message = message.replace(secret, "[REDACTED]");
            }
            json!({"success": false, "error": message})
        }
    };
    let write_result = (|| -> Result<(), Error> {
        let pending = result_path.with_extension("pending");
        fs::write(&pending, serde_json::to_vec_pretty(&output)?)?;
        fs::rename(pending, result_path)?;
        Ok(())
    })();
    if let Err(error) = write_result {
        eprintln!("Could not write job result: {error}");
        return 1;
    }
    if succeeded { 0 } else { 1 }
}

fn saved_ftp() -> Result<FtpSettings, Error> {
    let mut settings =
        load_ftp_settings()?.context("FTP is not configured; use configure_ftp first")?;
    settings.password = load_saved_password()?.context("saved FTP password is unavailable")?;
    Ok(settings)
}

fn execute(request: &Request) -> Result<Value, Error> {
    // A filesystem lock also prevents two different AI clients from mutating the app concurrently.
    let lock_dir = ftp_settings_path()?
        .parent()
        .context("settings directory unavailable")?
        .to_path_buf();
    fs::create_dir_all(&lock_dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_dir.join("automation.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("another automation worker is active; retry after it finishes")?;
    match request.action {
        Action::ProcessGames => {
            if request.destination == Destination::Ftp {
                RcloneFtp::new(&saved_ftp()?)?.connect_available()?;
            }
            let mut completed = Vec::new();
            for source in &request.sources {
                println!("Preparing {}", source.display());
                match process_game(request, source) {
                    Ok(value) => completed.push(value),
                    Err(error) => anyhow::bail!(
                        "Stopped at {}: {error:#}. Previously completed outputs: {}",
                        source.display(),
                        serde_json::to_string(&completed)?
                    ),
                }
            }
            Ok(json!({"games": completed}))
        }
        Action::VerifyGame => {
            let mut verified = Vec::new();
            for source in &request.sources {
                let game = existing_game(source)?;
                verify_prepared_game(&game)?;
                verified.push(json!({"source": source, "verified": true}));
            }
            Ok(
                json!({"games": verified, "verification": "structure and required files; not gameplay"}),
            )
        }
        Action::ConfigureFtp => {
            let settings = FtpSettings {
                host: normalise_ftp_host(request.host.as_deref().context("host required")?)?,
                username: request
                    .username
                    .as_deref()
                    .unwrap_or(DEFAULT_FTP_USERNAME)
                    .trim()
                    .to_owned(),
                port: request.port.unwrap_or(DEFAULT_FTP_PORT),
                background_port: Some(DEFAULT_BACKGROUND_FTP_PORT),
                password: request.password.clone().context("password required")?,
                destination_path: DEFAULT_FTP_DESTINATION.to_owned(),
            };
            RcloneFtp::new(&settings)?.connect_available()?;
            save_password(&settings.username, &settings.password)?;
            save_ftp_settings(&settings)?;
            Ok(json!({
                "configured": true,
                "connectionTested": true,
                "xboxPluginInstalled": false,
                "restartRequired": false
            }))
        }
        Action::TestFtp => {
            RcloneFtp::new(&saved_ftp()?)?.connect_available()?;
            Ok(json!({"connected": true}))
        }
        Action::InstallArchiveTool => {
            if let Some(path) = find_7zip() {
                return Ok(json!({"installed": true, "path": path}));
            }
            let path = install_7zip_automatically()?;
            Ok(json!({"installed": true, "path": path}))
        }
        Action::CheckUpdates => Ok(
            json!({"configured": update_repository().is_some(), "availableVersion": check_for_automatic_update()?.map(|u| u.version)}),
        ),
        Action::InstallUpdate => {
            let update = check_for_automatic_update()?
                .context("no newer release is available or updates are not configured")?;
            install_automatic_update(&update)?;
            Ok(json!({"installedVersion": update.version, "restartRequired": true}))
        }
    }
}

fn existing_game(source: &Path) -> Result<PreparedGame, Error> {
    match detect_extracted_xbox_game(source)? {
        DetectedXboxGame::God {
            title_dir,
            title_id,
        } => Ok(PreparedGame {
            root: source.to_path_buf(),
            name: title_id.clone(),
            kind: PreparedGameKind::God {
                title_dir,
                title_id,
            },
        }),
        DetectedXboxGame::Jtag(root) => {
            let title_id = read_prepared_jtag_title_id(&root)?;
            Ok(PreparedGame {
                name: root
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                root,
                kind: PreparedGameKind::Jtag { title_id },
            })
        }
        DetectedXboxGame::Iso(_) => anyhow::bail!(
            "verify_game expects a prepared GOD or default.xex folder; prepare the ISO first"
        ),
    }
}

fn copy_local(source: &Path, destination: &Path) -> Result<(), Error> {
    if destination.exists() {
        anyhow::bail!("destination already exists: {}", destination.display());
    }
    let total = collect_upload_files(source)?
        .iter()
        .map(|f| f.size)
        .sum::<u64>();
    let mut ancestor = destination
        .parent()
        .context("destination parent unavailable")?;
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .context("destination has no existing parent")?;
    }
    if fs2::available_space(ancestor)? < total {
        anyhow::bail!("not enough free space for local copy");
    }
    copy_directory_and_verify(source, destination, "Copying game to local storage")
}

fn process_game(request: &Request, source: &Path) -> Result<Value, Error> {
    let usb = if request.destination == Destination::Usb {
        let requested = fs::canonicalize(
            request
                .destination_path
                .as_ref()
                .context("USB root required")?,
        )?;
        Some(
            detect_removable_drives()?
                .into_iter()
                .find(|d| fs::canonicalize(&d.root).is_ok_and(|p| p == requested))
                .context("selected USB drive is no longer available")?,
        )
    } else {
        None
    };
    let mut workspace = TempDir::new()?;
    let mut removed_parts = Vec::new();
    let prepared = if source.is_file()
        && source
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
    {
        let destination = workspace.path().join("converted");
        let output = run(Cli {
            source_iso: source.to_path_buf(),
            dest_dir: destination.clone(),
            dry_run: false,
            game_title: None,
            trim: None,
            num_threads: 1,
            upload_ftp: false,
            copy_usb: false,
            use_saved_ftp: false,
            prompt_delivery: false,
            selected_usb_drive: None,
        })?;
        verify_converted_game(&output)?;
        PreparedGame {
            root: destination,
            name: source
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            kind: PreparedGameKind::God {
                title_dir: output.local_title_dir,
                title_id: output.title_id,
            },
        }
    } else if source.is_dir() && existing_game(source).is_ok() {
        let original = existing_game(source)?;
        verify_prepared_game(&original)?;
        let staging = workspace
            .path()
            .join("input")
            .join(sanitize_game_name(&original.name));
        copy_local(source, &staging)?;
        let detected = detect_extracted_xbox_game(&staging)?;
        prepare_detected_game(workspace.path(), &original.name, detected)?
    } else {
        let (_, sets, _) = multipart_sets_for_dropped_input(source)?;
        if sets.len() != 1 {
            anyhow::bail!(
                "supply one archive set per source; use scan_game_folder and select individual Part 1 inputs"
            );
        }
        let set = &sets[0];
        if let Some(missing) = &set.missing_part {
            anyhow::bail!("missing archive part: {missing}");
        }
        let seven_zip =
            find_7zip().context("7-Zip is required; install it before starting this job")?;
        let unpacked = list_archive_unpacked_size(&seven_zip, &set.entry_path)?;
        let required = unpacked.saturating_mul(2).saturating_add(512 * 1024 * 1024);
        if fs2::available_space(workspace.path())? < required {
            anyhow::bail!("not enough temporary storage for extraction and preparation");
        }
        let staging = workspace.path().join("extracted");
        fs::create_dir(&staging)?;
        let progress = ProgressDisplay::new();
        if !extract_with_7zip(&seven_zip, &set.entry_path, &staging, &progress)?.success() {
            anyhow::bail!("archive extraction failed; originals preserved");
        }
        progress.finish("Extraction complete");
        verify_nonempty_directory(&staging)?;
        let detected = detect_xbox_game_with_nested_archives(&staging, &seven_zip)?;
        let prepared = prepare_detected_game(workspace.path(), &set.game_name, detected)?;
        if request.remove_archive_parts && set.parts.len() > 1 {
            removed_parts = set.parts.clone();
        }
        prepared
    };
    verify_prepared_game(&prepared)?;
    let (files, relative) = prepared_source_and_relative_destination(&prepared);
    let output = match request.destination {
        Destination::Local => {
            let target = request
                .destination_path
                .as_ref()
                .context("local destination required")?
                .join(&relative);
            copy_local(files, &target)?;
            portable_path_text(&target)
        }
        Destination::Usb => {
            let drive = usb.as_ref().context("USB drive unavailable")?;
            copy_prepared_game_to_selected_drive(&prepared, drive)?;
            portable_path_text(&drive.root.join(&relative))
        }
        Destination::Ftp => {
            upload_prepared_game_to_xbox(&prepared)?;
            format!("/Hdd1/{}", relative.to_string_lossy().replace('\\', "/"))
        }
    };
    // Background jobs delete only explicitly requested source volumes, after delivery verifies.
    if !removed_parts.is_empty() {
        let parent = removed_parts[0]
            .parent()
            .context("archive source directory unavailable")?;
        remove_verified_archive_parts(parent, &removed_parts)?;
    }
    workspace.cleanup = true;
    Ok(
        json!({"source": source, "destination": output, "verified": true,
        "removedArchiveParts": removed_parts.len(), "verification": "file presence and sizes; not gameplay"}),
    )
}
