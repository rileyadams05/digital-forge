use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Error, bail, ensure};
use fs2::{available_space, total_space};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::backend::{ConversionResult, ProgressUpdate};
use super::{Platform, TargetFormat};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    pub remote_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TransferDestination {
    LocalFolder(PathBuf),
    RemovableDrive {
        drive_letter: String,
        target_root: PathBuf,
    },
    Ftp(FtpConfig),
}

pub struct TransferManager;

/// Listen for a one-way TCP stream from the console and receive it into a new
/// file. The caller supplies the local bind address and port because network
/// interfaces and transfer ports vary by setup. Data is staged beside the
/// target and becomes visible only after the peer closes the stream cleanly.
pub fn receive_tcp_file(
    listen_address: SocketAddr,
    destination: &Path,
    progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
    cancelled: Arc<AtomicBool>,
) -> Result<u64, Error> {
    let parent = destination
        .parent()
        .context("receive destination has no parent directory")?;
    ensure!(
        destination.file_name().is_some(),
        "receive destination must name a file"
    );
    fs::create_dir_all(parent)?;
    ensure!(
        fs::symlink_metadata(destination).is_err(),
        "receive destination already exists: {}",
        destination.display()
    );
    let file_name = destination.file_name().unwrap().to_string_lossy();
    let partial = parent.join(format!(".{file_name}.digital-forge-partial"));
    ensure!(
        fs::symlink_metadata(&partial).is_err(),
        "an incomplete receive file already exists: {}",
        partial.display()
    );

    let listener = TcpListener::bind(listen_address)
        .with_context(|| format!("could not listen for PS5 transfer on {listen_address}"))?;
    listener.set_nonblocking(true)?;
    let mut stream = loop {
        ensure!(!cancelled.load(Ordering::Relaxed), "PS5 transfer cancelled");
        match listener.accept() {
            Ok((stream, _peer)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                progress(ProgressUpdate {
                    stage: "Waiting for PS5 game dump".to_owned(),
                    percent: 0,
                    details: Some(format!("Listening on {listen_address}; Alt+C cancels")),
                    bytes_processed: 0,
                    total_bytes: 0,
                });
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error).context("could not accept PS5 data transfer"),
        }
    };
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let receive_result = (|| -> Result<u64, Error> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)?;
        let mut buffer = [0u8; 1024 * 1024];
        let mut received = 0u64;
        loop {
            ensure!(!cancelled.load(Ordering::Relaxed), "PS5 transfer cancelled");
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    file.write_all(&buffer[..count])?;
                    received = received.saturating_add(count as u64);
                    progress(ProgressUpdate {
                        stage: "Receiving PS5 game dump".to_owned(),
                        percent: 0,
                        details: Some(format!("{received} bytes received")),
                        bytes_processed: received,
                        total_bytes: 0,
                    });
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    progress(ProgressUpdate {
                        stage: "Waiting for PS5 game dump".to_owned(),
                        percent: 0,
                        details: Some(format!("{received} bytes received; Alt+C cancels")),
                        bytes_processed: received,
                        total_bytes: 0,
                    });
                    continue;
                }
                Err(error) => return Err(error).context("PS5 data transfer was interrupted"),
            }
        }
        ensure!(
            received > 0,
            "PS5 transfer ended without receiving any data"
        );
        file.sync_all()?;
        Ok(received)
    })();
    let received = match receive_result {
        Ok(received) => received,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
    };
    // Hard-link creation is atomic and fails if another process created the
    // destination while the transfer was in progress; unlike rename it never
    // replaces an existing destination on Unix-like systems.
    if let Err(error) = fs::hard_link(&partial, destination) {
        let _ = fs::remove_file(&partial);
        return Err(error).with_context(|| {
            format!("could not finalize received file {}", destination.display())
        });
    }
    fs::remove_file(&partial)?;
    progress(ProgressUpdate {
        stage: "PS5 transfer complete".to_owned(),
        percent: 100,
        details: Some(destination.display().to_string()),
        bytes_processed: received,
        total_bytes: received,
    });
    Ok(received)
}

