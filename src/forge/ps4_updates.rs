//! Read and cache the latest public PS4 title-update metadata.
//!
//! Sony's update feed exposes the current published version, not a version
//! history. This module therefore deliberately models a single latest result
//! and does not invent historical choices. HTTPS certificate validation stays
//! enabled for metadata, manifests, and package downloads.

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use futures::StreamExt;
use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::blocking::{Client, Response};
use reqwest::header::CONTENT_LENGTH;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const PATCH_URL_KEY: [u8; 32] = [
    0xAD, 0x62, 0xE3, 0x7F, 0x90, 0x5E, 0x06, 0xBC, 0x19, 0x59, 0x31, 0x42, 0x28, 0x1C, 0x11, 0x2C,
    0xEC, 0x0E, 0x7E, 0xC3, 0xE9, 0x7E, 0xFD, 0xCA, 0xEF, 0xCD, 0xBA, 0xAF, 0xA6, 0x37, 0x8D, 0x84,
];
const SONY_PATCH_ROOT: &str = "https://gs-sec.ww.np.dl.playstation.net/plo/np";
const SONY_ROOT_CA_PEM: &[u8] =
    include_bytes!("../../consoles/PlayStation/FPKG/resources/sony-dnas-root-05.pem");
const SONY_ROOT_CA_SHA256: [u8; 32] = [
    0x51, 0xd5, 0xa7, 0x83, 0x3f, 0x67, 0xf1, 0xd1, 0xc6, 0x21, 0x2f, 0x75, 0x99, 0x7e, 0x4b, 0x83,
    0xda, 0xda, 0x0f, 0x79, 0x10, 0xba, 0x4b, 0x5a, 0x3f, 0xa4, 0xe9, 0xb7, 0xb9, 0xbc, 0x82, 0x7c,
];
const MAX_METADATA_BYTES: u64 = 4 * 1024 * 1024;
const MAX_UPDATE_BYTES: u64 = 200 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestUpdate {
    pub title_id: String,
    pub title: Option<String>,
    pub version: String,
    pub content_id: String,
    pub size_bytes: Option<u64>,
    pub manifest_url: String,
    pub metadata_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePackage {
    pub url: String,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
}

/// Query Sony's published metadata for the latest update of a PS4 Title ID.
/// `None` means the title's current feed returned HTTP 404 (no update entry).
pub fn latest_update(title_id: &str) -> Result<Option<LatestUpdate>> {
    let title_id = normalize_title_id(title_id)?;
    let metadata_url = update_metadata_url(&title_id);
    let client = http_client()?;
    let response = client
        .get(&metadata_url)
        .send()
        .with_context(|| format!("could not query the PS4 update catalog for {title_id}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = checked_response(response, &metadata_url)?;
    let bytes = read_limited(response, MAX_METADATA_BYTES)?;
    parse_update_xml(&title_id, &metadata_url, &bytes).map(Some)
}

/// Fetch the update manifest and return its official package URL and integrity
/// fields, if supplied by Sony's manifest.
pub fn update_package(update: &LatestUpdate) -> Result<UpdatePackage> {
    let manifest_url = validate_playstation_url(&update.manifest_url)?;
    let response = match http_client()?.get(manifest_url.as_str()).send() {
        Ok(response) => response,
        Err(error) if is_tls_hostname_mismatch(&error) => {
            let host = reqwest::Url::parse(&manifest_url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| "Sony's CDN".to_owned());
            bail!(
                "Sony's update CDN host `{host}` returned a TLS certificate that does not identify that hostname; Digital Forge refused the manifest"
            );
        }
        Err(error) => {
            return Err(error).context("could not retrieve the Sony update package manifest");
        }
    };
    let response = checked_response(response, manifest_url.as_str())?;
    let bytes = read_limited(response, MAX_METADATA_BYTES)?;
    let json: Value =
        serde_json::from_slice(&bytes).context("Sony update manifest is not valid JSON")?;
    let piece = json
        .get("pieces")
        .and_then(Value::as_array)
        .and_then(|pieces| pieces.first())
        .context("Sony update manifest contains no package piece")?;
    let raw_url = piece
        .get("url")
        .and_then(Value::as_str)
        .context("Sony update manifest package piece has no URL")?;
    let url = validate_playstation_url(raw_url)?;
    let size_bytes = piece
        .get("size")
        .and_then(Value::as_u64)
        .or(update.size_bytes);
    let sha256 = ["sha256", "digest", "hash"]
        .iter()
        .find_map(|key| piece.get(*key).and_then(Value::as_str))
        .map(|hash| hash.trim().to_ascii_lowercase())
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if let Some(size) = size_bytes {
        ensure!(
            size > 0 && size <= MAX_UPDATE_BYTES,
            "update size is outside supported limits"
        );
    }
    Ok(UpdatePackage {
        url,
        size_bytes,
        sha256,
    })
}

/// Download an official update into a title/version cache. A verified cached
/// file is reused. Incomplete transfers are written to `.part`, removed on
/// cancellation or error, and retried from the start up to three times.
pub fn download_update(
    update: &LatestUpdate,
    package: &UpdatePackage,
    cache_root: &Path,
    cancelled: Arc<AtomicBool>,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<PathBuf> {
    ensure!(
        package
            .size_bytes
            .is_none_or(|size| size > 0 && size <= MAX_UPDATE_BYTES),
        "update package size is outside supported limits"
    );
    let destination_dir = cache_root.join(&update.title_id).join(&update.version);
    fs::create_dir_all(&destination_dir).with_context(|| {
        format!(
            "could not create update cache {}",
            destination_dir.display()
        )
    })?;
    if let Some(size) = package.size_bytes {
        ensure!(
            fs2::available_space(&destination_dir)? >= size,
            "not enough free space to download the {} update",
            update.version
        );
    }
    let destination = destination_dir.join(format!("{}.pkg", update.title_id));
    if destination.is_file() {
        if verify_download(&destination, package).is_ok() {
            return Ok(destination);
        }
        fs::remove_file(&destination).with_context(|| {
            format!(
                "could not remove an invalid cached update {}",
                destination.display()
            )
        })?;
    }
    let partial = destination.with_extension("pkg.part");
    for attempt in 1..=3 {
        if cancelled.load(Ordering::Relaxed) {
            let _ = fs::remove_file(&partial);
            bail!("PS4 update download cancelled");
        }
        let result = download_attempt(
            &package.url,
            &partial,
            package,
            cancelled.clone(),
            &mut progress,
        );
        match result {
            Ok(()) => {
                verify_download(&partial, package)?;
                fs::rename(&partial, &destination).with_context(|| {
                    format!(
                        "could not finalize update cache file {}",
                        destination.display()
                    )
                })?;
                return Ok(destination);
            }
            Err(error) => {
                let _ = fs::remove_file(&partial);
                if attempt == 3 || cancelled.load(Ordering::Relaxed) {
                    return Err(error).context("PS4 update download failed after retries");
                }
            }
        }
    }
    unreachable!("bounded retry loop returns on success or final failure")
}

fn download_attempt(
    url: &str,
    partial: &Path,
    package: &UpdatePackage,
    cancelled: Arc<AtomicBool>,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not initialize PS4 update downloader")?;
    runtime.block_on(download_attempt_async(
        url, partial, package, cancelled, progress,
    ))
}

async fn download_attempt_async(
    url: &str,
    partial: &Path,
    package: &UpdatePackage,
    cancelled: Arc<AtomicBool>,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let url = validate_playstation_url(url)?;
    let response = match download_client()?.get(&url).send().await {
        Ok(response) => response,
        Err(error) if is_tls_hostname_mismatch(&error) => {
            let host = reqwest::Url::parse(&url)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_else(|| "Sony's CDN".to_owned());
            bail!(
                "Sony's update CDN host `{host}` returned a TLS certificate that does not identify that hostname; Digital Forge refused the package download"
            );
        }
        Err(error) => {
            return Err(error).context("could not download the official PS4 update package");
        }
    };
    ensure!(
        response.status().is_success(),
        "update request failed with HTTP {} for {url}",
        response.status()
    );
    validate_playstation_url(response.url().as_str())?;
    let total = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .or(package.size_bytes);
    if let (Some(expected), Some(actual)) = (package.size_bytes, total) {
        ensure!(
            expected == actual,
            "update download size differs from the published size"
        );
    }
    if let Some(size) = total {
        ensure!(
            size > 0 && size <= MAX_UPDATE_BYTES,
            "update response size is outside supported limits"
        );
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(partial)?;
    let mut downloaded = 0_u64;
    let mut stream = response.bytes_stream();
    loop {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "PS4 update download cancelled"
        );
        let next = tokio::time::timeout(Duration::from_secs(90), stream.next())
            .await
            .context("update download stalled for 90 seconds")?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.context("update download stream failed")?;
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        ensure!(
            downloaded <= MAX_UPDATE_BYTES,
            "update download exceeded the safety limit"
        );
        file.write_all(&chunk)?;
        progress(downloaded, total);
    }
    file.flush()?;
    file.sync_all()?;
    if let Some(expected) = package.size_bytes {
        ensure!(
            downloaded == expected,
            "update download ended early ({downloaded} of {expected} bytes)"
        );
    }
    Ok(())
}

fn verify_download(path: &Path, package: &UpdatePackage) -> Result<()> {
    let metadata = fs::metadata(path)?;
    ensure!(
        metadata.len() >= 0x100,
        "download is too small to be a PS4 PKG"
    );
    if let Some(expected) = package.size_bytes {
        ensure!(
            metadata.len() == expected,
            "cached update size does not match catalog metadata"
        );
    }
    let mut file = File::open(path)?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)?;
    ensure!(
        magic == *b"\x7fCNT",
        "downloaded update is not a PS4 PKG (invalid magic)"
    );
    if let Some(expected) = &package.sha256 {
        let mut file = File::open(path)?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        let actual = format!("{:x}", hash.finalize());
        ensure!(
            actual.eq_ignore_ascii_case(expected),
            "update SHA-256 does not match the manifest"
        );
    }
    Ok(())
}

fn parse_update_xml(title_id: &str, metadata_url: &str, xml: &[u8]) -> Result<LatestUpdate> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut root_seen = false;
    let mut in_paramsfo = false;
    let mut title = None;
    let mut package = None;
    loop {
        match reader
            .read_event_into(&mut buf)
            .context("could not parse Sony update XML")?
        {
            Event::Start(event) | Event::Empty(event) => {
                let name = event.local_name();
                if !root_seen {
                    ensure!(
                        name.as_ref() == b"titlepatch",
                        "unexpected PS4 update XML root"
                    );
                    root_seen = true;
                }
                if name.as_ref() == b"paramsfo" {
                    in_paramsfo = true;
                } else if name.as_ref() == b"package" && package.is_none() {
                    let mut version = None;
                    let mut content_id = None;
                    let mut manifest_url = None;
                    let mut size_bytes = None;
                    for attr in event.attributes() {
                        let attr = attr.context("invalid attribute in PS4 update XML")?;
                        let value = attr
                            .decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )?
                            .into_owned();
                        match attr.key.local_name().as_ref() {
                            b"version" => version = Some(value),
                            b"content_id" => content_id = Some(value),
                            b"manifest_url" => manifest_url = Some(value),
                            b"size" => size_bytes = value.parse::<u64>().ok(),
                            _ => {}
                        }
                    }
                    package = Some((version, content_id, manifest_url, size_bytes));
                }
            }
            Event::Text(text) if in_paramsfo && title.is_none() => {
                let value = text.decode()?.trim().to_owned();
                if !value.is_empty() {
                    title = Some(value);
                }
            }
            Event::End(event) if event.local_name().as_ref() == b"paramsfo" => {
                in_paramsfo = false;
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    ensure!(root_seen, "PS4 update metadata is empty");
    let (version, content_id, manifest_url, size_bytes) =
        package.context("Sony update feed has no package entry")?;
    let version = version.context("Sony update entry has no version")?;
    ensure!(
        version.len() <= 24
            && version.split('.').count() >= 2
            && version
                .split('.')
                .all(|part| { !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) }),
        "Sony update entry has an invalid version"
    );
    let content_id = content_id.context("Sony update entry has no content ID")?;
    ensure!(
        content_id
            .to_ascii_uppercase()
            .contains(&title_id.to_ascii_uppercase()),
        "Sony update content ID does not match base-game title ID {title_id}"
    );
    let manifest_url =
        validate_playstation_url(&manifest_url.context("Sony update entry has no manifest URL")?)?;
    if let Some(size) = size_bytes {
        ensure!(
            size > 0 && size <= MAX_UPDATE_BYTES,
            "update size is outside supported limits"
        );
    }
    Ok(LatestUpdate {
        title_id: title_id.to_owned(),
        title,
        version,
        content_id,
        size_bytes,
        manifest_url,
        metadata_url: metadata_url.to_owned(),
    })
}

fn normalize_title_id(title_id: &str) -> Result<String> {
    let title_id = title_id.trim().to_ascii_uppercase();
    let title_id = title_id.strip_suffix("_00").unwrap_or(&title_id).to_owned();
    ensure!(
        title_id.len() == 9
            && title_id.starts_with("CUSA")
            && title_id[4..].bytes().all(|byte| byte.is_ascii_digit()),
        "unsupported PS4 title ID {title_id}; expected CUSA followed by five digits"
    );
    Ok(title_id)
}

fn update_metadata_url(title_id: &str) -> String {
    let digest = hmac_sha256(&PATCH_URL_KEY, format!("np_{title_id}").as_bytes());
    format!("{SONY_PATCH_ROOT}/{title_id}/{digest}/{title_id}-ver.xml")
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> String {
    let mut block = [0_u8; 64];
    block[..key.len().min(64)].copy_from_slice(&key[..key.len().min(64)]);
    let mut inner = Sha256::new();
    for byte in block {
        inner.update([byte ^ 0x36]);
    }
    inner.update(message);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    for byte in block {
        outer.update([byte ^ 0x5c]);
    }
    outer.update(inner_hash);
    format!("{:x}", outer.finalize())
}

fn validate_playstation_url(raw: &str) -> Result<String> {
    let mut url =
        reqwest::Url::parse(raw).context("Sony update metadata contains an invalid URL")?;
    ensure!(
        url.scheme() == "https" || url.scheme() == "http",
        "unsupported update URL scheme"
    );
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    ensure!(
        host == "playstation.net" || host.ends_with(".playstation.net"),
        "update source is not hosted by playstation.net: {host}"
    );
    // Legacy Sony metadata advertises HTTP, but the same public CDN paths are
    // served over HTTPS. Never downgrade package downloads to cleartext.
    if url.scheme() == "http" {
        url.set_scheme("https")
            .map_err(|_| anyhow::anyhow!("could not require HTTPS for update URL"))?;
    }
    Ok(url.into())
}

fn checked_response(response: Response, request_url: &str) -> Result<Response> {
    ensure!(
        response.status().is_success(),
        "update request failed with HTTP {} for {request_url}",
        response.status()
    );
    let final_url = response.url().as_str();
    let _ = validate_playstation_url(final_url)?;
    Ok(response)
}

fn is_tls_hostname_mismatch(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut cause = Some(error);
    while let Some(error) = cause {
        let message = error.to_string().to_ascii_lowercase();
        if message.contains("certificate")
            && (message.contains("name does not match")
                || message.contains("hostname mismatch")
                || message.contains("does not identify"))
        {
            return true;
        }
        cause = error.source();
    }
    false
}

fn read_limited(response: Response, limit: u64) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        ensure!(length <= limit, "update metadata exceeds the size limit");
    }
    let mut data = Vec::new();
    response
        .take(limit + 1)
        .read_to_end(&mut data)
        .context("could not read update metadata")?;
    ensure!(
        data.len() as u64 <= limit,
        "update metadata exceeds the size limit"
    );
    Ok(data)
}

fn sony_root_certificate() -> Result<reqwest::Certificate> {
    let pem =
        std::str::from_utf8(SONY_ROOT_CA_PEM).context("Sony root CA asset is not UTF-8 PEM")?;
    let encoded: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .map(str::trim)
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("Sony root CA asset is not valid PEM")?;
    ensure!(
        Sha256::digest(&der).as_slice() == SONY_ROOT_CA_SHA256,
        "Sony update trust anchor does not match the pinned SCEI DNAS Root 05 fingerprint"
    );
    reqwest::Certificate::from_der(&der).context("could not load pinned Sony update root CA")
}

fn sony_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let url = attempt.url();
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        if url.scheme() == "https"
            && (host == "playstation.net" || host.ends_with(".playstation.net"))
        {
            attempt.follow()
        } else {
            attempt.error("Sony update redirect left the verified HTTPS PlayStation domain")
        }
    })
}

