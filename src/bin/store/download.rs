use anyhow::{Context, Error, anyhow};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::{DownloadOption, percent_encode};

const MEDIAFIRE_FOLDER_API: &str = "https://www.mediafire.com/api/1.5/folder/get_content.php";
const DEFAULT_CONNECTIONS: usize = 8;
const MIN_SEGMENT_SIZE: u64 = 8 * 1024 * 1024;
const DOWNLOAD_RETRIES: usize = 4;
const DOWNLOAD_BUFFER_SIZE: usize = 256 * 1024;
static LAST_DOWNLOAD_RENDER: Mutex<Option<(u8, Instant)>> = Mutex::new(None);

#[derive(Debug, Clone, Copy)]
struct DownloadConfig {
    connections: usize,
    min_segment_size: u64,
    retries: usize,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            connections: DEFAULT_CONNECTIONS,
            min_segment_size: MIN_SEGMENT_SIZE,
            retries: DOWNLOAD_RETRIES,
        }
    }
}

#[derive(Debug)]
struct DownloadProbe {
    total: Option<u64>,
    ranges: bool,
    etag: Option<String>,
    last_modified: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DownloadSegment {
    index: usize,
    start: u64,
    end: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct DownloadManifest {
    version: u8,
    file_name: String,
    total: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    segments: Vec<DownloadSegment>,
}

pub struct DownloadProgress {
    pub file_name: String,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub percent: u8,
    pub bytes_per_second: f64,
    pub eta_seconds: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct MediaFireFile {
    pub direct_url: String,
    pub file_name: String,
}

pub fn require_disk_space(path: &Path, required: u64) -> Result<(), Error> {
    let available = fs2::available_space(path).context("could not check available disk space")?;
    if available < required {
        anyhow::bail!(
            "Insufficient disk space ({} required, {} available)",
            format_bytes(required),
            format_bytes(available)
        );
    }
    Ok(())
}

pub fn download_files(urls: &[String], destination: &Path) -> Result<Vec<PathBuf>, Error> {
    fs::create_dir_all(destination).context("could not create the download folder")?;
    require_disk_space(destination, 512 * 1024 * 1024)?;
    let mut files = Vec::new();
    for url in urls {
        files.push(download_file(url, destination)?);
    }
    Ok(files)
}

pub fn option_urls(option: &DownloadOption) -> Vec<&str> {
    if option.urls.is_empty() {
        vec![option.url.as_str()]
    } else {
        option.urls.iter().map(String::as_str).collect()
    }
}

/// Resolves a public MediaFire file or folder link and downloads every file
/// separately. MediaFire's bulk folder download requires an account, but the
/// individual public file pages do not.
pub fn download_mediafire_files(url: &str, destination: &Path) -> Result<Vec<PathBuf>, Error> {
    fs::create_dir_all(destination).context("could not create the download folder")?;
    require_disk_space(destination, 512 * 1024 * 1024)?;

    let agent = download_agent();
    let resolved = resolve_mediafire_files_with_agent(&agent, url)?;
    let mut files = Vec::with_capacity(resolved.len());
    for (index, file) in resolved.iter().enumerate() {
        if resolved.len() > 1 {
            println!(
                "Resolving MediaFire part {} of {}...",
                index + 1,
                resolved.len()
            );
        } else {
            println!("Resolving MediaFire download...");
        }
        files.push(download_file_with_agent(
            &agent,
            &file.direct_url,
            destination,
        )?);
    }
    Ok(files)
}

/// Resolves a MediaFire folder into separate public file downloads. Callers can
/// open each direct URL in its own browser tab, avoiding MediaFire's account-
/// gated bulk-folder download.
pub fn resolve_mediafire_files(url: &str) -> Result<Vec<MediaFireFile>, Error> {
    resolve_mediafire_files_with_agent(&download_agent(), url)
}

fn resolve_mediafire_files_with_agent(
    agent: &ureq::Agent,
    url: &str,
) -> Result<Vec<MediaFireFile>, Error> {
    let file_pages = if let Some(folder_key) = mediafire_folder_key(url) {
        mediafire_folder_file_pages(agent, &folder_key)?
    } else {
        vec![url.to_owned()]
    };
    if file_pages.is_empty() {
        anyhow::bail!("MediaFire folder contains no downloadable files");
    }
    file_pages
        .into_iter()
        .map(|page_url| {
            let direct_url = resolve_mediafire_file_page(agent, &page_url)?;
            Ok(MediaFireFile {
                file_name: file_name_from_url(&direct_url),
                direct_url,
            })
        })
        .collect()
}

pub fn download_file(url: &str, destination: &Path) -> Result<PathBuf, Error> {
    if url_requires_browser(url) {
        anyhow::bail!("Download link is unavailable");
    }
    let agent = download_agent();
    download_file_with_agent(&agent, url, destination)
}

fn download_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .user_agent("ISO2GOD/2.0 (authorized personal backup helper)")
        .build()
}

fn download_file_with_agent(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
) -> Result<PathBuf, Error> {
    accelerated_download(agent, url, destination, DownloadConfig::default())
}

fn accelerated_download(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
    config: DownloadConfig,
) -> Result<PathBuf, Error> {
    fs::create_dir_all(destination).context("could not create the download folder")?;
    let file_name = sanitize_file_name(&file_name_from_url(url));
    let path = destination.join(&file_name);
    let probe = probe_download(agent, url)?;

    if let Some(total) = probe.total {
        let existing = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if existing == total && total > 0 {
            println!("{file_name} is already downloaded.");
            return Ok(path);
        }

        // A partial final file came from the legacy single-stream downloader.
        // Continue it in place rather than discarding already downloaded data.
        if existing > 0 && existing < total {
            require_disk_space(
                destination,
                total
                    .saturating_sub(existing)
                    .saturating_add(64 * 1024 * 1024),
            )?;
            println!("Resuming {file_name} from {}...", format_bytes(existing));
            return single_stream_download(agent, url, &path, Some(total), config.retries);
        }

        if probe.ranges && total > config.min_segment_size {
            return segmented_download(agent, url, destination, &path, &file_name, &probe, config);
        }
        require_disk_space(destination, total.saturating_add(64 * 1024 * 1024))?;
    }

    println!("Server does not support accelerated ranges; using one connection.");
    let completed = single_stream_download(agent, url, &path, probe.total, config.retries)?;
    remove_stale_resume_data(destination, &file_name)?;
    Ok(completed)
}

fn probe_download(agent: &ureq::Agent, url: &str) -> Result<DownloadProbe, Error> {
    let response = match agent.get(url).set("Range", "bytes=0-0").call() {
        Ok(response) => response,
        Err(ureq::Error::Status(401 | 403 | 429, _)) => {
            anyhow::bail!("Download link is unavailable");
        }
        Err(error) => return Err(anyhow!("Download link is unavailable: {error}")),
    };
    let status = response.status();
    let etag = response.header("ETag").map(str::to_owned);
    let last_modified = response.header("Last-Modified").map(str::to_owned);
    if status == 206 {
        let total = response
            .header("Content-Range")
            .and_then(parse_content_range_total)
            .context("Download server returned an invalid range response")?;
        return Ok(DownloadProbe {
            total: Some(total),
            ranges: true,
            etag,
            last_modified,
        });
    }

    Ok(DownloadProbe {
        total: content_length(&response),
        ranges: false,
        etag,
        last_modified,
    })
}

fn segmented_download(
    agent: &ureq::Agent,
    url: &str,
    destination: &Path,
    final_path: &Path,
    file_name: &str,
    probe: &DownloadProbe,
    config: DownloadConfig,
) -> Result<PathBuf, Error> {
    let total = probe.total.context("Download size is unknown")?;
    let segment_count = calculate_segment_count(total, config.connections, config.min_segment_size);
    let segments = build_segments(total, segment_count);
    let part_dir = destination.join(format!(".{file_name}.iso2god-parts"));
    fs::create_dir_all(&part_dir).context("could not create download resume data")?;
    let manifest_path = part_dir.join("manifest.json");
    let manifest = DownloadManifest {
        version: 1,
        file_name: file_name.to_owned(),
        total,
        etag: probe.etag.clone(),
        last_modified: probe.last_modified.clone(),
        segments: segments.clone(),
    };
    prepare_resume_data(&part_dir, &manifest_path, &manifest)?;

    let initial_downloaded = segments.iter().try_fold(0_u64, |sum, segment| {
        let path = segment_path(&part_dir, segment.index);
        let length = fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        Ok::<u64, Error>(sum.saturating_add(length.min(segment_length(segment))))
    })?;
    require_disk_space(
        destination,
        total
            .saturating_sub(initial_downloaded)
            .saturating_add(64 * 1024 * 1024),
    )?;
    let downloaded = Arc::new(AtomicU64::new(initial_downloaded));
    println!("Accelerated download: {segment_count} background connections");

    let mut workers = Vec::with_capacity(segments.len());
    for segment in segments.clone() {
        let worker_url = url.to_owned();
        let worker_dir = part_dir.clone();
        let worker_downloaded = Arc::clone(&downloaded);
        let worker_etag = probe.etag.clone();
        workers.push(thread::spawn(move || {
            download_segment(
                &worker_url,
                &worker_dir,
                &segment,
                worker_etag.as_deref(),
                config.retries,
                &worker_downloaded,
            )
        }));
    }

    render_worker_progress(file_name, total, initial_downloaded, &downloaded, &workers);
    let mut first_error = None;
    for worker in workers {
        match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
            Err(_) if first_error.is_none() => {
                first_error = Some(anyhow!("A background download worker stopped unexpectedly"));
            }
            _ => {}
        }
    }
    println!();
    if let Some(error) = first_error {
        if format!("{error:#}").contains("stopped supporting ranges") {
            println!("Range acceleration was rejected; finishing with one connection.");
            let completed =
                single_stream_download(agent, url, final_path, Some(total), config.retries)?;
            fs::remove_dir_all(&part_dir).context("could not remove download resume data")?;
            return Ok(completed);
        }
        return Err(error).context(format!(
            "Download was interrupted; completed parts were kept for automatic resume in {}",
            part_dir.display()
        ));
    }

    merge_segments(&part_dir, &segments, final_path, total)?;
    fs::remove_dir_all(&part_dir).context("could not remove completed download resume data")?;
    Ok(final_path.to_path_buf())
}

fn download_segment(
    url: &str,
    part_dir: &Path,
    segment: &DownloadSegment,
    etag: Option<&str>,
    retries: usize,
    downloaded: &AtomicU64,
) -> Result<(), Error> {
    let path = segment_path(part_dir, segment.index);
    let expected = segment_length(segment);
    for attempt in 0..=retries {
        let existing = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if existing == expected {
            return Ok(());
        }
        if existing > expected {
            OpenOptions::new().write(true).truncate(true).open(&path)?;
            downloaded.fetch_sub(expected, Ordering::Relaxed);
        }
        let existing = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let request_start = segment.start + existing;
        let agent = download_agent();
        let mut request = agent
            .get(url)
            .set("Range", &format!("bytes={request_start}-{}", segment.end));
        if let Some(etag) = etag.filter(|value| !value.starts_with("W/")) {
            request = request.set("If-Range", etag);
        }
        let response = match request.call() {
            Ok(response) if response.status() == 206 => response,
            Ok(response) => {
                return Err(anyhow!(
                    "Download server stopped supporting ranges (HTTP {})",
                    response.status()
                ));
            }
            Err(error) => {
                if attempt < retries {
                    thread::sleep(retry_delay(attempt));
                    continue;
                }
                return Err(anyhow!("Download connection failed: {error}"));
            }
        };
        validate_content_range(&response, request_start, segment.end)?;
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let result = copy_response(response, &mut file, downloaded);
        file.flush()?;
        let length = fs::metadata(&path)?.len();
        if length == expected {
            return Ok(());
        }
        if length > expected {
            anyhow::bail!("Download server returned more data than requested");
        }
        if result.is_ok() && attempt == retries {
            anyhow::bail!("Download connection ended before its range was complete");
        }
        if attempt < retries {
            thread::sleep(retry_delay(attempt));
        }
    }
    anyhow::bail!("Download connection could not be completed")
}

fn copy_response(
    response: ureq::Response,
    file: &mut fs::File,
    downloaded: &AtomicU64,
) -> Result<(), Error> {
    let mut reader = response.into_reader();
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_SIZE];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        file.write_all(&buffer[..count])?;
        downloaded.fetch_add(count as u64, Ordering::Relaxed);
    }
}

