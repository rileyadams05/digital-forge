//! Public MEGA transfers run on the user's PC; no account or external app is needed.
use super::{
    DownloadOption,
    download::{self, DownloadProgress},
};
use anyhow::{Context, Result, bail};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use futures::io::{AllowStdIo, AsyncWrite, AsyncWriteExt};
use mega::{Client, Node, Nodes};
use reqwest::Url;
use std::{
    collections::HashSet,
    fs,
    io::{self},
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::{Duration, Instant},
};

pub fn is_mega_option(option: &DownloadOption) -> bool {
    option.label.to_ascii_lowercase().starts_with("mega")
        || download::option_urls(option)
            .iter()
            .any(|url| Url::parse(url).ok().is_some_and(|url| is_mega_host(&url)))
}

pub fn failure_detail(error: &anyhow::Error) -> Option<&'static str> {
    for cause in error.chain() {
        if let Some(mega::Error::MegaError { code }) = cause.downcast_ref::<mega::Error>() {
            return Some(match code {
                mega::ErrorCode::ENOENT | mega::ErrorCode::EBLOCKED => {
                    "MEGA removed or blocked this share. Choose another source."
                }
                mega::ErrorCode::EOVERQUOTA => {
                    "MEGA transfer allowance reached. Wait and retry this source later."
                }
                _ => "MEGA could not complete the request. Retry or choose another source.",
            });
        }
    }
    error
        .to_string()
        .contains("MEGA")
        .then_some("MEGA download failed. Retry or choose another source.")
}

fn is_mega_host(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("mega.nz" | "www.mega.nz" | "mega.co.nz" | "www.mega.co.nz")
    )
}

// Legacy links put the folder/file ID and encryption key in the fragment.
fn normalize_link(value: &str) -> Result<String> {
    let url = Url::parse(value).context("Invalid MEGA link")?;
    if !is_mega_host(&url) {
        bail!("The selected link did not lead to MEGA");
    }
    let fragment = url
        .fragment()
        .context("MEGA link is missing its decryption key")?;
    let (kind, handle, key) = if let Some(rest) = fragment.strip_prefix("F!") {
        let (handle, key) = rest
            .split_once('!')
            .context("Incomplete MEGA folder link")?;
        ("folder", handle, key)
    } else if let Some(rest) = fragment.strip_prefix('!') {
        let (handle, key) = rest.split_once('!').context("Incomplete MEGA file link")?;
        ("file", handle, key)
    } else {
        let mut path = url.path().trim_matches('/').split('/');
        let kind = path.next().unwrap_or_default();
        let handle = path.next().unwrap_or_default();
        if path.next().is_some() {
            bail!("Unsupported MEGA link path");
        }
        (kind, handle, fragment)
    };
    let valid = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    if !matches!(kind, "file" | "folder") || !valid(handle) || !valid(key) {
        bail!("MEGA link is incomplete or uses an unsupported link format");
    }
    let key_bytes = BASE64_URL_SAFE_NO_PAD
        .decode(key)
        .context("Invalid MEGA decryption key")?;
    if key_bytes.len() != if kind == "file" { 32 } else { 16 } {
        bail!("MEGA link contains an incomplete decryption key");
    }
    Ok(format!("https://mega.nz/{kind}/{handle}#{key}"))
}

async fn resolve_link(http: &reqwest::Client, value: &str) -> Result<String> {
    let parsed = Url::parse(value).context("Invalid download link")?;
    if is_mega_host(&parsed) {
        return normalize_link(value);
    }
    // Follow shortened links only after selection, never while listing options.
    let response =
        http.get(parsed).send().await.map_err(|e| {
            anyhow::anyhow!("Could not resolve the MEGA shortcut: {}", e.without_url())
        })?;
    let url = response.url().clone();
    // Some MEGA landing pages reject HTTP clients even though the redirect is valid.
    if is_mega_host(&url) {
        return normalize_link(url.as_str());
    }
    bail!("The selected shortcut did not lead to a public MEGA link")
}

fn safe_component(name: &str) -> Result<()> {
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if name.is_empty()
        || matches!(name, "." | "..")
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        bail!("MEGA contains a filename that cannot be saved safely on this PC");
    }
    Ok(())
}

