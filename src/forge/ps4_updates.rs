//! Discover, filter, and download PS4 title updates.
//!
//! Catalogue metadata and package downloading are separate concerns. Sony's
//! public title feed is the configured source and can list multiple package
//! versions. Itemzflow-style multi-entry `patches` responses
//! can be parsed without relying on Itemzflow's undisclosed private API; those
//! records are metadata-only until a supported package route is available.
//! HTTPS certificate validation stays enabled throughout.

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use futures::StreamExt;
use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_RANGE, RANGE};
use serde_json::Value;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
    /// Minimum firmware from Sony's decimal `system_ver` XML attribute.
    /// Missing or invalid values remain unknown and must not be assumed safe.
    pub required_firmware: Option<FirmwareVersion>,
    /// The digest embedded in the Sony PKG header, authenticated by the TLS
    /// title-update XML. It is not a SHA-256 hash of the entire PKG file.
    pub package_digest: Option<String>,
    pub manifest_url: String,
    pub metadata_url: String,
}

/// Provenance for an update catalogue record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCatalogueSource {
    SonyTitleFeed,
    ItemzflowPatches,
}

/// Normalized update metadata independent of its retrieval/download backend.
/// `raw_metadata` keeps the provider's complete record for fields Digital
/// Forge does not yet interpret. `download_info` is present only when the
/// record came from the verified Sony title-feed path used by our downloader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCatalogueEntry {
    pub title_id: String,
    pub version: String,
    pub size_bytes: Option<u64>,
    pub endpoint: Option<String>,
    pub required_firmware: Option<FirmwareVersion>,
    pub content_id: Option<String>,
    pub source: UpdateCatalogueSource,
    pub raw_metadata: Value,
    pub download_info: Option<LatestUpdate>,
}

/// Catalogue providers return metadata only. A provider must not imply that
/// an entry is downloadable unless it supplies a verified supported route.
pub trait UpdateCatalogueProvider {
    fn updates_for_title(&self, title_id: &str) -> Result<Vec<UpdateCatalogueEntry>>;
}

/// Sony's title update feed provider, including every package entry advertised
/// by the title's version XML.
#[derive(Debug, Default, Clone, Copy)]
pub struct SonyTitleFeedProvider;

impl UpdateCatalogueProvider for SonyTitleFeedProvider {
    fn updates_for_title(&self, title_id: &str) -> Result<Vec<UpdateCatalogueEntry>> {
        Ok(sony_updates_for_title(title_id)?
            .into_iter()
            .map(UpdateCatalogueEntry::from_sony)
            .collect())
    }
}

impl UpdateCatalogueEntry {
    fn from_sony(update: LatestUpdate) -> Self {
        let raw_metadata = serde_json::json!({
            "title": update.title.clone(),
            "version": update.version.clone(),
            "content_id": update.content_id.clone(),
            "size": update.size_bytes,
            "required_fw": update.required_firmware.as_ref().map(ToString::to_string),
            "manifest_url": update.manifest_url.clone(),
            "metadata_url": update.metadata_url.clone(),
        });
        Self {
            title_id: update.title_id.clone(),
            version: update.version.clone(),
            size_bytes: update.size_bytes,
            endpoint: Some(update.manifest_url.clone()),
            required_firmware: update.required_firmware.clone(),
            content_id: Some(update.content_id.clone()),
            source: UpdateCatalogueSource::SonyTitleFeed,
            raw_metadata,
            download_info: Some(update),
        }
    }
}