fn render_worker_progress(
    file_name: &str,
    total: u64,
    initial: u64,
    downloaded: &AtomicU64,
    workers: &[thread::JoinHandle<Result<(), Error>>],
) {
    let started = Instant::now();
    while workers.iter().any(|worker| !worker.is_finished()) {
        let current = downloaded.load(Ordering::Relaxed).min(total);
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        let rate = current.saturating_sub(initial) as f64 / elapsed;
        render_download_progress(&DownloadProgress {
            file_name: file_name.to_owned(),
            downloaded: current,
            total: Some(total),
            percent: (current.saturating_mul(100) / total.max(1)).min(100) as u8,
            bytes_per_second: rate,
            eta_seconds: (rate > 1.0).then_some(total.saturating_sub(current) as f64 / rate),
        });
        thread::sleep(Duration::from_millis(200));
    }
    let current = downloaded.load(Ordering::Relaxed).min(total);
    render_download_progress(&DownloadProgress {
        file_name: file_name.to_owned(),
        downloaded: current,
        total: Some(total),
        percent: (current.saturating_mul(100) / total.max(1)).min(100) as u8,
        bytes_per_second: current.saturating_sub(initial) as f64
            / started.elapsed().as_secs_f64().max(0.001),
        eta_seconds: Some(0.0),
    });
}