impl TransferManager {
    pub fn transfer(
        result: &ConversionResult,
        destination: &TransferDestination,
        progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<PathBuf, Error> {
        match destination {
            TransferDestination::LocalFolder(target_dir) => {
                Self::transfer_local(result, target_dir, progress, cancelled)
            }
            TransferDestination::RemovableDrive { target_root, .. } => {
                let target_root = prepare_removable_root(target_root, result)?;
                let platform_dir = removable_platform_dir(&target_root, result);
                fs::create_dir_all(&platform_dir)?;
                let delivered = Self::transfer_local(result, &platform_dir, progress, cancelled)?;
                verify_delivered_tree(&result.output_path, &delivered)?;
                println!(
                    "External-drive copy verified. Use Windows 'Eject' before disconnecting the drive."
                );
                Ok(delivered)
            }
            TransferDestination::Ftp(ftp_config) => {
                Self::transfer_ftp(result, ftp_config, progress, cancelled)
            }
        }
    }

    pub fn transfer_local(
        result: &ConversionResult,
        target_dir: &Path,
        progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<PathBuf, Error> {
        fs::create_dir_all(target_dir)?;
        let item_name = resolve_item_name(result);

        let destination = if result.output_path.is_dir() {
            let sfo1 = result.output_path.join("sce_sys").join("param.sfo");
            let sfo2 = result.output_path.join("param.sfo");
            let sfo_cand = if sfo1.is_file() { sfo1 } else { sfo2 };
            let is_ps4_dlc = sfo_cand.is_file()
                && crate::forge::detector::FormatDetector::read_sfo_category(&sfo_cand).as_deref()
                    == Some("ac");
            if is_ps4_dlc {
                bail!(
                    "PS4 DLC cannot be delivered as a nested loose-game folder; choose a target with a documented separate add-on workflow"
                );
            }
            target_dir.join(&item_name)
        } else {
            target_dir.join(&item_name)
        };

        let total_bytes = result.total_size.max(1);
        progress(ProgressUpdate {
            stage: "Saving verified output to destination".to_string(),
            percent: 5,
            details: Some(destination.display().to_string()),
            bytes_processed: 0,
            total_bytes,
        });

        if result.output_path.is_dir() {
            let partial = partial_destination_path(&destination);
            let backup = backup_destination_path(&destination);
            ensure!(
                !partial.exists(),
                "an incomplete Digital Forge destination already exists; inspect or move it before retrying: {}",
                partial.display()
            );
            ensure!(
                !backup.exists(),
                "a previous Digital Forge safety backup exists; inspect it before retrying: {}",
                backup.display()
            );
            ensure!(
                !destination.exists() || destination.is_dir(),
                "cannot safely replace non-directory destination {}",
                destination.display()
            );
            let copied = (|| -> Result<(), Error> {
                copy_dir_recursive(
                    &result.output_path,
                    &partial,
                    progress.clone(),
                    cancelled.clone(),
                )?;
                verify_delivered_tree(&result.output_path, &partial)?;
                let replacing = destination.exists();
                if replacing {
                    fs::rename(&destination, &backup).with_context(|| {
                        format!(
                            "could not preserve existing destination {}",
                            destination.display()
                        )
                    })?;
                }
                if let Err(error) = fs::rename(&partial, &destination) {
                    if replacing {
                        let _ = fs::rename(&backup, &destination);
                    }
                    return Err(error).with_context(|| {
                        format!(
                            "could not finalize verified destination {}",
                            destination.display()
                        )
                    });
                }
                if let Err(error) = verify_delivered_tree(&result.output_path, &destination) {
                    if replacing {
                        let failed = partial_destination_path(&destination);
                        let _ = fs::rename(&destination, &failed);
                        let _ = fs::rename(&backup, &destination);
                    }
                    return Err(error).context(
                        "finalized destination failed verification; the original was restored",
                    );
                }
                if replacing {
                    fs::remove_dir_all(&backup).with_context(|| {
                        format!(
                            "replacement succeeded but the safety backup could not be removed: {}",
                            backup.display()
                        )
                    })?;
                }
                Ok(())
            })();
            if copied.is_err() && partial.exists() {
                let _ = fs::remove_dir_all(&partial);
            }
            copied?;
        } else {
            ensure!(
                !destination.exists(),
                "destination already contains this file; existing data was left untouched: {}",
                destination.display()
            );
            copy_file_with_progress(
                &result.output_path,
                &destination,
                total_bytes,
                progress.clone(),
                cancelled.clone(),
            )?;
        }
        verify_delivered_tree(&result.output_path, &destination)?;

        // Copy any auxiliary files
        for aux in &result.aux_files {
            if let Some(aux_name) = aux.file_name() {
                let aux_dest = target_dir.join(aux_name);
                ensure!(
                    !aux_dest.exists(),
                    "destination already contains auxiliary output; existing data was left untouched: {}",
                    aux_dest.display()
                );
                if aux.is_dir() {
                    let partial = partial_destination_path(&aux_dest);
                    ensure!(
                        !partial.exists(),
                        "an incomplete auxiliary destination already exists: {}",
                        partial.display()
                    );
                    copy_dir_recursive(aux, &partial, progress.clone(), cancelled.clone())?;
                    verify_delivered_tree(aux, &partial)?;
                    fs::rename(&partial, &aux_dest)?;
                } else {
                    copy_file_with_progress(
                        aux,
                        &aux_dest,
                        fs::metadata(aux)?.len(),
                        progress.clone(),
                        cancelled.clone(),
                    )?;
                }
                verify_delivered_tree(aux, &aux_dest)?;
            }
        }

        progress(ProgressUpdate {
            stage: "Saved and verified successfully".to_string(),
            percent: 100,
            details: Some(destination.display().to_string()),
            bytes_processed: total_bytes,
            total_bytes,
        });

        Ok(destination)
    }

    pub fn transfer_ftp(
        result: &ConversionResult,
        ftp: &FtpConfig,
        progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<PathBuf, Error> {
        let item_name = resolve_item_name(result);
        let remote_base = format!("{}/{}", ftp.remote_path.trim_end_matches('/'), item_name);

        let total_bytes = result.total_size.max(1);
        progress(ProgressUpdate {
            stage: "Preparing FTP transfer to console".to_string(),
            percent: 5,
            details: Some(format!("ftp://{}:{}{}", ftp.host, ftp.port, remote_base)),
            bytes_processed: 0,
            total_bytes,
        });

        let mut upload_items: Vec<(PathBuf, String, u64)> = Vec::new();

        if result.output_path.is_file() {
            let size = fs::metadata(&result.output_path)?.len();
            let remote_file_url = format_ftp_url(&ftp.host, ftp.port, &remote_base);
            upload_items.push((result.output_path.clone(), remote_file_url, size));
        } else if result.output_path.is_dir() {
            for entry in walkdir_recursive(&result.output_path)? {
                let size = fs::metadata(&entry)?.len();
                let rel = entry.strip_prefix(&result.output_path)?;
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                let remote_file_path = format!("{}/{}", remote_base.trim_end_matches('/'), rel_str);
                let remote_file_url = format_ftp_url(&ftp.host, ftp.port, &remote_file_path);
                upload_items.push((entry, remote_file_url, size));
            }
        }

        for aux in &result.aux_files {
            if let Some(aux_name) = aux.file_name() {
                let aux_name_str = aux_name.to_string_lossy();
                let aux_remote_base =
                    format!("{}/{}", ftp.remote_path.trim_end_matches('/'), aux_name_str);
                if aux.is_file() {
                    let size = fs::metadata(aux)?.len();
                    let remote_file_url = format_ftp_url(&ftp.host, ftp.port, &aux_remote_base);
                    upload_items.push((aux.clone(), remote_file_url, size));
                } else if aux.is_dir() {
                    for entry in walkdir_recursive(aux)? {
                        let size = fs::metadata(&entry)?.len();
                        let rel = entry.strip_prefix(aux)?;
                        let rel_str = rel.to_string_lossy().replace('\\', "/");
                        let remote_file_path =
                            format!("{}/{}", aux_remote_base.trim_end_matches('/'), rel_str);
                        let remote_file_url =
                            format_ftp_url(&ftp.host, ftp.port, &remote_file_path);
                        upload_items.push((entry, remote_file_url, size));
                    }
                }
            }
        }

        let mut uploaded_bytes = 0u64;
        let auth = match &ftp.password {
            Some(p) if !p.is_empty() => format!("{}:{}", ftp.username, p),
            _ => ftp.username.clone(),
        };

        for (local_file, remote_url, size) in upload_items {
            if cancelled.load(Ordering::Relaxed) {
                bail!("FTP transfer cancelled");
            }

            let file_name = local_file.file_name().unwrap_or_default().to_string_lossy();
            let pct = (uploaded_bytes * 100 / total_bytes).min(99) as u8;
            progress(ProgressUpdate {
                stage: "Uploading to console via FTP".to_string(),
                percent: pct,
                details: Some(format!(
                    "{} ({:.1} MB / {:.1} MB)",
                    file_name,
                    uploaded_bytes as f64 / 1_000_000.0,
                    total_bytes as f64 / 1_000_000.0
                )),
                bytes_processed: uploaded_bytes,
                total_bytes,
            });

            let mut cmd = std::process::Command::new("curl");
            cmd.args(["-s", "-S", "--ftp-create-dirs", "-T"]);
            cmd.arg(&local_file);
            cmd.args(["--user", &auth, &remote_url]);

            let output = cmd
                .output()
                .context("Failed to execute curl for FTP transfer")?;
            if !output.status.success() {
                let err_msg = String::from_utf8_lossy(&output.stderr);
                bail!("FTP upload failed for {}: {}", file_name, err_msg.trim());
            }

            uploaded_bytes += size;
        }

        progress(ProgressUpdate {
            stage: "FTP transfer completed successfully".to_string(),
            percent: 100,
            details: Some(format!("ftp://{}:{}{}", ftp.host, ftp.port, remote_base)),
            bytes_processed: total_bytes,
            total_bytes,
        });

        Ok(PathBuf::from(format!(
            "ftp://{}:{}{}",
            ftp.host, ftp.port, remote_base
        )))
    }
}

fn prepare_removable_root(path: &Path, result: &ConversionResult) -> Result<PathBuf, Error> {
    ensure_removable_path_is_safe(path)?;
    let root = path.canonicalize().with_context(|| {
        format!(
            "removable-drive destination {} does not exist",
            path.display()
        )
    })?;
    ensure_removable_path_is_safe(&root)?;
    let required = result.total_size
        + result
            .aux_files
            .iter()
            .map(|path| tree_size(path))
            .sum::<u64>();
    let free = available_space(&root)?;
    let total = total_space(&root)?;
    if free < required {
        bail!(
            "external drive has insufficient free space: requires {:.2} GB, available {:.2} GB",
            required as f64 / 1_000_000_000.0,
            free as f64 / 1_000_000_000.0
        );
    }
    let label = volume_label(&root).unwrap_or_else(|| "Unlabelled volume".to_owned());
    let dest_dir = removable_platform_dir(&root, result);
    let item_name = resolve_item_name(result);
    let final_dest = dest_dir.join(&item_name);

    println!("\nDrive:          {}", root.display());
    println!("Label:          {}", label);
    println!("Capacity:       {:.2} GB", total as f64 / 1_000_000_000.0);
    println!("Free space:     {:.2} GB", free as f64 / 1_000_000_000.0);
    println!(
        "Required space: {:.2} GB",
        required as f64 / 1_000_000_000.0
    );
    println!("Destination:    {}", final_dest.display());
    Ok(root)
}

fn removable_platform_dir(root: &Path, result: &ConversionResult) -> PathBuf {
    if result.platform == Platform::PlayStation4 && result.target_format == TargetFormat::Ps4Folder
    {
        let is_games_root = root
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("games"));
        if is_games_root {
            root.join("PS4")
        } else {
            root.join("GAMES").join("PS4")
        }
    } else {
        root.join("Digital Forge")
            .join(result.platform.folder_name())
    }
}

/// Return the number of directories between a removable-drive mount root and
/// an ItemzFlow application. The application directory itself counts as one.
pub fn itemzflow_scan_depth(mount_root: &Path, application: &Path) -> Result<usize, Error> {
    let mount_root = mount_root
        .canonicalize()
        .with_context(|| format!("could not resolve drive root {}", mount_root.display()))?;
    let application = application.canonicalize().with_context(|| {
        format!(
            "could not resolve ItemzFlow application {}",
            application.display()
        )
    })?;
    let relative = application.strip_prefix(&mount_root).with_context(|| {
        format!(
            "ItemzFlow application {} is outside drive root {}",
            application.display(),
            mount_root.display()
        )
    })?;
    Ok(relative.components().count())
}

#[cfg(windows)]
pub fn volume_root(path: &Path) -> Result<PathBuf, Error> {
    use std::path::{Component, Prefix};

    let canonical = path.canonicalize()?;
    let mut components = canonical.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix)) => prefix,
        _ => bail!("{} is not on a Windows volume", canonical.display()),
    };
    ensure!(
        matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)),
        "{} is not on a drive-letter volume",
        canonical.display()
    );
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push("\\");
    Ok(root)
}