/// Parse the public Itemzflow `patches` response shape. This deliberately does
/// not fetch an endpoint: Itemzflow's checked-in source hides its PATCH_API_URL,
/// so no private URL is guessed or recovered here. Parsed records are
/// metadata-only and cannot be sent to the Sony downloader.
pub fn parse_itemzflow_patches(title_id: &str, bytes: &[u8]) -> Result<Vec<UpdateCatalogueEntry>> {
    let title_id = normalize_title_id(title_id)?;
    let root: Value =
        serde_json::from_slice(bytes).context("update catalogue is not valid JSON")?;
    let patches = root
        .get("patches")
        .and_then(Value::as_array)
        .context("update catalogue must contain a `patches` array")?;
    let mut updates = Vec::with_capacity(patches.len());
    for (index, patch) in patches.iter().enumerate() {
        let field_error =
            |field: &str| anyhow::anyhow!("patches[{index}] is missing or has invalid `{field}`");
        if let Some(record_title_id) = ["title_id", "titleId", "titleid"]
            .iter()
            .find_map(|key| patch.get(*key).and_then(Value::as_str))
        {
            ensure!(
                record_title_id.eq_ignore_ascii_case(&title_id),
                "patches[{index}] belongs to {record_title_id}, not requested title {title_id}"
            );
        }
        let version = patch
            .get("version")
            .and_then(value_as_text)
            .filter(|value| is_safe_update_version(value))
            .ok_or_else(|| field_error("version"))?;
        let size_bytes = patch
            .get("size")
            .and_then(value_as_u64)
            .filter(|size| *size > 0 && *size <= MAX_UPDATE_BYTES)
            .ok_or_else(|| field_error("size"))?;
        let endpoint = patch
            .get("endpoint")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .ok_or_else(|| field_error("endpoint"))?
            .to_owned();
        let required_firmware = match patch.get("required_fw") {
            None | Some(Value::Null) => None,
            // Itemzflow stores the console firmware and required_fw as the
            // same packed Orbis version integer, not as a decimal version
            // string. Decode that integer using Sony's system_ver encoding.
            // A zero value is Itemzflow's documented "unknown" sentinel.
            Some(Value::Number(number)) => {
                let packed = number
                    .as_u64()
                    .filter(|value| *value <= u32::MAX as u64)
                    .ok_or_else(|| field_error("required_fw"))?;
                if packed == 0 {
                    None
                } else {
                    Some(
                        FirmwareVersion::from_system_ver_decimal(&packed.to_string()).ok_or_else(
                            || {
                                anyhow::anyhow!(
                                    "patches[{index}] has invalid packed `required_fw` value {packed}"
                                )
                            },
                        )?,
                    )
                }
            }
            // Accept human-readable values from compatible third-party feeds,
            // but do not interpret arbitrary decimal strings as packed values.
            Some(Value::String(raw)) => Some(FirmwareVersion::parse(raw).with_context(|| {
                format!("patches[{index}] has invalid `required_fw` value {raw:?}")
            })?),
            Some(_) => return Err(field_error("required_fw")),
        };
        let content_id = patch
            .get("content_id")
            .or_else(|| patch.get("contentId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(content_id) = &content_id
            && let Some(content_title_id) =
                crate::forge::detector::title_id_from_content_id(content_id)
        {
            ensure!(
                content_title_id.eq_ignore_ascii_case(&title_id),
                "patches[{index}] content ID {content_id} does not match requested title {title_id}"
            );
        }
        updates.push(UpdateCatalogueEntry {
            title_id: title_id.clone(),
            version,
            size_bytes: Some(size_bytes),
            endpoint: Some(endpoint),
            required_firmware,
            content_id,
            source: UpdateCatalogueSource::ItemzflowPatches,
            raw_metadata: patch.clone(),
            download_info: None,
        });
    }
    sort_catalogue_newest_first(&mut updates);
    Ok(updates)
}

/// Sort parsed numeric game versions newest-first without trusting provider
/// response order. The stable order is retained for numerically equal labels.
pub fn sort_catalogue_newest_first(updates: &mut [UpdateCatalogueEntry]) {
    updates.sort_by(|left, right| {
        compare_update_versions(&right.version, &left.version).unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Keep records for this title with a known requirement no higher than the
/// user's manually entered console firmware. Unknown requirements are excluded.
pub fn compatible_catalogue_updates<'a>(
    updates: &'a [UpdateCatalogueEntry],
    title_id: &str,
    console_firmware: &FirmwareVersion,
) -> Vec<&'a UpdateCatalogueEntry> {
    updates
        .iter()
        .filter(|update| {
            update.title_id.eq_ignore_ascii_case(title_id)
                && update
                    .required_firmware
                    .as_ref()
                    .is_some_and(|required| required.compatible_with(console_firmware))
        })
        .collect()
}

/// Get selectable indices in catalogue order. Records without a download route
/// remain visible as metadata but cannot be selected for a conversion.
pub fn selectable_update_indices(
    updates: &[UpdateCatalogueEntry],
    title_id: &str,
    console_firmware: &FirmwareVersion,
) -> Vec<usize> {
    updates
        .iter()
        .enumerate()
        .filter(|(_, update)| {
            update.title_id.eq_ignore_ascii_case(title_id)
                && update.download_info.is_some()
                && update
                    .required_firmware
                    .as_ref()
                    .is_some_and(|required| required.compatible_with(console_firmware))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Compare dotted numeric game-version labels component-wise.
pub fn compare_update_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let parse = |version: &str| -> Option<Vec<u32>> {
        if !is_safe_update_version(version) {
            return None;
        }
        version.split('.').map(|part| part.parse().ok()).collect()
    };
    let mut left = parse(left)?;
    let mut right = parse(right)?;
    let width = left.len().max(right.len());
    left.resize(width, 0);
    right.resize(width, 0);
    Some(left.cmp(&right))
}

fn is_safe_update_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 32
        && version.split('.').all(|part| {
            !part.is_empty() && part.len() <= 9 && part.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn value_as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn value_as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Return records from the configured public provider. The Sony provider is
/// intentionally the only live provider; the public title XML can include
/// multiple version-specific package and manifest records.
pub fn update_catalogue(title_id: &str) -> Result<Vec<UpdateCatalogueEntry>> {
    let mut updates = SonyTitleFeedProvider.updates_for_title(title_id)?;
    sort_catalogue_newest_first(&mut updates);
    Ok(updates)
}

/// Human-readable PS4 firmware version with numeric, component-wise ordering.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FirmwareVersion(Vec<u16>);

impl FirmwareVersion {
    /// Parse a manually entered version such as `9.00` or `11.52`.
    pub fn parse(value: &str) -> Result<Self> {
        let parts = value.trim().split('.').collect::<Vec<_>>();
        ensure!(
            (2..=4).contains(&parts.len())
                && parts.iter().all(|part| {
                    !part.is_empty()
                        && part.len() <= 2
                        && part.bytes().all(|byte| byte.is_ascii_digit())
                }),
            "enter PS4 system software as numeric components, for example 9.00 or 11.52"
        );
        let parts = parts
            .iter()
            .map(|part| part.parse::<u16>())
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(
            parts.iter().all(|part| *part <= 99),
            "firmware components must be between 0 and 99"
        );
        let mut parts = parts;
        while parts.len() > 2 && parts.last() == Some(&0) {
            parts.pop();
        }
        Ok(Self(parts))
    }

    /// Sony encodes `system_ver` as a decimal representation of a packed BCD
    /// word. Decode each byte into the displayed decimal firmware components.
    pub fn from_system_ver_decimal(value: &str) -> Option<Self> {
        let encoded = value.trim().parse::<u32>().ok()?;
        if encoded == 0 {
            return None;
        }
        let bytes = encoded.to_be_bytes();
        let mut parts = vec![bytes[0] as u16];
        for byte in &bytes[1..] {
            let tens = *byte >> 4;
            let ones = *byte & 0x0f;
            if tens > 9 || ones > 9 {
                return None;
            }
            parts.push((tens * 10 + ones) as u16);
        }
        while parts.len() > 2 && parts.last() == Some(&0) {
            parts.pop();
        }
        (parts.len() >= 2).then_some(Self(parts))
    }

    pub fn compatible_with(&self, console: &Self) -> bool {
        self <= console
    }
}

impl std::fmt::Display for FirmwareVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, part) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(".")?;
                write!(formatter, "{part:02}")?;
            } else {
                write!(formatter, "{part}")?;
            }
        }
        Ok(())
    }
}

/// Keep only updates whose required firmware is explicitly published and no
/// newer than the firmware the user entered. Unknown requirements are omitted.
pub fn compatible_updates<'a>(
    updates: &'a [LatestUpdate],
    console_firmware: &FirmwareVersion,
) -> Vec<&'a LatestUpdate> {
    updates
        .iter()
        .filter(|update| {
            update
                .required_firmware
                .as_ref()
                .is_some_and(|required| required.compatible_with(console_firmware))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePiece {
    pub url: String,
    pub file_offset: u64,
    pub size_bytes: u64,
    pub sha1: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePackage {
    pub url: String,
    pub size_bytes: Option<u64>,
    pub package_digest: Option<String>,
    pub sha256: Option<String>,
    pub title_id: String,
    pub content_id: String,
    pub pieces: Vec<UpdatePiece>,
}

/// Query Sony's published metadata for the latest update of a PS4 Title ID.
/// `None` means the title's current feed returned HTTP 404 (no update entry).
pub fn latest_update(title_id: &str) -> Result<Option<LatestUpdate>> {
    let mut updates = sony_updates_for_title(title_id)?;
    sort_latest_updates_newest_first(&mut updates);
    Ok(updates.into_iter().next())
}

/// Query every update package version Sony lists in the public title XML.
pub fn sony_updates_for_title(title_id: &str) -> Result<Vec<LatestUpdate>> {
    let title_id = normalize_title_id(title_id)?;
    let metadata_url = update_metadata_url(&title_id);
    let client = http_client()?;
    let response = client.get(&metadata_url).send().map_err(|error| {
        network_error(
            error,
            "could not query the PS4 update catalog",
            &metadata_url,
        )
    })?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }
    let response = checked_response(response, &metadata_url)?;
    let bytes = read_limited(response, MAX_METADATA_BYTES)?;
    parse_update_xml_entries(&title_id, &metadata_url, &bytes)
}

/// Fetch the update manifest and return its official package URL and integrity
/// fields, if supplied by Sony's manifest.
pub fn update_package(update: &LatestUpdate) -> Result<UpdatePackage> {
    let manifest_url = validate_sony_update_url(&update.manifest_url)?;
    let response = update_transfer_client()?
        .get(manifest_url.as_str())
        .send()
        .map_err(|error| {
            network_error(
                error,
                "could not retrieve Sony's advertised PS4 package manifest",
                manifest_url.as_str(),
            )
        })?;
    let response = checked_update_response(response, &manifest_url)?;
    let bytes = read_limited(response, MAX_METADATA_BYTES)?;
    parse_update_manifest(update, &manifest_url, &bytes)
}

fn parse_update_manifest(
    update: &LatestUpdate,
    manifest_url: &reqwest::Url,
    bytes: &[u8],
) -> Result<UpdatePackage> {
    let json: Value =
        serde_json::from_slice(bytes).context("Sony update manifest is not valid JSON")?;
    let manifest_digest = json
        .get("packageDigest")
        .and_then(Value::as_str)
        .map(|digest| digest.trim().to_ascii_uppercase())
        .filter(|digest| is_hex_digest(digest, 64))
        .context("Sony update manifest has no valid packageDigest")?;
    let trusted_digest = update.package_digest.as_deref().context(
        "Sony's TLS catalog did not provide a package digest; refusing an HTTP manifest",
    )?;
    ensure!(
        manifest_digest.eq_ignore_ascii_case(trusted_digest),
        "Sony update manifest packageDigest does not match the TLS-authenticated title catalog"
    );
    let manifest_size = json
        .get("originalFileSize")
        .and_then(Value::as_u64)
        .context("Sony update manifest has no valid originalFileSize")?;
    if let Some(catalog_size) = update.size_bytes {
        ensure!(
            manifest_size == catalog_size,
            "Sony update manifest size does not match the TLS-authenticated title catalog"
        );
    }
    ensure!(
        manifest_size > 0 && manifest_size <= MAX_UPDATE_BYTES,
        "update size is outside supported limits"
    );
    let manifest_host = manifest_url.host_str().unwrap_or_default();
    let raw_pieces = json
        .get("pieces")
        .and_then(Value::as_array)
        .context("Sony update manifest contains no package pieces")?;
    ensure!(
        !raw_pieces.is_empty(),
        "Sony update manifest contains no package pieces"
    );
    let declared_piece_count = json
        .get("numberOfSplitFiles")
        .and_then(Value::as_u64)
        .context("Sony update manifest has no valid numberOfSplitFiles")?;
    ensure!(
        declared_piece_count == raw_pieces.len() as u64,
        "Sony update manifest piece count is inconsistent"
    );
    let mut pieces = Vec::with_capacity(raw_pieces.len());
    for raw_piece in raw_pieces {
        let raw_url = raw_piece
            .get("url")
            .and_then(Value::as_str)
            .context("Sony update manifest package piece has no URL")?;
        let url = validate_sony_update_url(raw_url)?;
        ensure!(
            url.host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(manifest_host)),
            "Sony update piece host does not match its manifest host"
        );
        let file_offset = raw_piece
            .get("fileOffset")
            .and_then(Value::as_u64)
            .context("Sony update manifest package piece has no valid fileOffset")?;
        let size_bytes = raw_piece
            .get("fileSize")
            .and_then(Value::as_u64)
            .context("Sony update manifest package piece has no valid fileSize")?;
        ensure!(
            size_bytes > 0,
            "Sony update manifest contains an empty package piece"
        );
        let sha1 = raw_piece
            .get("hashValue")
            .and_then(Value::as_str)
            .map(|hash| hash.trim().to_ascii_uppercase())
            .filter(|hash| is_hex_digest(hash, 40))
            .context("Sony update manifest package piece has no valid SHA-1 hash")?;
        pieces.push(UpdatePiece {
            url: url.into(),
            file_offset,
            size_bytes,
            sha1,
        });
    }
    pieces.sort_by_key(|piece| piece.file_offset);
    let mut next_offset = 0_u64;
    for piece in &pieces {
        ensure!(
            piece.file_offset == next_offset,
            "Sony update package pieces contain a gap or overlap"
        );
        next_offset = next_offset
            .checked_add(piece.size_bytes)
            .context("Sony update piece offsets overflow")?;
    }
    ensure!(
        next_offset == manifest_size,
        "Sony update pieces do not cover the declared package size"
    );
    let sha256 = json
        .get("sha256")
        .and_then(Value::as_str)
        .map(|hash| hash.trim().to_ascii_lowercase())
        .filter(|hash| is_hex_digest(hash, 64));
    let url = pieces[0].url.clone();
    Ok(UpdatePackage {
        url,
        size_bytes: Some(manifest_size),
        package_digest: Some(manifest_digest),
        sha256,
        title_id: update.title_id.clone(),
        content_id: update.content_id.clone(),
        pieces,
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
        package.title_id.eq_ignore_ascii_case(&update.title_id),
        "update package title ID does not match the selected game"
    );
    ensure!(
        package.content_id.eq_ignore_ascii_case(&update.content_id),
        "update package content ID does not match the selected update"
    );
    ensure!(
        package
            .size_bytes
            .is_none_or(|size| size > 0 && size <= MAX_UPDATE_BYTES),
        "update package size is outside supported limits"
    );
    ensure!(
        !package.pieces.is_empty(),
        "Sony update manifest contains no downloadable package pieces"
    );
    ensure!(
        package
            .package_digest
            .as_deref()
            .is_some_and(|digest| is_hex_digest(digest, 64)),
        "Sony update has no valid package-header digest authenticated by its TLS catalog"
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
        if verify_download(&destination, package).is_ok()
            && verify_download_pieces(&destination, &package.pieces).is_ok()
        {
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
    let piece_paths = package
        .pieces
        .iter()
        .enumerate()
        .map(|(index, _)| destination_dir.join(format!("{}.pkg.part.{index}", update.title_id)))
        .collect::<Vec<_>>();
    for attempt in 1..=3 {
        if cancelled.load(Ordering::Relaxed) {
            cleanup_partial_files(&partial, &piece_paths);
            bail!("PS4 update download cancelled");
        }
        let mut downloaded_before = 0_u64;
        let mut failed = None;
        for (piece, piece_path) in package.pieces.iter().zip(&piece_paths) {
            if piece_path.is_file() && verify_piece(piece_path, piece).is_ok() {
                downloaded_before = downloaded_before.saturating_add(piece.size_bytes);
                progress(downloaded_before, package.size_bytes);
                continue;
            }
            if piece_path.exists() {
                let _ = fs::remove_file(piece_path);
            }
            let mut piece_progress =
                |bytes, _| progress(downloaded_before.saturating_add(bytes), package.size_bytes);
            if let Err(error) = download_attempt(
                piece,
                piece_path,
                package.size_bytes,
                cancelled.clone(),
                &mut piece_progress,
            ) {
                failed = Some(error);
                break;
            }
            if let Err(error) = verify_piece(piece_path, piece) {
                let _ = fs::remove_file(piece_path);
                failed = Some(error);
                break;
            }
            downloaded_before = downloaded_before.saturating_add(piece.size_bytes);
        }
        if let Some(error) = failed {
            if cancelled.load(Ordering::Relaxed) {
                cleanup_partial_files(&partial, &piece_paths);
                return Err(error).context("PS4 update download cancelled");
            }
            if attempt == 3 {
                return Err(error).context("PS4 update download failed after retries");
            }
            continue;
        }
        let result = assemble_pieces(&piece_paths, &package.pieces, &partial);
        match result {
            Ok(()) => {
                if let Err(error) = verify_download(&partial, package) {
                    cleanup_partial_files(&partial, &piece_paths);
                    return Err(error).context("downloaded PS4 update failed package validation");
                }
                fs::rename(&partial, &destination).with_context(|| {
                    format!(
                        "could not finalize update cache file {}",
                        destination.display()
                    )
                })?;
                for piece_path in &piece_paths {
                    let _ = fs::remove_file(piece_path);
                }
                return Ok(destination);
            }
            Err(error) => {
                if cancelled.load(Ordering::Relaxed) {
                    cleanup_partial_files(&partial, &piece_paths);
                    return Err(error).context("PS4 update download cancelled");
                }
                if attempt == 3 {
                    return Err(error).context("PS4 update download failed after retries");
                }
            }
        }
    }
    unreachable!("bounded retry loop returns on success or final failure")
}

fn download_attempt(
    piece: &UpdatePiece,
    partial: &Path,
    package_size: Option<u64>,
    cancelled: Arc<AtomicBool>,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("could not initialize PS4 update downloader")?;
    runtime.block_on(download_attempt_async(
        piece,
        partial,
        package_size,
        cancelled,
        progress,
    ))
}

async fn download_attempt_async(
    piece: &UpdatePiece,
    partial: &Path,
    package_size: Option<u64>,
    cancelled: Arc<AtomicBool>,
    progress: &mut impl FnMut(u64, Option<u64>),
) -> Result<()> {
    let url = validate_sony_update_url(&piece.url)?;
    let existing = if partial.is_file() {
        fs::metadata(partial)?.len()
    } else {
        0
    };
    ensure!(
        existing <= piece.size_bytes,
        "partial update piece is larger than the manifest size"
    );
    if existing == piece.size_bytes {
        return Ok(());
    }
    let mut request = download_client()?.get(url.as_str());
    if existing > 0 {
        request = request.header(RANGE, format!("bytes={existing}-"));
    }
    let response = request.send().await.map_err(|error| {
        network_error(
            error,
            "could not download the official PS4 update package piece",
            url.as_str(),
        )
    })?;
    ensure!(
        response.url() == &url,
        "Sony update download redirected away from its catalog URL"
    );
    let append = match response.status() {
        reqwest::StatusCode::OK => false,
        reqwest::StatusCode::PARTIAL_CONTENT => {
            let value = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .context("Sony range response is missing Content-Range")?;
            let (start, end, total) = parse_content_range(value)?;
            ensure!(
                start == existing
                    && end >= start
                    && end.checked_add(1) == Some(total)
                    && total == piece.size_bytes,
                "Sony range response does not match the requested update piece range"
            );
            if existing > 0 { true } else { false }
        }
        status => bail!("update request failed with HTTP {status} for {url}"),
    };
    if let Some(content_length) = response.content_length() {
        let expected_remaining = if append {
            piece.size_bytes - existing
        } else {
            piece.size_bytes
        };
        ensure!(
            content_length == expected_remaining,
            "update piece response length differs from the published size"
        );
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(partial)?;
    let mut downloaded = if append { existing } else { 0 };
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
            downloaded <= piece.size_bytes,
            "update piece exceeded the published size"
        );
        file.write_all(&chunk)?;
        progress(downloaded, package_size);
    }
    file.flush()?;
    file.sync_all()?;
    ensure!(
        downloaded == piece.size_bytes,
        "update piece ended early ({downloaded} of {} bytes)",
        piece.size_bytes
    );
    Ok(())
}

fn verify_download(path: &Path, package: &UpdatePackage) -> Result<()> {
    let metadata = fs::metadata(path)?;
    ensure!(
        metadata.len() >= 0x1000,
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
    let mut header = [0_u8; 0x1000];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut header)?;
    let header_digest = &header[0xFE0..0x1000];
    let expected_digest = package
        .package_digest
        .as_deref()
        .context("missing Sony package-header digest")?;
    let actual_digest = header_digest
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    ensure!(
        actual_digest.eq_ignore_ascii_case(expected_digest),
        "downloaded PKG header digest does not match Sony's TLS-authenticated catalog"
    );
    let content_id = crate::forge::detector::parse_pkg_content_id(&header)
        .context("downloaded PS4 update has no valid package content ID")?;
    ensure!(
        content_id.eq_ignore_ascii_case(&package.content_id),
        "downloaded PS4 update content ID does not match update metadata"
    );
    let title_id = crate::forge::detector::title_id_from_content_id(&content_id)
        .context("downloaded PS4 update has no valid title ID")?;
    ensure!(
        title_id.eq_ignore_ascii_case(&package.title_id),
        "downloaded PS4 update title ID does not match selected game"
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

#[cfg(test)]
fn parse_update_xml(title_id: &str, metadata_url: &str, xml: &[u8]) -> Result<LatestUpdate> {
    let mut updates = parse_update_xml_entries(title_id, metadata_url, xml)?;
    sort_latest_updates_newest_first(&mut updates);
    updates
        .into_iter()
        .next()
        .context("Sony update feed has no package entry")
}

fn parse_update_xml_entries(
    title_id: &str,
    metadata_url: &str,
    xml: &[u8],
) -> Result<Vec<LatestUpdate>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut root_seen = false;
    let mut in_paramsfo = false;
    let mut title = None;
    let mut packages = Vec::new();
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
                } else if name.as_ref() == b"package" {
                    let mut version = None;
                    let mut content_id = None;
                    let mut manifest_url = None;
                    let mut size_bytes = None;
                    let mut package_digest = None;
                    let mut system_ver = None;
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
                            b"digest" => package_digest = Some(value.trim().to_ascii_uppercase()),
                            b"system_ver" => system_ver = Some(value),
                            _ => {}
                        }
                    }
                    packages.push((
                        version,
                        content_id,
                        manifest_url,
                        size_bytes,
                        package_digest,
                        system_ver,
                    ));
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
    ensure!(
        !packages.is_empty(),
        "Sony update feed has no package entry"
    );
    packages
        .into_iter()
        .enumerate()
        .map(
            |(
                index,
                (version, content_id, manifest_url, size_bytes, package_digest, system_ver),
            )| {
                let version =
                    version.with_context(|| format!("Sony update entry {index} has no version"))?;
                ensure!(
                    is_safe_update_version(&version),
                    "Sony update entry {index} has an invalid version"
                );
                let content_id = content_id
                    .with_context(|| format!("Sony update entry {index} has no content ID"))?;
                ensure!(
                    content_id
                        .to_ascii_uppercase()
                        .contains(&title_id.to_ascii_uppercase()),
                    "Sony update content ID does not match base-game title ID {title_id}"
                );
                let manifest_url =
                    validate_sony_update_url(&manifest_url.with_context(|| {
                        format!("Sony update entry {index} has no manifest URL")
                    })?)?
                    .into();
                let package_digest = package_digest.filter(|digest| is_hex_digest(digest, 64));
                if let Some(size) = size_bytes {
                    ensure!(
                        size > 0 && size <= MAX_UPDATE_BYTES,
                        "update size is outside supported limits"
                    );
                }
                Ok(LatestUpdate {
                    title_id: title_id.to_owned(),
                    title: title.clone(),
                    version,
                    content_id,
                    size_bytes,
                    required_firmware: system_ver
                        .as_deref()
                        .and_then(FirmwareVersion::from_system_ver_decimal),
                    package_digest,
                    manifest_url,
                    metadata_url: metadata_url.to_owned(),
                })
            },
        )
        .collect()
}

fn sort_latest_updates_newest_first(updates: &mut [LatestUpdate]) {
    updates.sort_by(|left, right| {
        compare_update_versions(&right.version, &left.version).unwrap_or(std::cmp::Ordering::Equal)
    });
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

/// Preserve Sony's advertised scheme. Sony's authenticated catalog currently
/// advertises HTTP PlayGo manifests and pieces; the app accepts them only after
/// cross-checking the manifest against the TLS catalog and verifying every
/// piece before using the PKG.
fn validate_sony_update_url(raw: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw).context("Sony update metadata contains an invalid URL")?;
    ensure!(
        url.scheme() == "https" || url.scheme() == "http",
        "unsupported update URL scheme"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Sony update URL must not contain credentials"
    );
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    ensure!(
        host == "playstation.net" || host.ends_with(".playstation.net"),
        "update source is not hosted by playstation.net: {host}"
    );
    Ok(url)
}

fn checked_update_response(response: Response, request_url: &reqwest::Url) -> Result<Response> {
    ensure!(
        response.status().is_success(),
        "update request failed with HTTP {} for {request_url}",
        response.status()
    );
    ensure!(
        response.url().host_str() == request_url.host_str(),
        "Sony update request redirected to another host"
    );
    ensure!(
        response.url().scheme() == request_url.scheme(),
        "Sony update request changed transport scheme"
    );
    Ok(response)
}

fn update_transfer_client() -> Result<Client> {
    Client::builder()
        .add_root_certificate(sony_root_certificate()?)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(90))
        .user_agent(concat!("DigitalForge/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("could not initialize Sony update manifest client")
}

fn parse_content_range(value: &str) -> Result<(u64, u64, u64)> {
    let value = value
        .strip_prefix("bytes ")
        .context("invalid Content-Range unit")?;
    let (range, total) = value.split_once('/').context("invalid Content-Range")?;
    let (start, end) = range.split_once('-').context("invalid Content-Range")?;
    Ok((start.parse()?, end.parse()?, total.parse()?))
}

fn is_hex_digest(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn verify_piece(path: &Path, piece: &UpdatePiece) -> Result<()> {
    ensure!(
        fs::metadata(path)?.len() == piece.size_bytes,
        "downloaded update piece size does not match Sony's manifest"
    );
    let mut file = File::open(path)?;
    let mut hash = Sha1::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let actual = format!("{:X}", hash.finalize());
    ensure!(
        actual.eq_ignore_ascii_case(&piece.sha1),
        "downloaded update piece SHA-1 does not match Sony's manifest"
    );
    Ok(())
}

fn verify_download_pieces(path: &Path, pieces: &[UpdatePiece]) -> Result<()> {
    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    for piece in pieces {
        file.seek(SeekFrom::Start(piece.file_offset))?;
        let mut remaining = piece.size_bytes;
        let mut hash = Sha1::new();
        while remaining > 0 {
            let limit = buffer.len().min(remaining as usize);
            let count = file.read(&mut buffer[..limit])?;
            ensure!(count > 0, "cached update ended inside a manifest piece");
            hash.update(&buffer[..count]);
            remaining -= count as u64;
        }
        let actual = format!("{:X}", hash.finalize());
        ensure!(
            actual.eq_ignore_ascii_case(&piece.sha1),
            "cached update piece SHA-1 does not match Sony's manifest"
        );
    }
    Ok(())
}

fn assemble_pieces(paths: &[PathBuf], pieces: &[UpdatePiece], output: &Path) -> Result<()> {
    ensure!(
        paths.len() == pieces.len(),
        "update piece path count is inconsistent"
    );
    let mut output_file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(output)?;
    for (path, piece) in paths.iter().zip(pieces) {
        verify_piece(path, piece)?;
        let mut input = File::open(path)?;
        std::io::copy(&mut input, &mut output_file)?;
    }
    output_file.flush()?;
    output_file.sync_all()?;
    Ok(())
}

fn cleanup_partial_files(partial: &Path, pieces: &[PathBuf]) {
    let _ = fs::remove_file(partial);
    for piece in pieces {
        let _ = fs::remove_file(piece);
    }
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

fn network_error(error: reqwest::Error, action: &str, url: &str) -> anyhow::Error {
    if is_tls_hostname_mismatch(&error) {
        anyhow::anyhow!(
            "{action}: TLS certificate hostname validation failed for {url}; Digital Forge refused the connection"
        )
    } else {
        anyhow::Error::new(error).context(format!("{action} for {url}"))
    }
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
        // Sony's catalog intentionally publishes HTTP PlayGo URLs. Never
        // follow redirects for them: an unexpected host/scheme change fails.
        .redirect(reqwest::redirect::Policy::none())
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

    const XML: &[u8] = br#"<?xml version="1.0"?><titlepatch titleid="CUSA00001"><tag><package version="01.07" content_id="IP9100-CUSA00001_00-PLAYROOM00000000" size="123456" digest="0000000000000000000000000000000000000000000000000000000000000000" system_ver="150994944" manifest_url="http://gs2.ww.prod.dl.playstation.net/update.json"><paramsfo><title>THE PLAYROOM</title></paramsfo></package></tag></titlepatch>"#;

    #[test]
    fn sony_root_ca_is_valid_pem_with_the_pinned_fingerprint() {
        assert!(sony_root_certificate().is_ok());
    }

    #[test]
    #[ignore = "downloads the live public CUSA00552 update from Sony"]
    fn live_sony_update_download_and_pkg_validation() {
        let update = latest_update("CUSA00552").expect("live Sony feed request must succeed");
        let update = update.expect("Sony should publish an update for the fixture title");
        eprintln!(
            "Sony update catalog: {} — {} ({:?} bytes)",
            update.title_id, update.version, update.size_bytes
        );
        let package =
            update_package(&update).expect("Sony manifest must pass authenticated validation");
        eprintln!(
            "Sony package manifest accepted; package size {:?} bytes",
            package.size_bytes
        );
        assert_eq!(update.title_id, "CUSA00552");
        assert!(!update.version.is_empty());
        let cache =
            std::env::temp_dir().join(format!("digital-forge-live-update-{}", std::process::id()));
        let path = download_update(
            &update,
            &package,
            &cache,
            Arc::new(AtomicBool::new(false)),
            |done, total| {
                if done == total.unwrap_or_default() || done % (64 * 1024 * 1024) < 1024 * 1024 {
                    eprintln!("Sony update download: {done}/{total:?} bytes");
                }
            },
        )
        .expect("Sony update must download and pass PKG validation");
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            package.size_bytes.unwrap()
        );
        verify_download(&path, &package).expect("cached update must pass package verification");
        eprintln!("Verified live Sony update PKG at {}", path.display());
    }

    #[test]
    #[ignore = "queries the live Sony catalog for one public title"]
    fn live_sony_catalog_includes_a_firmware_requirement() {
        let updates =
            sony_updates_for_title("CUSA00552").expect("live Sony catalog request must succeed");
        assert!(
            !updates.is_empty(),
            "Sony should publish updates for the fixture title"
        );
        eprintln!(
            "Sony listed {} update package version(s) for CUSA00552",
            updates.len()
        );
        for update in &updates {
            eprintln!(
                "  v{} requires firmware {:?} ({} bytes)",
                update.version,
                update.required_firmware,
                update.size_bytes.unwrap_or_default()
            );
        }
        assert!(
            updates
                .iter()
                .any(|update| update.required_firmware.is_some()),
            "Sony titlepatch entries should expose system_ver for compatibility filtering"
        );
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
        assert_eq!(
            update.required_firmware.as_ref().unwrap().to_string(),
            "9.00"
        );
        assert_eq!(
            update.package_digest.as_deref(),
            Some("0".repeat(64).as_str())
        );
    }

    #[test]
    fn firmware_versions_parse_numerically_and_reject_invalid_input() {
        let fw_9 = FirmwareVersion::parse("9.00").unwrap();
        let fw_11 = FirmwareVersion::parse("11.00").unwrap();
        let fw_1152 = FirmwareVersion::parse("11.52").unwrap();
        assert!(fw_9 < fw_11);
        assert!(fw_1152.compatible_with(&FirmwareVersion::parse("12.00").unwrap()));
        assert!(!fw_1152.compatible_with(&fw_11));
        assert_eq!(FirmwareVersion::parse("11.52.0").unwrap(), fw_1152);
        for invalid in ["", "9", "11.x", "11.100", "11.52.1.2.3", "9..00"] {
            assert!(
                FirmwareVersion::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn malformed_or_missing_sony_firmware_requirement_stays_unknown() {
        assert!(FirmwareVersion::from_system_ver_decimal("0").is_none());
        assert!(FirmwareVersion::from_system_ver_decimal("not-a-number").is_none());
        assert!(FirmwareVersion::from_system_ver_decimal("217710592").is_none());
        assert_eq!(
            FirmwareVersion::from_system_ver_decimal("189923328")
                .unwrap()
                .to_string(),
            "11.52"
        );
    }

    #[test]
    fn update_filter_requires_known_firmware_and_compares_components_numerically() {
        let mut older = manifest_fixture_update();
        older.version = "01.08".into();
        older.required_firmware = Some(FirmwareVersion::parse("9.00").unwrap());
        let mut equal = manifest_fixture_update();
        equal.version = "01.09".into();
        equal.required_firmware = Some(FirmwareVersion::parse("11.52").unwrap());
        let mut newer = manifest_fixture_update();
        newer.version = "01.10".into();
        newer.required_firmware = Some(FirmwareVersion::parse("12.00").unwrap());
        let unknown = manifest_fixture_update();
        let updates = [older, equal, newer, unknown];

        let compatible = compatible_updates(&updates, &FirmwareVersion::parse("11.52").unwrap());
        assert_eq!(compatible.len(), 2);
        assert_eq!(compatible[0].version, "01.08");
        assert_eq!(compatible[1].version, "01.09");
        assert!(
            compatible_updates(&updates, &FirmwareVersion::parse("9.00").unwrap())
                .iter()
                .all(|update| update
                    .required_firmware
                    .as_ref()
                    .is_some_and(|required| required.to_string() == "9.00"))
        );
    }

    #[test]
    fn sony_title_xml_parses_multiple_versions_and_filters_firmware_numerically() {
        let xml = br#"<?xml version="1.0"?>
            <titlepatch titleid="CUSA00001"><tag>
              <package version="01.9" content_id="IP9100-CUSA00001_00-PLAYROOM00000000"
                size="1900" system_ver="150994944" manifest_url="http://gs2.ww.prod.dl.playstation.net/v19.json">
                <paramsfo><title>THE PLAYROOM</title></paramsfo>
              </package>
              <package version="01.10" content_id="IP9100-CUSA00001_00-PLAYROOM00000000"
                size="3100" system_ver="184549376" manifest_url="http://gs2.ww.prod.dl.playstation.net/v110.json"/>
              <package version="01.11" content_id="IP9100-CUSA00001_00-PLAYROOM00000000"
                size="4100" manifest_url="http://gs2.ww.prod.dl.playstation.net/v111.json"/>
            </tag></titlepatch>"#;
        let mut updates = parse_update_xml_entries("CUSA00001", "https://example", xml).unwrap();
        sort_latest_updates_newest_first(&mut updates);
        assert_eq!(
            updates
                .iter()
                .map(|update| update.version.as_str())
                .collect::<Vec<_>>(),
            ["01.11", "01.10", "01.9"]
        );
        assert_eq!(updates[0].required_firmware, None);
        assert_eq!(
            updates[1].required_firmware.as_ref().unwrap().to_string(),
            "11.00"
        );
        assert_eq!(
            updates[2].required_firmware.as_ref().unwrap().to_string(),
            "9.00"
        );
        let compatible = updates
            .iter()
            .filter(|update| {
                update.required_firmware.as_ref().is_some_and(|required| {
                    required.compatible_with(&FirmwareVersion::parse("9.00").unwrap())
                })
            })
            .map(|update| update.version.as_str())
            .collect::<Vec<_>>();
        assert_eq!(compatible, ["01.9"]);
    }

    #[test]
    fn itemzflow_patch_catalogue_parses_all_versions_sorts_numeric_and_preserves_fields() {
        let json = br#"{
            "patches": [
                {"version":"1.10","size":3100,"endpoint":"https://updates.example/v110.pkg","required_fw":"11.00","note":"newer"},
                {"version":"1.8","size":1800,"endpoint":"https://updates.example/v18.pkg","required_fw":150994944,"metadata":{"region":"US"}},
                {"version":"1.9","size":"1900","endpoint":"https://updates.example/v19.pkg","note":"firmware omitted"}
            ]
        }"#;
        let updates = parse_itemzflow_patches("CUSA00001", json).unwrap();
        assert_eq!(
            updates
                .iter()
                .map(|update| update.version.as_str())
                .collect::<Vec<_>>(),
            ["1.10", "1.9", "1.8"]
        );
        assert_eq!(updates[0].source, UpdateCatalogueSource::ItemzflowPatches);
        assert_eq!(
            updates[0].required_firmware,
            Some(FirmwareVersion::parse("11.00").unwrap())
        );
        assert_eq!(updates[0].size_bytes, Some(3100));
        assert_eq!(
            updates[0].endpoint.as_deref(),
            Some("https://updates.example/v110.pkg")
        );
        assert_eq!(updates[0].raw_metadata["note"], "newer");
        assert_eq!(updates[1].required_firmware, None);
        assert_eq!(
            updates[2].required_firmware,
            Some(FirmwareVersion::parse("9.00").unwrap())
        );
        assert!(updates.iter().all(|update| update.download_info.is_none()));

        let compatible = compatible_catalogue_updates(
            &updates,
            "CUSA00001",
            &FirmwareVersion::parse("9.00").unwrap(),
        );
        assert_eq!(compatible.len(), 1);
        assert_eq!(compatible[0].version, "1.8");
        assert!(
            compatible_catalogue_updates(
                &updates,
                "CUSA99999",
                &FirmwareVersion::parse("12.00").unwrap(),
            )
            .is_empty()
        );
        let mut downloadable_update = manifest_fixture_update();
        downloadable_update.required_firmware = Some(FirmwareVersion::parse("9.00").unwrap());
        let sony_candidate = UpdateCatalogueEntry::from_sony(downloadable_update);
        assert_eq!(
            selectable_update_indices(
                &[sony_candidate],
                "CUSA00001",
                &FirmwareVersion::parse("12.00").unwrap(),
            ),
            [0]
        );
    }

    #[test]
    fn itemzflow_patch_catalogue_rejects_bad_schema_firmware_and_title_identity() {
        for json in [
            br#"{}"#.as_slice(),
            br#"{"patches":[{"size":10,"endpoint":"https://x.test/a","required_fw":"9.00"}]}"#,
            br#"{"patches":[{"version":"1.0","endpoint":"https://x.test/a","required_fw":"9.00"}]}"#,
            br#"{"patches":[{"version":"1.0","size":10,"endpoint":"https://x.test/a","required_fw":"unknown"}]}"#,
            br#"{"patches":[{"title_id":"CUSA99999","version":"1.0","size":10,"endpoint":"https://x.test/a"}]}"#,
        ] {
            assert!(parse_itemzflow_patches("CUSA00001", json).is_err());
        }
    }

    #[test]
    fn update_version_order_uses_numeric_components() {
        assert_eq!(
            compare_update_versions("01.10", "01.9"),
            Some(std::cmp::Ordering::Greater)
        );
        assert_eq!(
            compare_update_versions("1.2.0", "1.2"),
            Some(std::cmp::Ordering::Equal)
        );
        assert_eq!(compare_update_versions("../1", "1.0"), None);
    }

    #[test]
    fn itemzflow_numeric_required_firmware_uses_packed_orbis_version_and_zero_is_unknown() {
        let json = br#"{"patches":[
            {"version":"1.1","size":10,"endpoint":"https://example.invalid/a.pkg","required_fw":189923328},
            {"version":"1.2","size":10,"endpoint":"https://example.invalid/b.pkg","required_fw":0}
        ]}"#;
        let updates = parse_itemzflow_patches("CUSA00001", json).unwrap();
        let version_11 = updates
            .iter()
            .find(|update| update.version == "1.1")
            .unwrap();
        let version_12 = updates
            .iter()
            .find(|update| update.version == "1.2")
            .unwrap();
        assert_eq!(
            version_11.required_firmware,
            Some(FirmwareVersion::parse("11.52").unwrap())
        );
        assert_eq!(version_12.required_firmware, None);
        assert_eq!(version_11.raw_metadata["required_fw"], 189923328);
    }

    fn manifest_fixture_update() -> LatestUpdate {
        LatestUpdate {
            title_id: "CUSA00001".into(),
            title: None,
            version: "01.07".into(),
            content_id: "IP9100-CUSA00001_00-PLAYROOM00000000".into(),
            size_bytes: Some(10),
            required_firmware: None,
            package_digest: Some("A".repeat(64)),
            manifest_url: "http://gs2.ww.prod.dl.playstation.net/update.json".into(),
            metadata_url: "https://gs-sec.ww.np.dl.playstation.net/update.xml".into(),
        }
    }

    #[test]
    fn sony_manifest_parses_piece_metadata_and_requires_catalog_digest_match() {
        let update = manifest_fixture_update();
        let manifest = format!(
            r#"{{"originalFileSize":10,"packageDigest":"{}","numberOfSplitFiles":2,"pieces":[{{"url":"http://gs2.ww.prod.dl.playstation.net/piece0.pkg","fileOffset":0,"fileSize":4,"hashValue":"{}"}},{{"url":"http://gs2.ww.prod.dl.playstation.net/piece1.pkg","fileOffset":4,"fileSize":6,"hashValue":"{}"}}]}}"#,
            "A".repeat(64),
            "B".repeat(40),
            "C".repeat(40)
        );
        let parsed = parse_update_manifest(
            &update,
            &reqwest::Url::parse(&update.manifest_url).unwrap(),
            manifest.as_bytes(),
        )
        .unwrap();
        assert_eq!(parsed.size_bytes, Some(10));
        assert_eq!(parsed.pieces.len(), 2);
        assert_eq!(parsed.pieces[1].file_offset, 4);
        assert_eq!(
            parsed.package_digest.as_deref(),
            Some("A".repeat(64).as_str())
        );
    }

    #[test]
    fn sony_manifest_rejects_digest_mismatch_gaps_and_cross_host_pieces() {
        let update = manifest_fixture_update();
        let url = reqwest::Url::parse(&update.manifest_url).unwrap();
        let valid = format!(
            r#"{{"originalFileSize":10,"packageDigest":"{}","numberOfSplitFiles":1,"pieces":[{{"url":"http://gs2.ww.prod.dl.playstation.net/piece.pkg","fileOffset":0,"fileSize":10,"hashValue":"{}"}}]}}"#,
            "A".repeat(64),
            "B".repeat(40)
        );
        assert!(
            parse_update_manifest(
                &update,
                &url,
                valid.replace(&"A".repeat(64), &"D".repeat(64)).as_bytes()
            )
            .is_err()
        );
        assert!(
            parse_update_manifest(
                &update,
                &url,
                valid
                    .replace("\"fileOffset\":0", "\"fileOffset\":1")
                    .as_bytes()
            )
            .is_err()
        );
        assert!(
            parse_update_manifest(
                &update,
                &url,
                valid
                    .replace(
                        "gs2.ww.prod.dl.playstation.net/piece.pkg",
                        "evil.example/piece.pkg"
                    )
                    .as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn parses_http_range_response_metadata() {
        assert_eq!(
            parse_content_range("bytes 100-199/200").unwrap(),
            (100, 199, 200)
        );
        assert!(parse_content_range("items 100-199/200").is_err());
        assert!(parse_content_range("bytes 100/200").is_err());
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
    fn non_sony_urls_are_rejected_and_advertised_http_is_preserved() {
        assert!(validate_playstation_url("https://example.com/update.pkg").is_err());
        assert_eq!(
            validate_sony_update_url("http://gs2.ww.prod.dl.playstation.net/a.pkg")
                .unwrap()
                .as_str(),
            "http://gs2.ww.prod.dl.playstation.net/a.pkg"
        );
    }

    #[test]
    fn downloaded_pkg_validation_checks_magic_size_and_digest() {
        let path = std::env::temp_dir().join(format!(
            "digital-forge-ps4-update-check-{}.pkg",
            std::process::id()
        ));
        let mut bytes = vec![0_u8; 0x1000];
        bytes[..4].copy_from_slice(b"\x7fCNT");
        bytes[0x40..0x40 + 35].copy_from_slice(b"UP9000-CUSA00001_00-PLAYROOM0000000");
        let header_digest = "AB".repeat(32);
        for (index, chunk) in header_digest.as_bytes().chunks_exact(2).enumerate() {
            bytes[0xFE0 + index] =
                u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
        }
        fs::write(&path, &bytes).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let package = UpdatePackage {
            url: "http://gs2.ww.prod.dl.playstation.net/update.pkg".to_owned(),
            size_bytes: Some(bytes.len() as u64),
            package_digest: Some(header_digest),
            sha256: Some(digest),
            title_id: "CUSA00001".to_owned(),
            content_id: "UP9000-CUSA00001_00-PLAYROOM0000000".to_owned(),
            pieces: vec![],
        };
        assert!(verify_download(&path, &package).is_ok());
        let mut bad_package = package;
        bad_package.sha256 = Some("0".repeat(64));
        assert!(verify_download(&path, &bad_package).is_err());
        let _ = fs::remove_file(path);
    }

    #[test]
    #[ignore = "uses the locally cached, live Sony CUSA00552 update PKG"]
    fn cached_live_sony_update_matches_feed_and_piece_hashes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".tmp/CUSA00552-A0111-update.pkg");
        let update = LatestUpdate {
            title_id: "CUSA00552".into(),
            title: Some("The Last of Us Remastered".into()),
            version: "01.11".into(),
            content_id: "UP9000-CUSA00552_00-THELASTOFUS00000".into(),
            size_bytes: Some(369_229_824),
            required_firmware: None,
            package_digest: Some(
                "83AA95883144F786C3BEACBCD72F158214B8D57B6E5D94A01DCD132B922BE398".into(),
            ),
            manifest_url: String::new(),
            metadata_url: String::new(),
        };
        let package = UpdatePackage {
            url: String::new(),
            size_bytes: update.size_bytes,
            package_digest: update.package_digest.clone(),
            sha256: None,
            title_id: update.title_id.clone(),
            content_id: update.content_id.clone(),
            pieces: vec![UpdatePiece {
                url: String::new(),
                file_offset: 0,
                size_bytes: 369_229_824,
                sha1: "1DC7552C27228AD69C297EE1A1A4FEE5C2AB67CA".into(),
            }],
        };
        verify_download(&path, &package)
            .expect("real Sony update should pass PKG identity validation");
        verify_piece(&path, &package.pieces[0])
            .expect("real Sony update should match its piece hash");
        verify_download_pieces(&path, &package.pieces)
            .expect("cached Sony update should recheck its piece hash");
    }
}