fn single_stream_download(
    agent: &ureq::Agent,
    url: &str,
    path: &Path,
    expected_total: Option<u64>,
    retries: usize,
) -> Result<PathBuf, Error> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let started = Instant::now();
    let initial = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    for attempt in 0..=retries {
        let existing = fs::metadata(path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut request = agent.get(url);
        if existing > 0 {
            request = request.set("Range", &format!("bytes={existing}-"));
        }
        let response = match request.call() {
            Ok(response) => response,
            Err(_error) if attempt < retries => {
                thread::sleep(retry_delay(attempt));
                continue;
            }
            Err(error) => return Err(anyhow!("Download link is unavailable: {error}")),
        };
        let restart = existing > 0 && response.status() != 206;
        let mut file = if restart {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)?
        } else {
            OpenOptions::new().create(true).append(true).open(path)?
        };
        let base = if restart { 0 } else { existing };
        let total =
            expected_total.or_else(|| content_length(&response).map(|length| length + base));
        let counter = AtomicU64::new(base);
        let mut reader = response.into_reader();
        let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_SIZE];
        let copy_result: Result<(), Error> = (|| {
            loop {
                let count = reader.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                file.write_all(&buffer[..count])?;
                let current = counter.fetch_add(count as u64, Ordering::Relaxed) + count as u64;
                let elapsed = started.elapsed().as_secs_f64().max(0.001);
                let rate = current.saturating_sub(initial) as f64 / elapsed;
                render_download_progress(&DownloadProgress {
                    file_name: file_name.to_owned(),
                    downloaded: current,
                    total,
                    percent: total
                        .map(|total| (current * 100 / total.max(1)).min(100) as u8)
                        .unwrap_or(0),
                    bytes_per_second: rate,
                    eta_seconds: total.and_then(|total| {
                        (rate > 1.0).then_some(total.saturating_sub(current) as f64 / rate)
                    }),
                });
            }
            file.flush()?;
            Ok(())
        })();
        let final_size = fs::metadata(path)?.len();
        if copy_result.is_ok() && expected_total.is_none_or(|total| final_size == total) {
            println!();
            return Ok(path.to_path_buf());
        }
        if attempt < retries {
            thread::sleep(retry_delay(attempt));
            continue;
        }
        return copy_result.and_then(|_| {
            anyhow::bail!(
                "Download ended at {} instead of the expected {}",
                format_bytes(final_size),
                expected_total
                    .map(format_bytes)
                    .unwrap_or_else(|| "size".to_owned())
            )
        });
    }
    anyhow::bail!("Download could not be completed")
}