#[cfg(not(windows))]
pub fn volume_root(path: &Path) -> Result<PathBuf, Error> {
    let canonical = path.canonicalize()?;
    Ok(canonical
        .ancestors()
        .last()
        .unwrap_or(&canonical)
        .to_path_buf())
}

fn ensure_removable_path_is_safe(path: &Path) -> Result<(), Error> {
    ensure!(
        path.is_absolute(),
        "external-drive destination must be an absolute path"
    );
    ensure!(path.exists(), "external-drive destination does not exist");
    ensure!(
        path.is_dir(),
        "external-drive destination is not a directory"
    );
    Ok(())
}

fn verify_delivered_tree(source: &Path, destination: &Path) -> Result<(), Error> {
    if source.is_file() {
        ensure_same_size(source, destination)?;
        return Ok(());
    }
    let source_files = walkdir_recursive(source)?;
    let destination_files = walkdir_recursive(destination)?;
    ensure!(
        source_files.len() == destination_files.len(),
        "copy verification found an unexpected file count: source has {}, destination has {}",
        source_files.len(),
        destination_files.len()
    );
    for source_file in source_files {
        let relative = source_file.strip_prefix(source)?;
        let destination_file = destination.join(relative);
        ensure!(
            destination_file.is_file(),
            "external-drive verification found a missing file: {}",
            destination_file.display()
        );
        ensure_same_size(&source_file, &destination_file)?;
        if is_critical_ps4_file(relative) {
            ensure_same_sha256(&source_file, &destination_file)?;
        }
    }
    Ok(())
}