fn node_path(node: &Node, nodes: &Nodes) -> Result<PathBuf> {
    let mut parts = vec![node.name()];
    let mut parent = node.parent();
    let mut visited = HashSet::new();
    visited.insert(node.handle());
    while let Some(handle) = parent {
        if !visited.insert(handle) {
            bail!("MEGA folder contains a circular path");
        }
        let Some(folder) = nodes.get_node_by_handle(handle) else {
            break;
        };
        // Do not introduce the public share's top-level folder into multipart names.
        if folder
            .parent()
            .and_then(|p| nodes.get_node_by_handle(p))
            .is_none()
        {
            break;
        }
        parts.push(folder.name());
        parent = folder.parent();
    }
    let mut path = PathBuf::new();
    for part in parts.into_iter().rev() {
        safe_component(part)?;
        path.push(part);
    }
    Ok(path)
}

pub fn download_option(option: &DownloadOption, destination: &Path) -> Result<Vec<PathBuf>> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(download_async(option, destination))
}

async fn download_async(option: &DownloadOption, destination: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(destination)?;
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;
    let client = Client::builder()
        .https(true)
        .max_retries(3)
        .timeout(Some(Duration::from_secs(60)))
        .build(http.clone())?;
    transfer_links(option, destination, &http, &client).await
}

async fn transfer_links(
    option: &DownloadOption,
    destination: &Path,
    http: &reqwest::Client,
    client: &Client,
) -> Result<Vec<PathBuf>> {
    let mut completed = Vec::new();
    let mut paths = HashSet::new();
    let mut handles = HashSet::new();
    for url in download::option_urls(option) {
        crate::render_progress_screen("Downloading game", "Opening MEGA folder", 0, None);
        let link = resolve_link(&http, url).await?;
        let nodes = client.fetch_public_nodes(&link).await.context(
            "MEGA could not open this share (it may be removed or its key may be incorrect)",
        )?;
        let mut files = nodes
            .iter()
            .filter(|node| node.kind().is_file())
            .map(|node| Ok((node_path(node, &nodes)?, node)))
            .collect::<Result<Vec<_>>>()?;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        if files.is_empty() {
            bail!("MEGA reports no files in this share. Try another source link.");
        }
        for (relative, node) in files {
            safe_component(node.handle())?;
            let identity = format!("{}:{:?}", node.handle(), node.condensed_mac());
            if !handles.insert(identity.clone()) {
                continue;
            }
            if !paths.insert(relative.to_string_lossy().to_lowercase()) {
                bail!("MEGA contains conflicting filenames; cannot safely combine these links");
            }
            let target = destination.join(&relative);
            let parent = target.parent().context("Invalid MEGA destination")?;
            // The destination belongs to this job; reject pre-existing symlink components.
            let mut check = destination.to_path_buf();
            for component in relative.components() {
                check.push(component);
                if fs::symlink_metadata(&check).is_ok_and(|m| m.file_type().is_symlink()) {
                    bail!("MEGA download destination contains a symbolic link");
                }
            }
            fs::create_dir_all(parent)?;
            if target.is_file() && fs::metadata(&target)?.len() == node.size() {
                if let (Some(iv), Some(mac)) = (node.aes_iv(), node.condensed_mac()) {
                    let actual = mega::compute_condensed_mac(
                        AllowStdIo::new(fs::File::open(&target)?),
                        node.size(),
                        node.aes_key(),
                        iv,
                    )
                    .await?;
                    if actual == *mac {
                        completed.push(target);
                        continue;
                    }
                }
            }
            download::require_disk_space(
                destination,
                node.size().saturating_add(64 * 1024 * 1024),
            )?;
            let partial = parent.join(format!(".{}.mega-part", node.handle()));
            let mut output = ProgressWriter::new(
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&partial)
                    .or_else(|error| {
                        if error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(error);
                        }
                        // Only our own regular partial file may be replaced on retry.
                        if !fs::symlink_metadata(&partial)?.file_type().is_file() {
                            return Err(error);
                        }
                        fs::OpenOptions::new()
                            .write(true)
                            .truncate(true)
                            .open(&partial)
                    })?,
                node.name(),
                node.size(),
            );
            let result = client.download_node(node, &mut output).await;
            output.flush().await?;
            let written = output.written;
            output.inner.get_ref().sync_all()?;
            drop(output);
            // download_node validates MEGA's decrypted-content MAC before this rename.
            if let Err(error) = result {
                let _ = fs::remove_file(&partial);
                return Err(error).context("MEGA download failed; completed parts were kept. Retry or choose another source");
            }
            if written != node.size() {
                bail!("MEGA download ended before the complete file arrived");
            }
            if target.exists() {
                fs::remove_file(&target)?;
            }
            fs::rename(&partial, &target)?;
            completed.push(target);
        }
    }
    completed.sort();
    Ok(completed)
}