fn prepare_resume_data(
    part_dir: &Path,
    manifest_path: &Path,
    expected: &DownloadManifest,
) -> Result<(), Error> {
    let compatible = fs::read_to_string(manifest_path)
        .ok()
        .and_then(|contents| serde_json::from_str::<DownloadManifest>(&contents).ok())
        .is_some_and(|current| {
            current.version == expected.version
                && current.file_name == expected.file_name
                && current.total == expected.total
                && current.segments == expected.segments
                && validators_match(current.etag.as_deref(), expected.etag.as_deref())
                && validators_match(
                    current.last_modified.as_deref(),
                    expected.last_modified.as_deref(),
                )
        });
    if !compatible {
        for entry in fs::read_dir(part_dir)? {
            let path = entry?.path();
            if path.is_file() {
                fs::remove_file(path)?;
            }
        }
    }
    let contents = serde_json::to_vec_pretty(expected)?;
    fs::write(manifest_path, contents).context("could not save download resume data")
}

fn validators_match(previous: Option<&str>, current: Option<&str>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => previous == current,
        _ => true,
    }
}

fn remove_stale_resume_data(destination: &Path, file_name: &str) -> Result<(), Error> {
    let part_dir = destination.join(format!(".{file_name}.iso2god-parts"));
    if part_dir.is_dir() {
        fs::remove_dir_all(part_dir).context("could not remove stale download resume data")?;
    }
    Ok(())
}