fn is_critical_ps4_file(relative: &Path) -> bool {
    let normalized = relative.to_string_lossy().replace('\\', "/");
    normalized.eq_ignore_ascii_case("eboot.bin")
        || normalized.eq_ignore_ascii_case("sce_sys/param.sfo")
        || relative.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("prx") || extension.eq_ignore_ascii_case("sprx")
        })
}

fn ensure_same_sha256(source: &Path, destination: &Path) -> Result<(), Error> {
    let source_hash = sha256_file(source)?;
    let destination_hash = sha256_file(destination)?;
    ensure!(
        source_hash == destination_hash,
        "SHA-256 verification failed for {}",
        destination.display()
    );
    Ok(())
}

fn sha256_file(path: &Path) -> Result<[u8; 32], Error> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 4 * 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

fn ensure_same_size(source: &Path, destination: &Path) -> Result<(), Error> {
    let expected = fs::metadata(source)?.len();
    let actual = fs::metadata(destination)
        .with_context(|| format!("could not verify {}", destination.display()))?
        .len();
    ensure!(
        expected == actual,
        "copy verification failed for {}: expected {expected} bytes, found {actual}",
        destination.display()
    );
    Ok(())
}

fn tree_size(path: &Path) -> u64 {
    if path.is_file() {
        return fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
    }
    walkdir_recursive(path)
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| fs::metadata(entry).ok())
        .map(|metadata| metadata.len())
        .sum()
}