struct ProgressWriter {
    inner: AllowStdIo<fs::File>,
    name: String,
    total: u64,
    written: u64,
    started: Instant,
}
impl ProgressWriter {
    fn new(file: fs::File, name: &str, total: u64) -> Self {
        Self {
            inner: AllowStdIo::new(file),
            name: name.to_owned(),
            total,
            written: 0,
            started: Instant::now(),
        }
    }
}
impl AsyncWrite for ProgressWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = result {
            self.written += n as u64;
            let rate = self.written as f64 / self.started.elapsed().as_secs_f64().max(0.001);
            download::render_download_progress(&DownloadProgress {
                file_name: self.name.clone(),
                downloaded: self.written,
                total: Some(self.total),
                percent: (self.written.saturating_mul(100) / self.total.max(1)).min(99) as u8,
                bytes_per_second: rate,
                eta_seconds: (rate > 0.0)
                    .then_some(self.total.saturating_sub(self.written) as f64 / rate),
            });
        }
        result
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncrypt, BlockEncryptMut, KeyInit, KeyIvInit, StreamCipher};
    fn encrypted_attributes(name: &str, key: &[u8; 16]) -> String {
        let mut bytes = format!("MEGA{}", serde_json::json!({"n": name})).into_bytes();
        bytes.resize(bytes.len().div_ceil(16) * 16, 0);
        let mut aes = cbc::Encryptor::<aes::Aes128>::new(key.into(), (&[0u8; 16]).into());
        for block in bytes.chunks_exact_mut(16) {
            aes.encrypt_block_mut(block.into());
        }
        BASE64_URL_SAFE_NO_PAD.encode(bytes)
    }

    #[test]
    fn encrypted_folder_transfer_retries_completed_parts_and_rejects_corruption() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let payload: Vec<u8> = (0..400_123).map(|i| (i % 251) as u8).collect();
            let key = [23u8;16]; let iv = [7u8;8]; let folder_key = [41u8;16];
            let mac = mega::compute_condensed_mac(futures::io::Cursor::new(&payload), payload.len() as u64, &key, &iv).await.unwrap();
            let mut merged = [0u8;32];
            merged[..16].copy_from_slice(&key); merged[16..24].copy_from_slice(&iv); merged[24..].copy_from_slice(&mac);
            for i in 0..16 { merged[i] ^= merged[i+16]; }
            let mut encrypted_key = merged;
            let cipher = aes::Aes128::new((&folder_key).into());
            for chunk in encrypted_key.chunks_exact_mut(16) { cipher.encrypt_block(chunk.into()); }
            let mut root_key = folder_key;
            cipher.encrypt_block((&mut root_key).into());
            let mut ctr_iv = [0u8;16]; ctr_iv[..8].copy_from_slice(&iv);
            let mut ciphertext = payload.clone();
            ctr::Ctr128BE::<aes::Aes128>::new((&key).into(), (&ctr_iv).into()).apply_keystream(&mut ciphertext);
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let origin = format!("http://{}", server.server_addr());
            let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let corrupt = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (count, damaged, done) = (requests.clone(), corrupt.clone(), stop.clone());
            let root = serde_json::json!({"t":1,"a":encrypted_attributes("Share", &folder_key),"k":format!("root:{}", BASE64_URL_SAFE_NO_PAD.encode(root_key)),"h":"root","p":"","u":"owner","ts":0});
            let file_node = |name: &str, handle: &str| serde_json::json!({"t":0,"a":encrypted_attributes(name,&key),"k":format!("root:{}",BASE64_URL_SAFE_NO_PAD.encode(encrypted_key)),"h":handle,"p":"root","u":"owner","ts":0,"s":payload.len()});
            let listing = serde_json::json!([{"f":[root,file_node("game.part01.rar","part1"),file_node("game.part02.rar","part2")],"sn":"fixture"}]).to_string();
            let download_response = serde_json::json!([{"g":format!("{origin}/data"),"s":payload.len(),"at":encrypted_attributes("game.part01.rar",&key)}]).to_string();
            let server_thread = std::thread::spawn(move || {
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
                    let Some(mut request) = server.recv_timeout(Duration::from_millis(50)).unwrap() else { continue; };
                    if request.url().starts_with("/data") {
                        count.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
                        let mut bytes = ciphertext.clone();
                        if damaged.load(std::sync::atomic::Ordering::Relaxed) { bytes[0] ^= 1; }
                        request.respond(tiny_http::Response::from_data(bytes)).unwrap();
                    } else {
                        let mut body = String::new(); request.as_reader().read_to_string(&mut body).unwrap();
                        let commands: serde_json::Value = serde_json::from_str(&body).unwrap();
                        let response = if commands[0]["a"] == "f" { &listing } else { &download_response };
                        request.respond(tiny_http::Response::from_string(response)).unwrap();
                    }
                }
            });
            let http = reqwest::Client::new();
            let client = Client::builder().origin(Url::parse(&origin).unwrap()).build(http.clone()).unwrap();
            let url = format!("https://mega.nz/#F!root!{}", BASE64_URL_SAFE_NO_PAD.encode(folder_key));
            let option = DownloadOption { label:"MEGA".into(),url:url.clone(),urls:vec![url],password:None };
            let dir = std::env::temp_dir().join(format!("digital360-mega-fixture-{}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let files = transfer_links(&option,&dir,&http,&client).await.unwrap();
            assert_eq!(files.len(),2);
            for file in &files { assert_eq!(fs::read(file).unwrap(),payload); }
            assert_eq!(requests.load(std::sync::atomic::Ordering::Relaxed),2);
            transfer_links(&option,&dir,&http,&client).await.unwrap();
            assert_eq!(requests.load(std::sync::atomic::Ordering::Relaxed),2);
            // Same-size local damage must be detected and repaired.
            fs::write(&files[0],vec![0;payload.len()]).unwrap();
            transfer_links(&option,&dir,&http,&client).await.unwrap();
            assert_eq!(fs::read(&files[0]).unwrap(),payload);
            fs::remove_file(&files[0]).unwrap();
            corrupt.store(true,std::sync::atomic::Ordering::Relaxed);
            let error = transfer_links(&option,&dir,&http,&client).await.unwrap_err();
            assert!(format!("{error:#}").contains("MAC mismatch"));
            assert!(!files[0].exists());
            assert!(files[1].exists());
            stop.store(true,std::sync::atomic::Ordering::Relaxed); server_thread.join().unwrap();
            fs::remove_dir_all(&dir).unwrap();
        });
    }

    #[test]
    fn accepts_modern_and_legacy_public_links() {
        let folder_key = BASE64_URL_SAFE_NO_PAD.encode([1u8; 16]);
        let file_key = BASE64_URL_SAFE_NO_PAD.encode([1u8; 32]);
        assert_eq!(
            normalize_link(&format!("https://mega.co.nz/#F!folder123!{folder_key}")).unwrap(),
            format!("https://mega.nz/folder/folder123#{folder_key}")
        );
        assert_eq!(
            normalize_link(&format!("https://mega.nz/#!file123!{file_key}")).unwrap(),
            format!("https://mega.nz/file/file123#{file_key}")
        );
        assert_eq!(
            normalize_link(&format!("https://mega.nz/file/file123#{file_key}")).unwrap(),
            format!("https://mega.nz/file/file123#{file_key}")
        );
        assert!(normalize_link("https://mega.nz/#!file123!short").is_err());
        assert!(normalize_link("https://evil.example/#!file!key").is_err());
        assert!(normalize_link("https://mega.nz/folder/file123").is_err());
    }

    #[test]
    fn public_share_metadata_link_resolves_without_external_fixture() {
        let folder_key = BASE64_URL_SAFE_NO_PAD.encode([7u8; 16]);
        let legacy = format!("https://mega.co.nz/#F!folder123!{folder_key}");
        let expected = format!("https://mega.nz/folder/folder123#{folder_key}");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let http = reqwest::Client::new();
                assert_eq!(resolve_link(&http, &legacy).await.unwrap(), expected);
            });
    }

    #[test]
    fn public_download_option_is_detected_without_external_fixture() {
        let file_key = BASE64_URL_SAFE_NO_PAD.encode([9u8; 32]);
        let url = format!("https://mega.nz/file/file123#{file_key}");
        let option = DownloadOption {
            label: "Direct download".into(),
            url: url.clone(),
            urls: vec![url],
            password: None,
        };
        assert!(is_mega_option(&option));
    }

    #[test]
    fn rejects_unsafe_filenames_without_changing_multipart_names() {
        for name in [
            "..",
            "../escape",
            "C:\\escape",
            "file:stream",
            "CON",
            "nul.txt",
            "a.",
            "a ",
            "a\0",
        ] {
            assert!(safe_component(name).is_err(), "{name}");
        }
        for name in [
            "game.part01.rar",
            "game.part02.rar",
            "default.xex",
            "Game Files",
        ] {
            safe_component(name).unwrap();
        }
    }
}