fn merge_segments(
    part_dir: &Path,
    segments: &[DownloadSegment],
    final_path: &Path,
    total: u64,
) -> Result<(), Error> {
    let temporary = final_path.with_extension(format!(
        "{}.assembling",
        final_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("download")
    ));
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_SIZE];
    for segment in segments {
        let part_path = segment_path(part_dir, segment.index);
        let actual = fs::metadata(&part_path)?.len();
        if actual != segment_length(segment) {
            anyhow::bail!("A completed download part has the wrong size");
        }
        let mut part = fs::File::open(&part_path)?;
        loop {
            let count = part.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
        }
        drop(part);
        fs::remove_file(part_path)?;
    }
    output.flush()?;
    drop(output);
    if fs::metadata(&temporary)?.len() != total {
        anyhow::bail!("Assembled download has the wrong size");
    }
    if final_path.exists() {
        fs::remove_file(final_path)?;
    }
    fs::rename(&temporary, final_path).context("could not finish the downloaded file")
}

fn calculate_segment_count(total: u64, maximum: usize, minimum_size: u64) -> usize {
    let useful = total.div_ceil(minimum_size.max(1)).max(1) as usize;
    maximum.max(1).min(useful)
}

fn build_segments(total: u64, count: usize) -> Vec<DownloadSegment> {
    let size = total.div_ceil(count as u64);
    (0..count)
        .filter_map(|index| {
            let start = index as u64 * size;
            (start < total).then_some(DownloadSegment {
                index,
                start,
                end: (start + size - 1).min(total - 1),
            })
        })
        .collect()
}

fn segment_length(segment: &DownloadSegment) -> u64 {
    segment.end - segment.start + 1
}

fn segment_path(part_dir: &Path, index: usize) -> PathBuf {
    part_dir.join(format!("part-{index:03}.download"))
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    value.split('/').nth(1)?.trim().parse().ok()
}

fn validate_content_range(response: &ureq::Response, start: u64, end: u64) -> Result<(), Error> {
    let value = response
        .header("Content-Range")
        .context("Download range response omitted Content-Range")?;
    let range = value
        .strip_prefix("bytes ")
        .and_then(|value| value.split('/').next())
        .context("Download server returned invalid Content-Range")?;
    let (actual_start, actual_end) = range
        .split_once('-')
        .context("Download server returned invalid Content-Range")?;
    if actual_start.parse::<u64>().ok() != Some(start)
        || actual_end.parse::<u64>().ok() != Some(end)
    {
        anyhow::bail!("Download server returned the wrong byte range");
    }
    Ok(())
}

fn retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(4)))
}

fn sanitize_file_name(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let value = value.trim_matches([' ', '.']);
    if value.is_empty() {
        "download.bin".to_owned()
    } else {
        value.to_owned()
    }
}

fn resolve_mediafire_file_page(agent: &ureq::Agent, page_url: &str) -> Result<String, Error> {
    let html = fetch_text(agent, page_url, "MediaFire download page")?;
    extract_mediafire_download_url(&html)
        .ok_or_else(|| anyhow!("MediaFire did not provide a public download for this file"))
}