#[cfg(windows)]
pub(crate) fn volume_label(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;

    let volume_root = path.ancestors().last()?;
    let mut root = volume_root.as_os_str().encode_wide().collect::<Vec<_>>();
    root.push(0);
    let mut label = [0u16; 261];
    let success = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    };
    if success == 0 {
        return None;
    }
    let length = label
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(label.len());
    let value = String::from_utf16_lossy(&label[..length]);
    (!value.is_empty()).then_some(value)
}

#[cfg(not(windows))]
pub(crate) fn volume_label(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

fn format_ftp_url(host: &str, port: u16, remote_path: &str) -> String {
    let clean_path = remote_path.trim_start_matches('/');
    format!("ftp://{}:{}/{}", host, port, clean_path)
}

fn walkdir_recursive(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut files = Vec::new();
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir_recursive(&path)?);
            } else {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn copy_file_with_progress(
    src: &Path,
    dst: &Path,
    total_bytes: u64,
    progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Error> {
    if src.canonicalize().ok() == dst.canonicalize().ok() && dst.exists() {
        return Ok(());
    }

    let partial = partial_destination_path(dst);
    let _ = fs::remove_file(&partial);
    let source_length = fs::metadata(src)?.len();
    let result = (|| -> Result<(), Error> {
        let mut in_file = File::open(src)?;
        let mut out_file = File::create(&partial)?;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let mut copied = 0u64;

        loop {
            if cancelled.load(Ordering::Relaxed) {
                bail!("operation cancelled with Alt+C");
            }
            let n = in_file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out_file.write_all(&buf[..n])?;
            copied += n as u64;

            let pct = (copied * 100 / total_bytes.max(1)).min(100) as u8;
            progress(ProgressUpdate {
                stage: "Writing file to destination".to_string(),
                percent: pct,
                details: Some(format!(
                    "{:.1} GB / {:.1} GB",
                    copied as f64 / 1_000_000_000.0,
                    source_length as f64 / 1_000_000_000.0
                )),
                bytes_processed: copied,
                total_bytes,
            });
        }

        out_file.flush()?;
        out_file.sync_all()?;
        let written = fs::metadata(&partial)?.len();
        if written != source_length {
            bail!(
                "destination size verification failed: expected {source_length} bytes, wrote {written}"
            );
        }
        Ok(())
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    if dst.exists() {
        fs::remove_file(dst)?;
    }
    if let Err(error) = fs::rename(&partial, dst) {
        let _ = fs::remove_file(&partial);
        return Err(error.into());
    }
    Ok(())
}

fn partial_destination_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    destination.with_file_name(format!("{name}.digital-forge-partial"))
}

fn backup_destination_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    destination.with_file_name(format!("{name}.digital-forge-backup"))
}

fn copy_dir_recursive(
    src: &Path,
    dst: &Path,
    progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
    cancelled: Arc<AtomicBool>,
) -> Result<(), Error> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        if cancelled.load(Ordering::Relaxed) {
            bail!("operation cancelled");
        }
        let path = entry.path();
        let file_name = entry.file_name();
        let target = dst.join(&file_name);
        if path.is_dir() {
            copy_dir_recursive(&path, &target, progress.clone(), cancelled.clone())?;
        } else {
            let file_name_str = file_name.to_string_lossy();
            if file_name_str.eq_ignore_ascii_case("param.sfo") && target.exists() {
                let src_cat = crate::forge::detector::FormatDetector::read_sfo_category(&path);
                let dst_cat = crate::forge::detector::FormatDetector::read_sfo_category(&target);
                if src_cat.as_deref() == Some("ac") && dst_cat.as_deref() != Some("ac") {
                    // Do not let DLC addon param.sfo overwrite base game/patch param.sfo
                    let dlc_sfo = dst.join("param_addon.sfo");
                    let _ = fs::copy(&path, &dlc_sfo);
                    continue;
                }
            }
            if (file_name_str.eq_ignore_ascii_case("icon0.png")
                || file_name_str.eq_ignore_ascii_case("icon0.dds")
                || file_name_str.eq_ignore_ascii_case("pic0.png")
                || file_name_str.eq_ignore_ascii_case("pic1.png")
                || file_name_str.eq_ignore_ascii_case("snd0.at9"))
                && target.exists()
            {
                if let Some(parent) = path.parent() {
                    let neighbor_sfo = parent.join("param.sfo");
                    let src_cat =
                        crate::forge::detector::FormatDetector::read_sfo_category(&neighbor_sfo);
                    if src_cat.as_deref() == Some("ac") {
                        // Do not let DLC icon or artwork overwrite the base game's assets
                        continue;
                    }
                }
            }
            fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

fn resolve_item_name(result: &ConversionResult) -> String {
    if result.output_path.is_dir() {
        let sfo1 = result.output_path.join("sce_sys").join("param.sfo");
        let sfo2 = result.output_path.join("param.sfo");
        let sfo3 = result.output_path.join("PS3_GAME").join("PARAM.SFO");
        let mut resolved_id = None;
        for sfo in &[sfo1, sfo2, sfo3] {
            if sfo.is_file() {
                let (id, _) = crate::forge::detector::FormatDetector::read_sfo(sfo);
                if let Some(id) = id {
                    if !id.is_empty() {
                        resolved_id = Some(id);
                        break;
                    }
                }
            }
        }
        if resolved_id.is_none() {
            resolved_id = result.title_id.clone();
        }
        if let Some(id) = resolved_id.as_deref().filter(|s| !s.is_empty()) {
            id.to_string()
        } else if let Some(title) = result.title_name.as_deref().filter(|s| !s.is_empty()) {
            title
                .chars()
                .map(|c| {
                    if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                        '_'
                    } else {
                        c
                    }
                })
                .collect::<String>()
        } else {
            let name = result
                .output_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            if name == "extracted" || name == "staged" || name == "Image0" {
                "Game".to_string()
            } else {
                name.to_string()
            }
        }
    } else {
        if result.platform == crate::forge::platform::Platform::PlayStation4 {
            let mut resolved_id = result.title_id.clone();
            if resolved_id.is_none() {
                if let Ok(mut f) = File::open(&result.output_path) {
                    let mut hdr = [0u8; 4096];
                    if let Ok(n) = f.read(&mut hdr) {
                        if let Some(cid) = crate::forge::detector::parse_pkg_content_id(&hdr[..n]) {
                            resolved_id = crate::forge::detector::title_id_from_content_id(&cid);
                        }
                    }
                }
            }
            if let Some(id) = resolved_id.as_deref().filter(|s| !s.is_empty()) {
                return format!("{id}.pkg");
            }
        }
        result
            .output_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "output".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        TransferManager, backup_destination_path, copy_file_with_progress,
        partial_destination_path, receive_tcp_file, removable_platform_dir, verify_delivered_tree,
    };
    use crate::forge::archive::TempWorkspace;
    use crate::forge::backend::ConversionResult;
    use crate::forge::{Platform, TargetFormat};
    use std::fs;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn tcp_dump_receive_writes_atomically_and_reports_byte_progress() {
        let workspace = TempWorkspace::new("ps5_tcp_receive").unwrap();
        let port_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = port_listener.local_addr().unwrap();
        drop(port_listener);
        let payload = b"Digital Forge PS5 transfer fixture".to_vec();
        let sender_payload = payload.clone();
        let sender = thread::spawn(move || {
            let started = std::time::Instant::now();
            let mut stream = loop {
                match TcpStream::connect(address) {
                    Ok(stream) => break stream,
                    Err(error) if started.elapsed() < Duration::from_secs(5) => {
                        let _ = error;
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("receiver did not start listening: {error}"),
                }
            };
            stream.write_all(&sender_payload).unwrap();
        });
        let destination = workspace.root.join("received.tar");
        let progress_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress_seen_cb = progress_seen.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_on_timeout = cancelled.clone();
        let test_started = std::time::Instant::now();
        let received = receive_tcp_file(
            address,
            &destination,
            Arc::new(move |update| {
                if update.bytes_processed > 0 {
                    progress_seen_cb.store(true, Ordering::Relaxed);
                }
                if test_started.elapsed() > Duration::from_secs(6) {
                    cancel_on_timeout.store(true, Ordering::Relaxed);
                }
            }),
            cancelled,
        )
        .unwrap();
        sender.join().unwrap();
        assert_eq!(received, payload.len() as u64);
        assert_eq!(fs::read(&destination).unwrap(), payload);
        assert!(progress_seen.load(Ordering::Relaxed));
        assert!(
            !workspace
                .root
                .join(".received.tar.digital-forge-partial")
                .exists()
        );
    }

    #[test]
    fn tcp_dump_receive_does_not_overwrite_existing_destination() {
        let workspace = TempWorkspace::new("ps5_tcp_receive_exists").unwrap();
        let destination = workspace.root.join("received.tar");
        fs::write(&destination, b"keep").unwrap();
        let error = receive_tcp_file(
            "127.0.0.1:0".parse().unwrap(),
            &destination,
            Arc::new(|_| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read(destination).unwrap(), b"keep");
    }

    #[test]
    fn cancelled_copy_removes_partial_output_and_preserves_source() {
        let workspace = TempWorkspace::new("cancelled_transfer").unwrap();
        let source = workspace.root.join("source.pkg");
        let destination = workspace.root.join("output.pkg");
        fs::write(&source, vec![0x5a; 9 * 1024 * 1024]).unwrap();
        let expected = fs::read(&source).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let set_cancelled = cancelled.clone();
        let result = copy_file_with_progress(
            &source,
            &destination,
            fs::metadata(&source).unwrap().len(),
            Arc::new(move |update| {
                if update.bytes_processed > 0 {
                    set_cancelled.store(true, Ordering::Relaxed);
                }
            }),
            cancelled,
        );

        assert!(result.is_err());
        assert!(!destination.exists());
        assert!(!partial_destination_path(&destination).exists());
        assert_eq!(fs::read(source).unwrap(), expected);
    }

    #[test]
    fn completed_copy_is_verified_and_atomically_named() {
        let workspace = TempWorkspace::new("completed_transfer").unwrap();
        let source = workspace.root.join("source.pkg");
        let destination = workspace.root.join("output.pkg");
        fs::write(&source, b"verified package bytes").unwrap();
        copy_file_with_progress(
            &source,
            &destination,
            fs::metadata(&source).unwrap().len(),
            Arc::new(|_| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"verified package bytes");
        assert!(!partial_destination_path(&destination).exists());
    }

    fn ps4_folder_result(source: std::path::PathBuf) -> ConversionResult {
        ConversionResult {
            platform: Platform::PlayStation4,
            target_format: TargetFormat::Ps4Folder,
            output_path: source,
            total_size: 32,
            was_passthrough: false,
            title_id: Some("CUSA09897".to_owned()),
            title_name: Some("LEGO The Incredibles".to_owned()),
            aux_files: Vec::new(),
            tool_report: None,
        }
    }

    #[test]
    fn ps4_removable_layout_is_games_ps4_title_id() {
        let result = ps4_folder_result(std::path::PathBuf::from("CUSA09897"));
        assert_eq!(
            removable_platform_dir(std::path::Path::new(r"P:\GAMES"), &result),
            std::path::PathBuf::from(r"P:\GAMES\PS4")
        );
    }

    #[test]
    fn directory_delivery_safely_replaces_an_existing_title() {
        let workspace = TempWorkspace::new("existing_ps4_destination").unwrap();
        let source = workspace.root.join("source");
        let library = workspace.root.join("library");
        let existing = library.join("CUSA09897");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&existing).unwrap();
        fs::write(source.join("payload.bin"), b"new bytes").unwrap();
        fs::write(existing.join("payload.bin"), b"existing bytes").unwrap();

        let delivered = TransferManager::transfer_local(
            &ps4_folder_result(source),
            &library,
            Arc::new(|_| {}),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert_eq!(delivered, existing);
        assert_eq!(
            fs::read(existing.join("payload.bin")).unwrap(),
            b"new bytes"
        );
        assert!(!partial_destination_path(&existing).exists());
        assert!(!backup_destination_path(&existing).exists());
    }

    #[test]
    fn delivered_tree_hashes_critical_ps4_files() {
        let workspace = TempWorkspace::new("critical_ps4_hash").unwrap();
        let source = workspace.root.join("source");
        let destination = workspace.root.join("destination");
        fs::create_dir_all(source.join("sce_sys")).unwrap();
        fs::create_dir_all(destination.join("sce_sys")).unwrap();
        fs::write(source.join("eboot.bin"), b"1234567890abcdef").unwrap();
        fs::write(destination.join("eboot.bin"), b"fedcba0987654321").unwrap();
        fs::write(source.join("sce_sys/param.sfo"), b"same metadata").unwrap();
        fs::write(destination.join("sce_sys/param.sfo"), b"same metadata").unwrap();

        assert!(verify_delivered_tree(&source, &destination).is_err());
    }
}
