use anyhow::{Context, Error};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{MenuOption, pause_to_continue, render_terminal_header, select_menu};
use crate::app_log::OperationLog;
use crate::app_settings;
use crate::store::{DownloadOption, StoreListing};

const CONTROL_PORT: u16 = 36060;
const MAGIC: &str = "ISO2GOD";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum QueueStatus {
    Queued,
    Running,
    Paused,
    Complete,
    Failed,
}

impl QueueStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Queued => "Queued",
            Self::Running => "Downloading",
            Self::Paused => "Paused",
            Self::Complete => "Complete",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueItem {
    id: String,
    title: String,
    listing: StoreListing,
    option: DownloadOption,
    drive: String,
    status: QueueStatus,
    error: Option<String>,
}

fn queue_path() -> Result<PathBuf, Error> {
    Ok(app_settings::jobs_dir()?.join("download-queue.json"))
}

fn load() -> Result<Vec<QueueItem>, Error> {
    let path = queue_path()?;
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("the Downloads queue is invalid"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("could not read the Downloads queue"),
    }
}

fn save(items: &[QueueItem]) -> Result<(), Error> {
    let path = queue_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pending = path.with_extension("pending");
    fs::write(&pending, serde_json::to_vec_pretty(items)?)?;
    if path.exists() {
        fs::remove_file(&path)?;
    }
    fs::rename(pending, path).context("could not save the Downloads queue")
}

pub(super) fn enqueue(
    listing: StoreListing,
    option: DownloadOption,
    drive: String,
) -> Result<(), Error> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut items = load()?;
    items.push(QueueItem {
        id: format!("{}-{stamp}", std::process::id()),
        title: listing.title.clone(),
        listing,
        option,
        drive,
        status: QueueStatus::Queued,
        error: None,
    });
    save(&items)?;
    ensure_service();
    Ok(())
}

pub(super) fn ensure_service() {
    if std::env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").is_some() {
        return;
    }
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(executable);
    command
        .arg("--download-queue-service")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000008);
    }
    let _ = command.spawn();
}

fn next_queued(items: &[QueueItem]) -> Option<String> {
    items
        .iter()
        .find(|item| item.status == QueueStatus::Queued)
        .map(|item| item.id.clone())
}

fn update_status(id: &str, status: QueueStatus, error: Option<String>) -> Result<(), Error> {
    let mut items = load()?;
    let item = items
        .iter_mut()
        .find(|item| item.id == id)
        .context("queued download was not found")?;
    item.status = status;
    item.error = error;
    save(&items)
}

fn summary() -> String {
    let Ok(items) = load() else {
        return "Queue unavailable".to_owned();
    };
    if items.is_empty() {
        return "No queued downloads".to_owned();
    }
    let progress = crate::pipeline::load_last_job().ok().flatten();
    let visible = items
        .iter()
        .filter(|item| item.status != QueueStatus::Complete)
        .take(6)
        .map(|item| {
            if item.status == QueueStatus::Running {
                if let Some(job) = progress.as_ref().filter(|job| job.game_name == item.title) {
                    let (stage, percent) = job.pipeline.current_progress(job.delivery_label());
                    return format!("{} - {} ({}%)", item.title, stage, percent);
                }
            }
            format!("{} - {}", item.title, item.status.label())
        })
        .collect::<Vec<_>>();
    if visible.is_empty() {
        "All downloads complete".to_owned()
    } else {
        visible.join("\n")
    }
}

fn set_active_status(status: QueueStatus) -> String {
    let Ok(mut items) = load() else {
        return "Queue unavailable".to_owned();
    };
    let target = match status {
        QueueStatus::Paused => items
            .iter_mut()
            .find(|item| matches!(item.status, QueueStatus::Running | QueueStatus::Queued)),
        QueueStatus::Queued => items
            .iter_mut()
            .find(|item| matches!(item.status, QueueStatus::Paused | QueueStatus::Failed)),
        _ => None,
    };
    let Some(item) = target else {
        return "No matching download".to_owned();
    };
    item.status = status;
    let title = item.title.clone();
    if save(&items).is_err() {
        return "Queue unavailable".to_owned();
    }
    format!("{} - {}", title, status.label())
}