fn mediafire_folder_file_pages(
    agent: &ureq::Agent,
    folder_key: &str,
) -> Result<Vec<String>, Error> {
    let mut pages = Vec::new();
    for chunk in 1..=100_u32 {
        let url = format!(
            "{MEDIAFIRE_FOLDER_API}?folder_key={}&content_type=files&filter=all&order_by=name&order_direction=asc&chunk={chunk}&response_format=json",
            percent_encode(folder_key)
        );
        let json = fetch_text(agent, &url, "MediaFire folder")?;
        let value: serde_json::Value =
            serde_json::from_str(&json).context("MediaFire returned invalid folder data")?;
        if value
            .pointer("/response/result")
            .and_then(|value| value.as_str())
            == Some("Error")
        {
            let message = value
                .pointer("/response/message")
                .and_then(|value| value.as_str())
                .unwrap_or("MediaFire folder is unavailable");
            anyhow::bail!("{message}");
        }
        let folder = value
            .pointer("/response/folder_content")
            .ok_or_else(|| anyhow!("MediaFire folder is unavailable"))?;
        let chunk_pages = folder
            .get("files")
            .and_then(|value| value.as_array())
            .map(|files| {
                files
                    .iter()
                    .filter_map(mediafire_page_from_file_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let empty = chunk_pages.is_empty();
        pages.extend(chunk_pages);
        let more_chunks = folder
            .get("more_chunks")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("yes"));
        if empty || !more_chunks {
            break;
        }
    }
    let mut unique = Vec::with_capacity(pages.len());
    for page in pages {
        if !unique.contains(&page) {
            unique.push(page);
        }
    }
    Ok(unique)
}

fn mediafire_page_from_file_json(file: &serde_json::Value) -> Option<String> {
    if let Some(url) = file
        .pointer("/links/normal_download")
        .and_then(|value| value.as_str())
    {
        return Some(url.to_owned());
    }
    let quickkey = file.get("quickkey")?.as_str()?;
    let name = file.get("filename")?.as_str()?;
    Some(format!(
        "https://www.mediafire.com/file/{quickkey}/{}/file",
        percent_encode(name)
    ))
}

fn fetch_text(agent: &ureq::Agent, url: &str, what: &str) -> Result<String, Error> {
    let response = agent
        .get(url)
        .call()
        .map_err(|error| anyhow!("{what} is unavailable: {error}"))?;
    response
        .into_string()
        .with_context(|| format!("could not read the {what}"))
}

fn extract_mediafire_download_url(html: &str) -> Option<String> {
    let marker = "id=\"downloadButton\"";
    let button_at = html.find(marker)?;
    let tag_start = html[..button_at].rfind('<')?;
    let tag_end = html[button_at..]
        .find('>')
        .map(|offset| button_at + offset)?;
    let tag = &html[tag_start..=tag_end];
    let url = html_attribute(tag, "href")?;
    is_mediafire_direct_url(&url).then_some(url)
}

fn html_attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let marker = format!("{name}=");
    let at = lower.find(&marker)? + marker.len();
    let rest = tag[at..].trim_start();
    let quote = rest.chars().next()?;
    if matches!(quote, '\'' | '"') {
        let value = rest[1..].split(quote).next()?;
        Some(decode_html_url(value))
    } else {
        Some(decode_html_url(
            rest.split(|character: char| character.is_ascii_whitespace() || character == '>')
                .next()?,
        ))
    }
}

fn decode_html_url(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("\\/", "/")
}

fn mediafire_folder_key(url: &str) -> Option<String> {
    let path = url
        .split_once("://")?
        .1
        .split_once('/')
        .map(|(_, path)| path)?;
    let mut segments = path.split('/');
    while let Some(segment) = segments.next() {
        if segment.eq_ignore_ascii_case("folder") {
            return segments
                .next()
                .filter(|key| !key.is_empty())
                .map(|key| key.split('?').next().unwrap_or(key).to_owned());
        }
    }
    None
}

fn is_mediafire_direct_url(url: &str) -> bool {
    let Some(after_scheme) = url.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let host = after_scheme
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    host.starts_with("download") && host.ends_with(".mediafire.com")
}

fn content_length(response: &ureq::Response) -> Option<u64> {
    response
        .header("Content-Length")
        .and_then(|value| value.parse().ok())
}

fn url_requires_browser(url: &str) -> bool {
    !is_mediafire_direct_url(url) && matches!(super::classify_url(url), super::UrlClass::Hosted)
}

pub fn file_name_from_url(url: &str) -> String {
    let without_query = url.split('?').next().unwrap_or(url);
    let name = without_query.rsplit('/').next().unwrap_or("download.bin");
    let decoded = percent_decode(name);
    if decoded.is_empty() || decoded == "download.bin" {
        format!(
            "download-{}.bin",
            percent_encode(&url.chars().take(12).collect::<String>())
        )
    } else {
        decoded
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(value) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                decoded.push(value);
                index += 3;
                continue;
            }
        }
        decoded.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

pub fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / GIB)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

pub fn format_speed(bytes_per_second: f64) -> String {
    if bytes_per_second >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes_per_second / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KB/s", bytes_per_second / 1024.0)
    }
}