fn http_client() -> Result<Client> {
    Client::builder()
        .add_root_certificate(sony_root_certificate()?)
        .redirect(sony_redirect_policy())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(90))
        .user_agent(concat!("DigitalForge/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not initialize secure PS4 update client")
}

fn download_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .add_root_certificate(sony_root_certificate()?)
        .redirect(sony_redirect_policy())
        .connect_timeout(Duration::from_secs(15))
        // A per-chunk idle timeout below allows very large packages to take
        // longer than a fixed total duration while still retrying stalled IO.
        .timeout(Duration::from_secs(7 * 24 * 60 * 60))
        .user_agent(concat!("DigitalForge/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not initialize secure PS4 package downloader")
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &[u8] = br#"<?xml version="1.0"?><titlepatch titleid="CUSA00001"><tag><package version="01.07" content_id="IP9100-CUSA00001_00-PLAYROOM00000000" size="123456" manifest_url="https://gs2.ww.prod.dl.playstation.net/update.json"><paramsfo><title>THE PLAYROOM</title></paramsfo></package></tag></titlepatch>"#;

    #[test]
    fn sony_root_ca_is_valid_pem_with_the_pinned_fingerprint() {
        assert!(sony_root_certificate().is_ok());
    }

    #[test]
    #[ignore = "requires a live, TLS-validated connection to Sony's update feed"]
    fn live_sony_update_feed_lookup() {
        let update = latest_update("CUSA00552").expect("live Sony feed request must succeed");
        let update = update.expect("Sony should publish an update for the fixture title");
        eprintln!(
            "Sony update catalog: {} — {} ({:?} bytes)",
            update.title_id, update.version, update.size_bytes
        );
        match update_package(&update) {
            Ok(package) => eprintln!(
                "Sony package manifest accepted; package size {:?} bytes",
                package.size_bytes
            ),
            Err(error) => eprintln!("Sony package manifest unavailable: {error:#}"),
        }
        assert_eq!(update.title_id, "CUSA00552");
        assert!(!update.version.is_empty());
    }

    #[test]
    fn cdn_hostname_mismatch_is_detected_without_relaxing_tls() {
        let error =
            std::io::Error::other("The certificate's CN name does not match the passed value.");
        assert!(is_tls_hostname_mismatch(&error));
        let error = std::io::Error::other("connection reset");
        assert!(!is_tls_hostname_mismatch(&error));
    }

    #[test]
    fn title_id_normalization_and_validation() {
        assert_eq!(normalize_title_id("cusa00001_00").unwrap(), "CUSA00001");
        assert!(normalize_title_id("CUSA1234").is_err());
        assert!(normalize_title_id("PPSA00001").is_err());
    }

    #[test]
    fn update_url_matches_published_itemzflow_compatible_endpoint_derivation() {
        assert_eq!(
            update_metadata_url("CUSA00001"),
            "https://gs-sec.ww.np.dl.playstation.net/plo/np/CUSA00001/1123f23c1f00810a5e43fcb409ada7823bc5ad21b357817e314b6c4832cf6f9f/CUSA00001-ver.xml"
        );
    }

    #[test]
    fn latest_update_parser_preserves_version_identity_and_size() {
        let update = parse_update_xml("CUSA00001", "https://example", XML).unwrap();
        assert_eq!(update.title.as_deref(), Some("THE PLAYROOM"));
        assert_eq!(update.version, "01.07");
        assert_eq!(update.content_id, "IP9100-CUSA00001_00-PLAYROOM00000000");
        assert_eq!(update.size_bytes, Some(123456));
    }

    #[test]
    fn latest_update_parser_rejects_unsafe_version_as_path_component() {
        let xml = String::from_utf8_lossy(XML).replace("01.07", "../latest");
        assert!(parse_update_xml("CUSA00001", "https://example", xml.as_bytes()).is_err());
    }

    #[test]
    fn mismatched_update_content_id_is_rejected() {
        let xml = String::from_utf8_lossy(XML).replace("CUSA00001_00", "CUSA99999_00");
        assert!(parse_update_xml("CUSA00001", "https://example", xml.as_bytes()).is_err());
    }

    #[test]
    fn non_sony_urls_and_http_manifest_hops_are_rejected_or_upgraded() {
        assert!(validate_playstation_url("https://example.com/update.pkg").is_err());
        assert_eq!(
            validate_playstation_url("http://gs2.ww.prod.dl.playstation.net/a.pkg").unwrap(),
            "https://gs2.ww.prod.dl.playstation.net/a.pkg"
        );
    }

    #[test]
    fn downloaded_pkg_validation_checks_magic_size_and_digest() {
        let path = std::env::temp_dir().join(format!(
            "digital-forge-ps4-update-check-{}.pkg",
            std::process::id()
        ));
        let mut bytes = vec![0_u8; 0x100];
        bytes[..4].copy_from_slice(b"\x7fCNT");
        fs::write(&path, &bytes).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let package = UpdatePackage {
            url: "https://gs2.ww.prod.dl.playstation.net/update.pkg".to_owned(),
            size_bytes: Some(bytes.len() as u64),
            sha256: Some(digest),
        };
        assert!(verify_download(&path, &package).is_ok());
        let mut bad_package = package;
        bad_package.sha256 = Some("0".repeat(64));
        assert!(verify_download(&path, &bad_package).is_err());
        let _ = fs::remove_file(path);
    }
}