fn handle_control(socket: &UdpSocket, buffer: &mut [u8]) {
    let Ok((count, peer)) = socket.recv_from(buffer) else {
        return;
    };
    let request = String::from_utf8_lossy(&buffer[..count]);
    let response = match request.trim() {
        "ISO2GOD STATUS" => summary(),
        "ISO2GOD PAUSE" => set_active_status(QueueStatus::Paused),
        "ISO2GOD RESUME" => set_active_status(QueueStatus::Queued),
        _ => return,
    };
    let _ = socket.send_to(format!("{MAGIC} {response}").as_bytes(), peer);
}

pub(super) fn run_service() -> i32 {
    // Guide integration is retired. PC queue control stays local to this PC.
    let socket = match UdpSocket::bind(("127.0.0.1", CONTROL_PORT)) {
        Ok(socket) => socket,
        Err(_) => return 0, // The one persistent service is already running.
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));
    if let Ok(mut items) = load() {
        for item in &mut items {
            if item.status == QueueStatus::Running {
                item.status = QueueStatus::Queued;
            }
        }
        let _ = save(&items);
    }
    let mut active: Option<(String, Child)> = None;
    let mut buffer = [0_u8; 1024];
    loop {
        handle_control(&socket, &mut buffer);
        if let Some((id, child)) = active.as_mut() {
            let paused = load()
                .ok()
                .and_then(|items| items.into_iter().find(|item| item.id == *id))
                .is_some_and(|item| item.status == QueueStatus::Paused);
            if paused {
                let _ = child.kill();
                let _ = child.wait();
                active = None;
                continue;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = update_status(
                        id,
                        if status.success() {
                            QueueStatus::Complete
                        } else {
                            QueueStatus::Failed
                        },
                        (!status.success()).then(|| {
                            "The background job stopped; select Resume to retry.".to_owned()
                        }),
                    );
                    active = None;
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = update_status(id, QueueStatus::Failed, Some(error.to_string()));
                    active = None;
                }
            }
        } else if let Ok(items) = load() {
            if let Some(id) = next_queued(&items) {
                let _ = update_status(&id, QueueStatus::Running, None);
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .arg("--download-queue-job")
                    .arg(&id)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.creation_flags(0x08000000);
                }
                let child = command.spawn();
                match child {
                    Ok(child) => active = Some((id, child)),
                    Err(error) => {
                        let _ = update_status(&id, QueueStatus::Failed, Some(error.to_string()));
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(250));
    }
}

pub(super) fn run_job(id: &str) -> i32 {
    let result = (|| -> Result<(), Error> {
        let item = load()?
            .into_iter()
            .find(|item| item.id == id)
            .context("queued download was not found")?;
        let mut log = OperationLog::create("download_queue")?;
        // The destination is selected while queueing so the hidden worker never
        // blocks waiting for terminal input.
        unsafe { std::env::set_var("ISO2GOD_FTP_DRIVE", &item.drive) };
        unsafe { std::env::set_var("ISO2GOD_QUEUE_JOB", "1") };
        super::workflow::process_store_listing(
            &item.listing,
            crate::pipeline::JobDelivery::Ftp,
            item.option,
            &mut log,
        )
    })();
    if result.is_ok() { 0 } else { 1 }
}

pub(super) fn run_menu() -> Result<(), Error> {
    loop {
        render_terminal_header()?;
        println!("Downloads\n========================================================\n");
        let items = load()?;
        if items.is_empty() {
            println!("No downloads have been queued.");
            pause_to_continue()?;
            return Ok(());
        }
        for item in &items {
            println!("{:<42} {}", item.title, item.status.label());
            if let Some(error) = &item.error {
                println!("  {error}");
            }
        }
        println!();
        let options = [
            MenuOption {
                value: 0_u8,
                label: "Pause active download".to_owned(),
            },
            MenuOption {
                value: 1_u8,
                label: "Resume paused download".to_owned(),
            },
            MenuOption {
                value: 2_u8,
                label: "Back".to_owned(),
            },
        ];
        match select_menu(&options)? {
            Some(0) => {
                set_active_status(QueueStatus::Paused);
            }
            Some(1) => {
                set_active_status(QueueStatus::Queued);
                ensure_service();
            }
            _ => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_status_labels_are_user_facing() {
        assert_eq!(QueueStatus::Queued.label(), "Queued");
        assert_eq!(QueueStatus::Running.label(), "Downloading");
        assert_eq!(QueueStatus::Paused.label(), "Paused");
    }
}