pub fn format_eta(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds else {
        return "calculating".to_owned();
    };
    let seconds = seconds.max(0.0).round() as u64;
    if seconds < 60 {
        format!("{seconds} sec")
    } else if seconds < 3600 {
        format!("{} min", seconds.div_ceil(60))
    } else {
        format!("{} hr", seconds / 3600)
    }
}

pub(super) fn render_download_progress(progress: &DownloadProgress) {
    let mut last = LAST_DOWNLOAD_RENDER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some((percent, updated)) = *last
        && percent == progress.percent
        && updated.elapsed() < Duration::from_millis(500)
    {
        return;
    }
    *last = Some((progress.percent, Instant::now()));
    drop(last);
    let total = progress
        .total
        .map(format_bytes)
        .unwrap_or_else(|| "unknown".to_owned());
    let file_name = progress.file_name.chars().take(24).collect::<String>();
    let detail = format!(
        "{} / {total}  |  {}  |  ETA: {}",
        format_bytes(progress.downloaded),
        format_speed(progress.bytes_per_second),
        format_eta(progress.eta_seconds)
    );
    crate::render_progress_screen(
        "Downloading game",
        &format!("Downloading {file_name}"),
        progress.percent,
        Some(&detail),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct RangeServer {
        url: String,
        requests: Arc<AtomicUsize>,
        ranges: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        address: std::net::SocketAddr,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl RangeServer {
        fn start(
            payload: Vec<u8>,
            supports_ranges: bool,
            fail_one_range: bool,
            reject_segment_ranges: bool,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let payload = Arc::new(payload);
            let requests = Arc::new(AtomicUsize::new(0));
            let ranges = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let failure_available = Arc::new(AtomicBool::new(fail_one_range));
            let worker_requests = Arc::clone(&requests);
            let worker_ranges = Arc::clone(&ranges);
            let worker_stop = Arc::clone(&stop);
            let worker = thread::spawn(move || {
                while !worker_stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let payload = Arc::clone(&payload);
                            let requests = Arc::clone(&worker_requests);
                            let ranges = Arc::clone(&worker_ranges);
                            let failure_available = Arc::clone(&failure_available);
                            thread::spawn(move || {
                                serve_test_request(
                                    stream,
                                    &payload,
                                    supports_ranges,
                                    reject_segment_ranges,
                                    &requests,
                                    &ranges,
                                    &failure_available,
                                );
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url: format!("http://{address}/game.bin"),
                requests,
                ranges,
                stop,
                address,
                worker: Some(worker),
            }
        }
    }

    impl Drop for RangeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect(self.address);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn serve_test_request(
        mut stream: TcpStream,
        payload: &[u8],
        supports_ranges: bool,
        reject_segment_ranges: bool,
        requests: &AtomicUsize,
        ranges: &AtomicUsize,
        failure_available: &AtomicBool,
    ) {
        requests.fetch_add(1, Ordering::Relaxed);
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut range = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                range = Some(value.trim().to_owned());
            }
        }

        if let Some(range) = range.as_deref() {
            ranges.fetch_add(1, Ordering::Relaxed);
            if range != "0-0" && failure_available.swap(false, Ordering::Relaxed) {
                let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                return;
            }
            if range != "0-0" && reject_segment_ranges {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(payload);
                return;
            }
            if supports_ranges {
                let (start, end) = range.split_once('-').unwrap();
                let start = start.parse::<usize>().unwrap();
                let end = if end.is_empty() {
                    payload.len() - 1
                } else {
                    end.parse::<usize>().unwrap().min(payload.len() - 1)
                };
                let body = &payload[start..=end];
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{}\r\nETag: \"test-file\"\r\nConnection: close\r\n\r\n",
                    body.len(),
                    payload.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(body);
                return;
            }
        }

        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: \"test-file\"\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(payload);
    }

    fn test_folder(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "iso2god-download-test-{label}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_payload() -> Vec<u8> {
        (0..256 * 1024).map(|index| (index % 251) as u8).collect()
    }

    fn test_config() -> DownloadConfig {
        DownloadConfig {
            connections: 4,
            min_segment_size: 1024,
            retries: 2,
        }
    }

    #[test]
    fn names_archive_files_from_direct_urls() {
        assert_eq!(
            file_name_from_url("https://cdn.example.com/files/game.part01.rar?token=1"),
            "game.part01.rar"
        );
    }

    #[test]
    fn extracts_mediafire_public_download_button() {
        let html = r#"<a class="input popsok" href="https://download123.mediafire.com/token/key/game.part01.rar" id="downloadButton" rel="nofollow">Download</a>"#;
        assert_eq!(
            extract_mediafire_download_url(html).as_deref(),
            Some("https://download123.mediafire.com/token/key/game.part01.rar")
        );
    }

    #[test]
    fn rejects_advertising_links_near_mediafire_button() {
        let html = r#"<a href="https://ads.example/download" id="downloadButton">Download</a>"#;
        assert_eq!(extract_mediafire_download_url(html), None);
    }

    #[test]
    fn recognizes_mediafire_folder_keys() {
        assert_eq!(
            mediafire_folder_key("https://www.mediafire.com/folder/abc123/Game_Parts"),
            Some("abc123".to_owned())
        );
        assert_eq!(
            mediafire_folder_key("https://www.mediafire.com/file/abc123/game.rar/file"),
            None
        );
    }

    #[test]
    fn reads_individual_file_page_from_folder_api() {
        let file = serde_json::json!({
            "filename": "game.part01.rar",
            "quickkey": "abc123",
            "links": {"normal_download": "https://www.mediafire.com/file/abc123/game.part01.rar/file"}
        });
        assert_eq!(
            mediafire_page_from_file_json(&file).as_deref(),
            Some("https://www.mediafire.com/file/abc123/game.part01.rar/file")
        );
    }

    #[test]
    fn downloads_and_assembles_concurrent_ranges() {
        let payload = test_payload();
        let server = RangeServer::start(payload.clone(), true, false, false);
        let folder = test_folder("ranges");
        let path =
            accelerated_download(&download_agent(), &server.url, &folder, test_config()).unwrap();
        assert_eq!(fs::read(path).unwrap(), payload);
        assert!(server.ranges.load(Ordering::Relaxed) >= 5);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn falls_back_when_server_ignores_ranges() {
        let payload = test_payload();
        let server = RangeServer::start(payload.clone(), false, false, false);
        let folder = test_folder("fallback");
        let path =
            accelerated_download(&download_agent(), &server.url, &folder, test_config()).unwrap();
        assert_eq!(fs::read(path).unwrap(), payload);
        assert!(server.requests.load(Ordering::Relaxed) >= 2);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn retries_a_failed_background_range() {
        let payload = test_payload();
        let server = RangeServer::start(payload.clone(), true, true, false);
        let folder = test_folder("retry");
        let path =
            accelerated_download(&download_agent(), &server.url, &folder, test_config()).unwrap();
        assert_eq!(fs::read(path).unwrap(), payload);
        assert!(server.ranges.load(Ordering::Relaxed) >= 6);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn resumes_completed_background_parts() {
        let payload = test_payload();
        let server = RangeServer::start(payload.clone(), true, false, false);
        let folder = test_folder("resume");
        let file_name = "game.bin";
        let part_dir = folder.join(format!(".{file_name}.iso2god-parts"));
        fs::create_dir_all(&part_dir).unwrap();
        let segments = build_segments(payload.len() as u64, 4);
        let manifest = DownloadManifest {
            version: 1,
            file_name: file_name.to_owned(),
            total: payload.len() as u64,
            etag: Some("\"test-file\"".to_owned()),
            last_modified: None,
            segments: segments.clone(),
        };
        fs::write(
            part_dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let first = &segments[0];
        fs::write(
            segment_path(&part_dir, first.index),
            &payload[first.start as usize..=first.end as usize],
        )
        .unwrap();

        let path =
            accelerated_download(&download_agent(), &server.url, &folder, test_config()).unwrap();
        assert_eq!(fs::read(path).unwrap(), payload);
        assert_eq!(server.ranges.load(Ordering::Relaxed), 4);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn falls_back_if_parallel_ranges_are_rejected_after_probe() {
        let payload = test_payload();
        let server = RangeServer::start(payload.clone(), true, false, true);
        let folder = test_folder("range-rejected");
        let path =
            accelerated_download(&download_agent(), &server.url, &folder, test_config()).unwrap();
        assert_eq!(fs::read(path).unwrap(), payload);
        assert!(!folder.join(".game.bin.iso2god-parts").exists());
        fs::remove_dir_all(folder).unwrap();
    }
}
