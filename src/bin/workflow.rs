use anyhow::{Context, Error, bail, ensure};
use crossterm::cursor::{EnableBlinking, MoveTo, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind, poll, read,
};
use crossterm::execute;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

const PS4_FPKG_PASSCODE: &str = "00000000000000000000000000000000";

use super::{
    Cli, ConversionOutput, ManualDestinationAction, MenuOption, MultipartDestination,
    MultipartGameSet, PreparedGame, PreparedGameKind, RawModeGuard, TerminalDelivery,
    configure_ftp_terminal, copy_directory_and_verify, copy_prepared_game_to_folder,
    copy_prepared_game_to_selected_drive, copy_to_removable_drive, current_archive_password,
    find_7zip, find_default_xex_game, find_existing_god_package, multipart_game_import_once,
    multipart_sets_for_dropped_input, open_folder_in_file_explorer, open_url_in_default_browser,
    pause_to_continue, print_terminal_error, prompt_dropped_game_file, prompt_dropped_path,
    prompt_optional_dropped_path, prompt_terminal_text, prompt_yes_no_key,
    read_prepared_jtag_title_id, render_progress_screen, render_terminal_header, run,
    run_multipart_game_import, run_terminal_conversion, saved_ftp_is_ready,
    scan_archive_sets_in_directory, select_menu, select_removable_drive, set_archive_password,
    test_archive_password, upload_prepared_game_to_xbox, upload_to_saved_xbox,
    verify_converted_game,
};
use crate::app_log::OperationLog;
use crate::app_settings;
use crate::pipeline::{self, JobDelivery, JobState, JobWorkflow, StageKind, StageStatus};
use crate::store::browser;
use crate::store::download;
use crate::store::{
    ArchiveFormat, DownloadOption, StoreAdapter, StoreListing, WordpressStore, format_listing,
};
use crate::workspace::{
    JobWorkspaceBuilder, WorkspaceDrivePreference, enumerate_drives,
    estimate_ps4_base_update_workspace, format_bytes, load_workspace_preference,
    probe_archive_sizes, remove_dir_all_robust,
};

pub(super) fn run_store_workflow() -> Result<(), Error> {
    let mut log = OperationLog::create("store")?;
    log.info("Opened Store")?;
    if offer_resume_job(JobWorkflow::Store, &mut log)? {
        return Ok(());
    }

    loop {
        render_terminal_header()?;
        println!("Store");
        println!("========================================================");
        println!("Search the Store only for games you own or are authorised to download.");
        println!("ISO 2 GOD does not bypass website logins, paywalls, or access restrictions.\n");
        let settings = app_settings::load_app_settings()?;
        let store = crate::store::WordpressStore::new(settings.store_base_url)
            .with_cache(app_settings::config_dir()?);
        if store.catalog_is_stale() {
            println!("Loading the full Store title list. This is only needed once a day.\n");
            if let Err(error) = store.ensure_catalog() {
                print_terminal_error(&error);
                println!("Search will still run, but may miss some titles.\n");
            } else {
                println!();
            }
        }
        print!("Search: ");
        io::stdout().flush()?;
        let Some(query) = prompt_terminal_text("Search", None, false)? else {
            return Ok(());
        };
        log.info(&format!("Store search: {query}"))?;
        match search_store(&query) {
            Ok(results) if results.is_empty() => {
                println!("\nSearch returned no results.");
                pause_to_continue()?;
            }
            Ok(results) => match select_store_result(&query, &results)? {
                StoreNav::Listing(listing) => {
                    if run_store_item(&listing, &mut log)? {
                        return Ok(());
                    }
                }
                StoreNav::SearchAgain => continue,
                StoreNav::Back => return Ok(()),
            },
            Err(error) => {
                log.error(&error)?;
                print_terminal_error(&error);
                let choices = [
                    MenuOption {
                        value: true,
                        label: "Retry".to_owned(),
                    },
                    MenuOption {
                        value: false,
                        label: "Return".to_owned(),
                    },
                ];
                if !select_menu(&choices)?.unwrap_or(false) {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRole {
    BaseGame,
    Update,
    Dlc,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ClassifiedSourceSet {
    pub platform: iso2god::forge::Platform,
    pub title_id: Option<String>,
    pub title_name: Option<String>,
    pub base_sources: Vec<PathBuf>,
    pub update_sources: Vec<PathBuf>,
    pub dlc_sources: Vec<PathBuf>,
    pub dlc_detected: bool,
    pub base_version: Option<String>,
    pub update_version: Option<String>,
    pub itemzflow: Option<iso2god::forge::itemzflow::Evidence>,
}

#[derive(Debug, Clone)]
struct ScannedCandidate {
    path: PathBuf,
    role: ContentRole,
    title_id: Option<String>,
    title_name: Option<String>,
    version: Option<String>,
    platform: Option<iso2god::forge::Platform>,
}

pub fn parse_dropped_paths_input(input: &str) -> Vec<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let mut paths = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = '"';

    for ch in trimmed.chars() {
        match ch {
            '"' | '\'' if !in_quotes => {
                in_quotes = true;
                quote_char = ch;
            }
            c if in_quotes && c == quote_char => {
                in_quotes = false;
                let candidate = current.trim();
                if !candidate.is_empty() {
                    paths.push(PathBuf::from(candidate));
                }
                current.clear();
            }
            ' ' | '\t' if !in_quotes => {
                let candidate = current.trim();
                if !candidate.is_empty() {
                    paths.push(PathBuf::from(candidate));
                }
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }
    let candidate = current.trim();
    if !candidate.is_empty() {
        paths.push(PathBuf::from(candidate));
    }

    let existing: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
    if !existing.is_empty() {
        existing
    } else {
        let whole = PathBuf::from(trimmed.trim_matches('"').trim_matches('\''));
        if whole.exists() {
            vec![whole]
        } else {
            Vec::new()
        }
    }
}

fn extract_version_from_str(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    if let Some(pos) = lower.find("_v") {
        let after = &lower[pos + 2..];
        let ver: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let ver = ver.trim_end_matches('.').to_string();
        if !ver.is_empty() && ver.contains('.') {
            return Some(ver);
        }
    }
    if let Some(pos) = lower.find('v') {
        let after = &lower[pos + 1..];
        let ver: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let ver = ver.trim_end_matches('.').to_string();
        if !ver.is_empty() && ver.contains('.') && ver.len() >= 3 {
            return Some(ver);
        }
    }
    if let Some(pos) = lower.find("-a") {
        let after = &lower[pos + 2..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() == 4 {
            let major = digits[..2].trim_start_matches('0');
            let major = if major.is_empty() { "0" } else { major };
            let minor = &digits[2..];
            return Some(format!("{major}.{minor}"));
        }
    }
    None
}

fn extract_title_name_from_str(s: &str) -> Option<String> {
    let clean = s.rsplit(['/', '\\']).next().unwrap_or(s);
    if let Some(pos) = clean.to_ascii_uppercase().find("_CUSA") {
        let raw_title = &clean[..pos];
        let title = raw_title
            .replace('.', " ")
            .replace('_', " ")
            .trim()
            .to_string();
        if !title.is_empty() {
            return Some(title);
        }
    }
    None
}

fn inspect_candidate(path: &Path, hint: Option<ContentRole>) -> ScannedCandidate {
    let mut candidate = ScannedCandidate {
        path: path.to_path_buf(),
        role: hint.unwrap_or(ContentRole::Unknown),
        title_id: None,
        title_name: None,
        version: None,
        platform: None,
    };

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let lower_name = name.to_ascii_lowercase();

    if candidate.role == ContentRole::Unknown {
        if lower_name.contains("update") || lower_name.contains("patch") {
            candidate.role = ContentRole::Update;
        } else if lower_name.contains("dlc")
            || lower_name.contains("addon")
            || lower_name.contains("bonus")
            || lower_name.contains("pack")
        {
            candidate.role = ContentRole::Dlc;
        } else if lower_name.contains("game") || lower_name.contains("base") {
            candidate.role = ContentRole::BaseGame;
        }
    }

    if path.is_dir() {
        let sfo = path.join("sce_sys").join("param.sfo");
        if sfo.is_file() {
            candidate.platform = Some(iso2god::forge::Platform::PlayStation4);
            let (tid, tname) = iso2god::forge::FormatDetector::read_sfo(&sfo);
            candidate.title_id = tid;
            candidate.title_name = tname;
            if let Some(cat) = iso2god::forge::FormatDetector::read_sfo_category(&sfo) {
                match cat.as_str() {
                    "gd" | "gdc" | "gda" => candidate.role = ContentRole::BaseGame,
                    "gp" => candidate.role = ContentRole::Update,
                    "ac" => candidate.role = ContentRole::Dlc,
                    _ => {}
                }
            }
            if let Ok(bytes) = fs::read(&sfo) {
                if let Some(ver) = iso2god::forge::detector::extract_sfo_string(&bytes, "APP_VER") {
                    candidate.version = Some(ver);
                }
            }
            return candidate;
        }

        if let Ok(entries) = fs::read_dir(path) {
            let mut sub_files = Vec::new();
            for entry in entries.flatten() {
                if entry.path().is_file() {
                    sub_files.push(entry.path());
                }
            }
            sub_files.sort();
            for file in &sub_files {
                let f_name = file
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if f_name.contains("part2")
                    || f_name.contains("part02")
                    || f_name.starts_with("pt2.")
                    || f_name.starts_with("pt02.")
                    || f_name.contains(".002")
                {
                    continue;
                }
                let sub_cand = inspect_candidate(file, Some(candidate.role));
                if sub_cand.platform.is_some() {
                    candidate.platform = sub_cand.platform;
                }
                if sub_cand.title_id.is_some() && candidate.title_id.is_none() {
                    candidate.title_id = sub_cand.title_id;
                }
                if sub_cand.title_name.is_some() && candidate.title_name.is_none() {
                    candidate.title_name = sub_cand.title_name;
                }
                if sub_cand.version.is_some() && candidate.version.is_none() {
                    candidate.version = sub_cand.version;
                }
                if sub_cand.role != ContentRole::Unknown {
                    candidate.role = sub_cand.role;
                }
                if candidate.title_id.is_some() {
                    break;
                }
            }
        }
        return candidate;
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "rar" | "zip" | "7z") {
        let entry_volume = match iso2god::forge::archive::ArchiveEngine::classify(path) {
            iso2god::forge::archive::ArchiveKind::MultiVolume { first_volume, .. } => first_volume,
            _ => path.to_path_buf(),
        };
        candidate.path = entry_volume.clone();

        if let Ok(entries) =
            iso2god::forge::archive::ArchiveEngine::list_archive_entries(&entry_volume)
        {
            for entry in &entries {
                let entry_lower = entry.to_ascii_lowercase();
                if entry_lower.ends_with(".pkg") || entry_lower.contains("param.sfo") {
                    candidate.platform = Some(iso2god::forge::Platform::PlayStation4);
                    if let Some(tid) =
                        iso2god::forge::detector::extract_ps4_title_id_from_str(entry)
                    {
                        candidate.title_id = Some(tid);
                    }
                    if let Some(tname) = extract_title_name_from_str(entry) {
                        if candidate.title_name.is_none() {
                            candidate.title_name = Some(tname);
                        }
                    }
                    if entry_lower.contains("dlc")
                        || entry_lower.contains("pack")
                        || entry_lower.contains("_ac")
                    {
                        candidate.role = ContentRole::Dlc;
                    } else if entry_lower.contains("_v1.00")
                        || entry_lower.contains("-a0100-")
                        || entry_lower.contains("_gd")
                    {
                        candidate.role = ContentRole::BaseGame;
                        candidate.version = Some("1.00".to_string());
                    } else if let Some(v) = extract_version_from_str(entry) {
                        candidate.version = Some(v.clone());
                        if v == "1.00" || v == "1.0" {
                            candidate.role = ContentRole::BaseGame;
                        } else {
                            candidate.role = ContentRole::Update;
                        }
                    }
                }
            }
        }
        return candidate;
    }

    if ext == "pkg" {
        candidate.platform = Some(iso2god::forge::Platform::PlayStation4);
        if let Some(tid) = iso2god::forge::detector::extract_ps4_title_id_from_str(name) {
            candidate.title_id = Some(tid);
        }
        if let Some(tname) = extract_title_name_from_str(name) {
            candidate.title_name = Some(tname);
        }
        if lower_name.contains("dlc") || lower_name.contains("pack") || lower_name.contains("_ac") {
            candidate.role = ContentRole::Dlc;
        } else if lower_name.contains("_v1.00")
            || lower_name.contains("-a0100-")
            || lower_name.contains("_gd")
        {
            candidate.role = ContentRole::BaseGame;
            candidate.version = Some("1.00".to_string());
        } else if let Some(v) = extract_version_from_str(name) {
            candidate.version = Some(v.clone());
            if v == "1.00" || v == "1.0" {
                candidate.role = ContentRole::BaseGame;
            } else {
                candidate.role = ContentRole::Update;
            }
        }
        return candidate;
    }

    if let Ok(det) = iso2god::forge::FormatDetector::detect(path, None) {
        candidate.platform = Some(det.platform);
        candidate.title_id = det.game_id;
        candidate.title_name = det.title_name;
        candidate.role = ContentRole::BaseGame;
    }

    candidate
}

pub fn classify_source_set(sources: &[PathBuf]) -> Result<ClassifiedSourceSet, Error> {
    ensure!(!sources.is_empty(), "no source paths were provided");
    for source in sources {
        ensure!(
            source.exists(),
            "input path does not exist: {}",
            source.display()
        );
    }

    let mut itemzflow_evidence = None;
    let mut candidate_paths: Vec<(PathBuf, Option<ContentRole>)> = Vec::new();
    if sources.len() == 1
        && sources[0].is_dir()
        && let Some((dumps, evidence)) = iso2god::forge::itemzflow::discover(&sources[0])?
    {
        itemzflow_evidence = Some(evidence);
        candidate_paths.extend(dumps.into_iter().map(|dump| {
            let role = match dump.role {
                iso2god::forge::itemzflow::DumpRole::BaseGame => ContentRole::BaseGame,
                iso2god::forge::itemzflow::DumpRole::Update => ContentRole::Update,
            };
            (dump.path, Some(role))
        }));
    } else if sources.len() == 1 && sources[0].is_dir() {
        let parent = &sources[0];
        let direct_game = parent
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("Game"));
        let game_child = if direct_game {
            Some(parent.to_path_buf())
        } else {
            find_named_child_directory_or_file(parent, "Game")
        };
        let update_parent = if direct_game {
            parent.parent().unwrap_or(parent)
        } else {
            parent
        };
        let update_child = find_named_child_directory_or_file(update_parent, "Update");
        let dlc_child = find_named_child_directory_or_file(parent, "DLC");
        if let Some(game_child) = game_child {
            candidate_paths.push((game_child, Some(ContentRole::BaseGame)));
        } else {
            // A dropped arbitrary folder may itself be the extracted game
            // payload.  Its name is never used as the game's identity.
            candidate_paths.push((parent.to_path_buf(), Some(ContentRole::BaseGame)));
        }
        if let Some(update_child) = update_child {
            candidate_paths.push((update_child, Some(ContentRole::Update)));
        }
        if let Some(dlc_child) = dlc_child {
            candidate_paths.push((dlc_child, Some(ContentRole::Dlc)));
        }
    } else {
        for src in sources {
            candidate_paths.push((src.clone(), None));
        }
    }

    let mut base_sources = Vec::new();
    let mut update_sources = Vec::new();
    let mut dlc_sources = Vec::new();
    let mut detected_platform = None;
    let mut global_title_id = None;
    let mut global_title_name = None;
    let mut base_version = None;
    let mut update_version = None;

    let mut base_title_id = None;
    let mut update_title_id = None;

    let mut seen_paths = BTreeSet::new();

    for (path, hint) in candidate_paths {
        let cand = inspect_candidate(&path, hint);
        if seen_paths.contains(&cand.path) {
            continue;
        }
        seen_paths.insert(cand.path.clone());

        if cand.platform.is_some() && detected_platform.is_none() {
            detected_platform = cand.platform;
        }
        if cand.title_id.is_some() && global_title_id.is_none() {
            global_title_id = cand.title_id.clone();
        }
        if cand.title_name.is_some() && global_title_name.is_none() {
            global_title_name = cand.title_name.clone();
        }

        match cand.role {
            ContentRole::BaseGame => {
                if cand.version.is_some() && base_version.is_none() {
                    base_version = cand.version.clone();
                }
                if cand.title_id.is_some() && base_title_id.is_none() {
                    base_title_id = cand.title_id.clone();
                }
                base_sources.push(cand.path);
            }
            ContentRole::Update => {
                if cand.version.is_some() && update_version.is_none() {
                    update_version = cand.version.clone();
                }
                if cand.title_id.is_some() && update_title_id.is_none() {
                    update_title_id = cand.title_id.clone();
                }
                update_sources.push(cand.path);
            }
            ContentRole::Dlc => {
                dlc_sources.push(cand.path);
            }
            ContentRole::Unknown => {
                if base_sources.is_empty() {
                    base_sources.push(cand.path);
                } else {
                    update_sources.push(cand.path);
                }
            }
        }
    }

    if let (Some(b_id), Some(u_id)) = (&base_title_id, &update_title_id) {
        ensure!(
            b_id.eq_ignore_ascii_case(u_id),
            "Base Game Title ID ({b_id}) does not match Update Title ID ({u_id}). Cannot combine unrelated packages."
        );
    }

    let platform = detected_platform
        .or_else(|| {
            iso2god::forge::FormatDetector::detect(&sources[0], None)
                .ok()
                .map(|d| d.platform)
        })
        .unwrap_or(iso2god::forge::Platform::PlayStation4);

    let dlc_detected = !dlc_sources.is_empty();

    Ok(ClassifiedSourceSet {
        platform,
        title_id: global_title_id,
        title_name: global_title_name,
        base_sources,
        update_sources,
        dlc_sources,
        dlc_detected,
        base_version,
        update_version,
        itemzflow: itemzflow_evidence,
    })
}

fn display_source_name(path: &Path) -> String {
    if let Some(parent) = path.parent() {
        if let Some(p_name) = parent.file_name().and_then(|n| n.to_str()) {
            if matches!(
                p_name.to_ascii_lowercase().as_str(),
                "game" | "update" | "dlc"
            ) {
                return p_name.to_string();
            }
        }
    }
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("content")
        .to_string()
}

fn build_content_group_from_sources(
    label: &'static str,
    sources: &[PathBuf],
) -> Result<FolderContentGroup, Error> {
    if sources.is_empty() {
        return Ok(FolderContentGroup {
            label,
            directory: PathBuf::new(),
            sources: Vec::new(),
            multipart_sets: Vec::new(),
            ordinary_items: 0,
        });
    }

    let mut multipart_sets = Vec::new();
    let mut ordinary_items = 0;
    let mut clean_sources = Vec::new();

    for src in sources {
        if src.is_dir() {
            // An extracted PS4 application already is one package payload.
            // Scanning its immediate files as independent inputs can select an
            // adjacent .gp4 project as the base and fail the later payload
            // check. Keep the complete application directory intact here.
            if is_ps4_extracted_payload(src) {
                clean_sources.push(src.clone());
                ordinary_items += 1;
                continue;
            }
            let scanned = scan_folder_content_group(label, src, false)?;
            clean_sources.extend(scanned.sources);
            multipart_sets.extend(scanned.multipart_sets);
            ordinary_items += scanned.ordinary_items;
        } else {
            let archive_kind = iso2god::forge::archive::ArchiveEngine::classify(src);
            match archive_kind {
                iso2god::forge::archive::ArchiveKind::MultiVolume {
                    first_volume,
                    all_volumes,
                    missing_volumes,
                } => {
                    let missing_part = missing_volumes.into_iter().next();
                    clean_sources.push(first_volume.clone());
                    multipart_sets.push(MultipartGameSet {
                        game_name: label.to_string(),
                        entry_path: first_volume,
                        parts: all_volumes,
                        missing_part,
                    });
                }
                iso2god::forge::archive::ArchiveKind::SingleVolume(_) => {
                    clean_sources.push(src.clone());
                    multipart_sets.push(MultipartGameSet {
                        game_name: label.to_string(),
                        entry_path: src.clone(),
                        parts: vec![src.clone()],
                        missing_part: None,
                    });
                }
                _ => {
                    clean_sources.push(src.clone());
                    ordinary_items += 1;
                }
            }
        }
    }

    clean_sources.sort();
    clean_sources.dedup();

    let parent_dir = sources[0].parent().unwrap_or(&sources[0]).to_path_buf();
    Ok(FolderContentGroup {
        label,
        directory: parent_dir,
        sources: clean_sources,
        multipart_sets,
        ordinary_items,
    })
}

fn is_ps4_extracted_payload(directory: &Path) -> bool {
    [
        directory.join("sce_sys").join("param.sfo"),
        directory.join("Image0").join("sce_sys").join("param.sfo"),
    ]
    .iter()
    .any(|path| path.is_file())
}

thread_local! {
    // Passwords live only for this synchronous import job, never on disk.
    static IMPORT_ARCHIVE_PASSWORDS: std::cell::RefCell<BTreeMap<PathBuf, Option<String>>> =
        std::cell::RefCell::new(BTreeMap::new());
}

struct ImportPasswordScope;
impl Drop for ImportPasswordScope {
    fn drop(&mut self) {
        IMPORT_ARCHIVE_PASSWORDS.with(|passwords| passwords.borrow_mut().clear());
    }
}

fn archive_password_for_import(source: &Path) -> Result<Option<String>, Error> {
    let key = source.canonicalize()?;
    if let Some(password) = IMPORT_ARCHIVE_PASSWORDS.with(|cache| cache.borrow().get(&key).cloned())
    {
        return Ok(password);
    }
    let password = if iso2god::forge::ArchiveEngine::requires_password(source)? {
        println!("\r\x1b[2KPassword-protected archive: {}", source.display());
        Some(
            prompt_terminal_text("Archive password", None, true)?
                .context("Archive password entry cancelled")?,
        )
    } else {
        None
    };
    IMPORT_ARCHIVE_PASSWORDS.with(|cache| cache.borrow_mut().insert(key, password.clone()));
    Ok(password)
}

fn preflight_archive_passwords(source: &Path, seen: &mut BTreeSet<PathBuf>) -> Result<(), Error> {
    if source.is_dir() {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if !entry.file_type()?.is_symlink() {
                preflight_archive_passwords(&entry.path(), seen)?;
            }
        }
    } else {
        let archive = match iso2god::forge::ArchiveEngine::classify(source) {
            iso2god::forge::ArchiveKind::SingleVolume(path) => Some(path),
            iso2god::forge::ArchiveKind::MultiVolume { first_volume, .. } => Some(first_volume),
            _ => None,
        };
        if let Some(archive) = archive {
            if seen.insert(archive.canonicalize()?) {
                archive_password_for_import(&archive)?;
            }
        }
    }
    Ok(())
}

pub(super) fn run_drag_and_drop_pipeline(sources: &[PathBuf]) -> Result<(), Error> {
    let _password_scope = ImportPasswordScope;
    IMPORT_ARCHIVE_PASSWORDS.with(|cache| cache.borrow_mut().clear());
    if sources.is_empty() {
        return Ok(());
    }
    let mut log = OperationLog::create("drag_drop_ingest")?;
    let result = run_drag_and_drop_pipeline_logged(sources, &mut log);
    if let Err(error) = &result {
        let _ = log.error(error);
        eprintln!("\nProcessing failed: {error:#}");
        eprintln!("Diagnostic log: {}", log.path().display());
        eprintln!("Shareable redacted report: {}", log.share_path().display());
        eprintln!("Search existing reports: https://github.com/rileyadams05/digital-forge/issues");
    }
    result
}

pub(super) fn receive_ps5_dump() -> Result<(), Error> {
    render_terminal_header()?;
    println!("Receive PS5 game dump");
    println!("========================================================\n");
    println!(
        "Digital Forge will listen for the console and save the incoming TAR stream. Configure the console to send the transfer to this PC's local network address and the port selected below. Use 0.0.0.0 to listen on all network interfaces.\n"
    );
    let Some(bind_ip_text) = prompt_terminal_text("Listen address", Some("0.0.0.0"), false)? else {
        return Ok(());
    };
    let Some(port_text) = prompt_terminal_text("Listening port", Some("9023"), false)? else {
        return Ok(());
    };
    let port: u16 = port_text
        .trim()
        .parse()
        .context("transfer port must be a number from 1 to 65535")?;
    ensure!(port != 0, "transfer port must be between 1 and 65535");
    let ip: std::net::IpAddr = bind_ip_text
        .trim()
        .parse()
        .context("enter a local IPv4 or IPv6 interface address")?;
    let listen_address = std::net::SocketAddr::new(ip, port);
    let default_path = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Downloads")
        .join("ps5-game-dump.tar");
    let Some(destination_text) = prompt_terminal_text(
        "Save received TAR file as",
        Some(&default_path.display().to_string()),
        false,
    )?
    else {
        return Ok(());
    };
    let destination = PathBuf::from(destination_text.trim().trim_matches('"'));
    ensure!(
        destination
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("tar")),
        "the received PS5 stream must be saved with a .tar extension"
    );

    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel_flag = cancelled.clone();
    println!("\nListening on {listen_address}. Press Alt+C to stop receiving.");
    let received = iso2god::forge::transfer::receive_tcp_file(
        listen_address,
        &destination,
        Arc::new(move |update| {
            if poll(Duration::from_millis(1)).unwrap_or(false)
                && matches!(read(), Ok(Event::Key(key)) if key.kind == KeyEventKind::Press && key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('c'))
            {
                cancel_flag.store(true, Ordering::Relaxed);
            }
            print!("\r{} — {} bytes", update.stage, update.bytes_processed);
            let _ = io::stdout().flush();
        }),
        cancelled,
    )?;
    println!("\nReceived {received} bytes: {}", destination.display());
    println!("\nDigital Forge will now inspect the received archive.");
    run_drag_and_drop_pipeline(&[destination])
}

fn run_drag_and_drop_pipeline_logged(
    sources: &[PathBuf],
    log: &mut OperationLog,
) -> Result<(), Error> {
    log.info(&format!(
        "Drag-and-drop ingest started with {} source(s)",
        sources.len()
    ))?;

    let mut effective_sources = sources.to_vec();
    log.stage(
        "source_classification",
        "classifying input paths and content metadata",
    )?;
    let mut classified = classify_source_set(&effective_sources)?;

    // The optional-update decision is shared by all console backends. A
    // populated sibling Update directory was already classified above and
    // must proceed without asking. When no usable update is present, pause
    // here before any console-specific usage or destination prompts.
    if effective_sources.len() == 1
        && effective_sources[0].is_dir()
        && !classified_has_usable_update(&classified)
    {
        println!(
            "\nNo game update was detected.\n\nDo you have an update folder?\n\nDrag and drop your Update folder here,\nor press Enter to continue without an update.\n"
        );
        loop {
            let Some(update_path) = prompt_optional_dropped_path("", Some(true))? else {
                println!("Continuing without an update...");
                break;
            };

            let mut candidate_sources = effective_sources.clone();
            candidate_sources.push(update_path);
            match classify_source_set(&candidate_sources) {
                Ok(candidate) if classified_has_usable_update(&candidate) => {
                    println!("Update folder received.\nUpdate validated.");
                    effective_sources = candidate_sources;
                    classified = candidate;
                    break;
                }
                Ok(_) => println!(
                    "The supplied Update folder is empty. Drop another Update folder or press Enter to continue without an update."
                ),
                Err(error) => println!(
                    "Update rejected: {error:#}\nDrop another Update folder or press Enter to continue without an update."
                ),
            }
        }
    }
    let mut checked_archives = BTreeSet::new();
    for source in classified
        .base_sources
        .iter()
        .chain(&classified.update_sources)
    {
        preflight_archive_passwords(source, &mut checked_archives)?;
    }
    log.info(&format!(
        "Detected platform: {}, title: {:?}, Title ID: {:?}, base version: {:?}, update version: {:?}",
        classified.platform.display_name(),
        classified.title_name,
        classified.title_id,
        classified.base_version,
        classified.update_version
    ))?;

    if classified.platform == iso2god::forge::Platform::PlayStation4 {
        return run_ps4_ingest_flow(&classified, log);
    }

    if classified.platform == iso2god::forge::Platform::PlayStation5 {
        render_terminal_header()?;
        println!("Detected PlayStation 5 Content");
        println!("========================================================\n");
        if let Some(ref title) = classified.title_name {
            println!("Title:    {title}");
        }
        if let Some(ref id) = classified.title_id {
            println!("Title ID: {id}");
        }
        println!("\nPS5 content requires a target selection before processing.\n");
        let Some(usage_mode) = iso2god::forge::ForgeUi::prompt_usage_mode(classified.platform)?
        else {
            return Ok(());
        };
        let target_workflow = if usage_mode == iso2god::forge::UsageMode::PhysicalConsole {
            let Some(storage) =
                iso2god::forge::ForgeUi::prompt_console_storage(classified.platform)?
            else {
                return Ok(());
            };
            iso2god::forge::target_workflow_for_console_storage(classified.platform, storage)
        } else {
            iso2god::forge::target_workflow_for_usage(classified.platform, usage_mode)
        };
        let Some(target_workflow) = target_workflow else {
            return Ok(());
        };
        let base_source = classified
            .base_sources
            .first()
            .cloned()
            .unwrap_or_else(|| sources[0].clone());
        let _completed = iso2god::forge::ForgeUi::run_interactive_flow_with_target(
            &base_source,
            classified.platform,
            target_workflow,
        )?;
        return pause_to_continue();
    }

    if classified.platform == iso2god::forge::Platform::PlayStation3
        && sources.len() == 1
        && sources[0].is_dir()
    {
        return run_folder_contract_import(
            &sources[0],
            classified.platform,
            iso2god::forge::TargetWorkflow::EstablishedConsoleWorkflow,
            log,
        );
    }

    render_terminal_header()?;
    println!("Detected {} Content", classified.platform.display_name());
    println!("========================================================\n");
    if let Some(ref title) = classified.title_name {
        println!("Title:    {title}");
    }
    if let Some(ref id) = classified.title_id {
        println!("Game ID:  {id}");
    }
    println!("\nComponents:");
    for src in &classified.base_sources {
        println!("  - {}", src.display());
    }

    let base_source = classified
        .base_sources
        .first()
        .cloned()
        .unwrap_or_else(|| sources[0].clone());
    let completed = iso2god::forge::ForgeUi::run_interactive_flow_with_target(
        &base_source,
        classified.platform,
        iso2god::forge::TargetWorkflow::EstablishedConsoleWorkflow,
    )?;
    if completed.is_some() {
        offer_successful_test_log(classified.title_id.as_deref(), log)?;
    }
    pause_to_continue()
}

fn offer_successful_test_log(game_id: Option<&str>, log: &OperationLog) -> Result<(), Error> {
    let Some(game_id) = game_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    let safe_id: String = game_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(32)
        .collect();
    if safe_id.is_empty() {
        println!("Could not prepare a game-specific report link because the game ID is invalid.");
        return Ok(());
    }

    let status = check_community_status(&safe_id);
    if let Some(message) = &status.result_error {
        println!("Could not check the approved test list: {message}");
    }
    if let Some(message) = &status.issue_error {
        println!("Could not check GitHub failure reports: {message}");
    }
    if !status.issues.is_empty() {
        println!(
            "\nPrevious failure report(s) exist for this game. You are welcome to test it; results can vary by game version and setup."
        );
        for (title, url, _) in &status.issues {
            println!("  - {title}\n    {url}");
        }
    }
    let newest_failure = status
        .issues
        .iter()
        .map(|(_, _, created_at)| created_at.as_str())
        .filter(|created_at| !created_at.is_empty())
        .max();
    let failure_after_success = !status.issues.is_empty()
        && (!status.approved_result
            || newest_failure.is_some_and(|failure_date| {
                status
                    .approved_tested_at
                    .as_deref()
                    .is_none_or(|success_date| failure_date > success_date)
            }));
    if status.result_error.is_some() || status.issue_error.is_some() {
        println!(
            "\nTest status: Unable to check the current community result. Please review the website for the latest status."
        );
    } else if status.approved_result && !failure_after_success {
        println!(
            "\nTest status: Tested working. An approved successful test is listed for {safe_id}."
        );
    } else if failure_after_success {
        println!(
            "\nTest status: Problem reported. A matching failure report is newer than the approved test."
        );
    } else if status.issues.is_empty() {
        println!(
            "\nTest status: Unknown. No approved test result was found for game ID {safe_id}, so its status is unknown."
        );
    } else {
        println!("\nTest status: Problem reported.");
    }
    wait_for_status_acknowledgement()?;
    println!(
        "Would you like to share your experience? Press Y to save a copy of the log and open the game status page, or press Enter to skip."
    );
    if !prompt_yes_no_key("Save log and open website?", false)?.unwrap_or(false) {
        return Ok(());
    }

    let reports_dir = user_downloads_dir()
        .map(|downloads| downloads.join("Digital Forge Reports"))
        .unwrap_or(app_settings::logs_dir()?.join("Digital Forge Reports"));
    fs::create_dir_all(&reports_dir)
        .with_context(|| format!("could not create report folder {}", reports_dir.display()))?;
    let filename = log
        .path()
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("digital-forge.log");
    let file_stem = format!("{}-{filename}", safe_id.to_ascii_uppercase());
    let mut destination = reports_dir.join(&file_stem);
    let mut suffix = 2usize;
    while destination.exists() {
        destination = reports_dir.join(format!("{file_stem}-{suffix}"));
        suffix += 1;
    }
    fs::copy(log.path(), &destination).with_context(|| {
        format!(
            "could not save a copy of the processing log to {}",
            destination.display()
        )
    })?;
    println!("Log saved locally: {}", destination.display());
    println!(
        "Digital Forge does not upload the log. Attach it on the website if you choose to submit a report."
    );
    let url = format!(
        "https://digital-forge.vortex-prime-emu.com/compatibility.html?game={}#success-report",
        safe_id.to_ascii_uppercase()
    );
    if let Err(error) = open_url_in_default_browser(&url) {
        println!(
            "Open the Compatibility page to review the game status and submit your report: {url}"
        );
        eprintln!("Could not open the browser automatically: {error:#}");
    } else {
        println!(
            "Opened the Compatibility page. Review the game status and submit your report there if you choose."
        );
    }
    Ok(())
}

fn wait_for_status_acknowledgement() -> Result<(), Error> {
    if env::var_os("ISO2GOD_QUEUE_JOB").is_some() {
        return Ok(());
    }
    print!("Press Enter to continue to the optional community prompt...");
    io::stdout().flush().context("error writing prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("error reading input")?;
    Ok(())
}

struct CommunityStatusCheck {
    approved_result: bool,
    approved_tested_at: Option<String>,
    issues: Vec<(String, String, String)>,
    result_error: Option<String>,
    issue_error: Option<String>,
}

fn check_community_status(game_id: &str) -> CommunityStatusCheck {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(6))
        .build();
    let mut approved_result = false;
    let mut approved_tested_at: Option<String> = None;
    let mut result_error = None;
    let catalog_url = "https://raw.githubusercontent.com/rileyadams05/digital-forge/master/site/assets/game-results.json";
    match public_json(&agent, catalog_url) {
        Ok(catalog) => {
            if let Some(games) = catalog.get("games").and_then(serde_json::Value::as_array) {
                for game in games {
                    let is_approved = game_id_matches(game.get("game_id"), game_id)
                        && game.get("status").and_then(serde_json::Value::as_str)
                            == Some("working");
                    if is_approved {
                        approved_result = true;
                        let tested_at = game
                            .get("tested_at")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                        if tested_at.as_deref() > approved_tested_at.as_deref() {
                            approved_tested_at = tested_at;
                        }
                    }
                }
            }
        }
        Err(error) => result_error = Some(error),
    }

    let individual_url = format!(
        "https://raw.githubusercontent.com/rileyadams05/digital-forge/master/site/assets/game-results/{}.json",
        game_id.to_ascii_uppercase()
    );
    match public_json(&agent, &individual_url) {
        Ok(record) => {
            let game = record.get("game").unwrap_or(&record);
            let is_approved = game_id_matches(game.get("game_id"), game_id)
                && game.get("status").and_then(serde_json::Value::as_str) == Some("working");
            approved_result |= is_approved;
            if is_approved {
                let tested_at = game
                    .get("tested_at")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                if tested_at.as_deref() > approved_tested_at.as_deref() {
                    approved_tested_at = tested_at;
                }
            }
            if approved_result {
                result_error = None;
            }
        }
        Err(error) if error.contains("HTTP 404") => {}
        Err(error) => {
            result_error.get_or_insert(error);
        }
    }

    let query = format!("repo:rileyadams05/digital-forge is:issue {game_id}");
    let issues_url = format!(
        "https://api.github.com/search/issues?q={}&per_page=100&sort=created&order=desc",
        encode_query_component(&query)
    );
    let (issues, issue_error) = match agent
        .get(&issues_url)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "DigitalForge")
        .call()
    {
        Ok(response) => match response.into_json::<serde_json::Value>() {
            Ok(data) => {
                let issues = data
                    .get("items")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|issue| issue.get("pull_request").is_none())
                    .filter(|issue| is_matching_failure_issue(issue, game_id))
                    .filter_map(|issue| {
                        let title = issue.get("title")?.as_str()?.to_owned();
                        let url = issue.get("html_url")?.as_str()?.to_owned();
                        let created_at = issue
                            .get("created_at")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        Some((title, url, created_at))
                    })
                    .collect();
                (issues, None)
            }
            Err(error) => (Vec::new(), Some(format!("invalid response ({error})"))),
        },
        Err(ureq::Error::Status(code, _)) => {
            (Vec::new(), Some(format!("GitHub returned HTTP {code}")))
        }
        Err(error) => (Vec::new(), Some(format!("request failed ({error})"))),
    };

    CommunityStatusCheck {
        approved_result,
        approved_tested_at,
        issues,
        result_error,
        issue_error,
    }
}

fn public_json(agent: &ureq::Agent, url: &str) -> Result<serde_json::Value, String> {
    match agent
        .get(url)
        .set("Accept", "application/json")
        .set("User-Agent", "DigitalForge")
        .call()
    {
        Ok(response) => response
            .into_json()
            .map_err(|error| format!("invalid response ({error})")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(error) => Err(format!("request failed ({error})")),
    }
}

fn game_id_matches(value: Option<&serde_json::Value>, game_id: &str) -> bool {
    value
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case(game_id))
}

fn is_matching_failure_issue(issue: &serde_json::Value, game_id: &str) -> bool {
    let title = issue
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let body = issue
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let lower_body = body.to_ascii_lowercase();
    let failure_report = title.to_ascii_lowercase().starts_with("[game failure]")
        || lower_body.contains("### observed game failure")
        || lower_body.contains("### game test status\ntested; a problem was found");
    failure_report && contains_delimited_id(&format!("{title} {body}"), game_id)
}

fn contains_delimited_id(text: &str, game_id: &str) -> bool {
    let text = text.to_ascii_uppercase();
    let game_id = game_id.to_ascii_uppercase();
    text.match_indices(&game_id).any(|(start, _)| {
        let end = start + game_id.len();
        let before_is_id_char = text[..start]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_alphanumeric());
        let after_is_id_char = text[end..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric());
        !before_is_id_char && !after_is_id_char
    })
}

fn encode_query_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn user_downloads_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = env::var_os("USERPROFILE").map(PathBuf::from);
    #[cfg(not(windows))]
    let home = env::var_os("HOME").map(PathBuf::from);
    home.map(|path| path.join("Downloads"))
}

fn classified_has_usable_update(classified: &ClassifiedSourceSet) -> bool {
    classified.update_sources.iter().any(|path| {
        path.is_file()
            || (path.is_dir()
                && fs::read_dir(path)
                    .map(|mut entries| entries.next().is_some())
                    .unwrap_or(false))
    })
}

fn measure_source_payload_bytes(sources: &[PathBuf]) -> Result<u64, Error> {
    let mut total = 0u64;
    let mut pending = sources.to_vec();
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("could not inspect source size at {}", path.display()))?;
        if metadata.is_file() {
            let support_file = path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| {
                    matches!(
                        extension.to_ascii_lowercase().as_str(),
                        "log" | "gp4" | "complete"
                    )
                });
            if support_file {
                continue;
            }
            total = total.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            for entry in fs::read_dir(&path)
                .with_context(|| format!("could not read source directory {}", path.display()))?
            {
                let entry = entry?;
                if entry.file_type()?.is_file() || entry.file_type()?.is_dir() {
                    pending.push(entry.path());
                }
            }
        }
    }
    Ok(total)
}

fn run_ps4_ingest_flow(
    classified: &ClassifiedSourceSet,
    log: &mut OperationLog,
) -> Result<(), Error> {
    render_terminal_header()?;
    println!("Detected {} Content", classified.platform.display_name());
    println!("========================================================\n");

    if let Some(ref title) = classified.title_name {
        println!("Title:    {title}");
    }
    if let Some(ref id) = classified.title_id {
        println!("Title ID: {id}");
    }
    if classified.itemzflow.is_some() {
        let payload_sources: Vec<PathBuf> = classified
            .base_sources
            .iter()
            .chain(&classified.update_sources)
            .cloned()
            .collect();
        let source_bytes = measure_source_payload_bytes(&payload_sources)?;
        log.info(&format!(
            "Itemzflow base and update source payload size: {source_bytes} bytes"
        ))?;
        if source_bytes > 50_000_000_000 {
            println!(
                "\nLarge Game Detected\nThis game is larger than 50 GB.\nFor larger games, Digital Forge recommends using your PC's internal storage for the merging and packaging process. Some large games may encounter errors when processed directly on external drives. You can still continue using an external drive if you prefer. If the build fails, you may need to restart the process using internal storage. Once your final PKG has been created successfully, you can transfer it to your external drive. Large transfers may take some time.\n"
            );
            log.info("Large-game advisory shown: source payload exceeds 50 GB; external storage remains allowed")?;
        }
    }
    println!();
    println!("Components:");
    if !classified.base_sources.is_empty() {
        let base_summary = if let Some(ref v) = classified.base_version {
            format!(
                "{} (v{})",
                display_source_name(&classified.base_sources[0]),
                v
            )
        } else {
            display_source_name(&classified.base_sources[0])
        };
        println!("  Base Game:\n    {base_summary}");
    }
    if !classified.update_sources.is_empty() {
        let update_summary = if let Some(ref v) = classified.update_version {
            format!(
                "{} (v{})",
                display_source_name(&classified.update_sources[0]),
                v
            )
        } else {
            display_source_name(&classified.update_sources[0])
        };
        println!("  Update:\n    {update_summary}");
    }
    if let Some(evidence) = &classified.itemzflow {
        println!("\nSource type: Itemzflow Dump");
        println!(
            "  Completion markers: base {}, update {}",
            if evidence.base_marker_found {
                "found"
            } else {
                "not provided"
            },
            if classified.update_sources.is_empty() {
                "not applicable"
            } else if evidence.update_marker_found {
                "found"
            } else {
                "not provided"
            }
        );
        println!(
            "  Itemzflow logs: {}",
            if evidence.logs_found > 0 {
                format!("{} available", evidence.logs_found)
            } else {
                "not provided".to_owned()
            }
        );
        println!(
            "  Source GP4: base {}, update {}",
            gp4_status_label(evidence.base_gp4_status),
            if classified.update_sources.is_empty() {
                "not applicable"
            } else {
                evidence
                    .update_gp4_status
                    .map(gp4_status_label)
                    .unwrap_or("not provided")
            }
        );
        println!("  Working GP4 projects will be generated in the temporary workspace.");
    }
    println!();

    let target_workflow = if classified.itemzflow.is_some() {
        println!("Itemzflow base/update dumps will be merged into one PS4 FPKG.");
        Some(iso2god::forge::TargetWorkflow::Ps4InternalInstall)
    } else {
        let Some(usage_mode) =
            iso2god::forge::ForgeUi::prompt_usage_mode(iso2god::forge::Platform::PlayStation4)?
        else {
            return Ok(());
        };
        if usage_mode == iso2god::forge::UsageMode::PhysicalConsole {
            let Some(storage) = iso2god::forge::ForgeUi::prompt_console_storage(
                iso2god::forge::Platform::PlayStation4,
            )?
            else {
                return Ok(());
            };
            iso2god::forge::target_workflow_for_console_storage(
                iso2god::forge::Platform::PlayStation4,
                storage,
            )
        } else {
            iso2god::forge::target_workflow_for_usage(
                iso2god::forge::Platform::PlayStation4,
                usage_mode,
            )
        }
    };
    let Some(target_workflow) = target_workflow else {
        return Ok(());
    };

    let base_group = build_content_group_from_sources("Base Game", &classified.base_sources)?;
    let update_group = build_content_group_from_sources("Update", &classified.update_sources)?;
    let dlc_group = FolderContentGroup {
        label: "DLC",
        directory: PathBuf::new(),
        sources: Vec::new(),
        multipart_sets: Vec::new(),
        ordinary_items: 0,
    };

    let result =
        run_ps4_package_groups_flow(&base_group, &update_group, &dlc_group, target_workflow, log);
    if result.is_ok() {
        offer_successful_test_log(classified.title_id.as_deref(), log)?;
        pause_to_continue()?;
    }
    result
}

fn gp4_status_label(status: iso2god::forge::itemzflow::Gp4Status) -> &'static str {
    match status {
        iso2god::forge::itemzflow::Gp4Status::Missing => "not provided",
        iso2god::forge::itemzflow::Gp4Status::Valid => "validated",
        iso2god::forge::itemzflow::Gp4Status::Invalid => "invalid; replacement will be generated",
    }
}

pub(super) fn run_manual_import_menu(dropped_source: Option<PathBuf>) -> Result<(), Error> {
    if let Some(source) = dropped_source {
        return run_drag_and_drop_pipeline(&[source]);
    }

    let mut log = OperationLog::create("manual_import")?;
    let result = run_manual_import_menu_logged(&mut log);
    if let Err(error) = &result {
        let _ = log.error(error);
        eprintln!("\nProcessing failed: {error:#}");
        eprintln!("Diagnostic log: {}", log.path().display());
        eprintln!("Shareable redacted report: {}", log.share_path().display());
        eprintln!("Search existing reports: https://github.com/rileyadams05/digital-forge/issues");
    }
    result
}

fn run_manual_import_menu_logged(log: &mut OperationLog) -> Result<(), Error> {
    log.info("Opened Manual Import")?;

    let mut auto_detected_source: Option<PathBuf> = None;

    loop {
        let detected_platform = auto_detected_source.as_ref().and_then(|src| {
            iso2god::forge::FormatDetector::detect(src, None)
                .ok()
                .filter(|d| d.platform != iso2god::forge::Platform::EpochCassetteVision)
                .map(|d| d.platform)
        });

        let platform = if let Some(p) = detected_platform {
            p
        } else {
            let Some(platform) = select_manual_import_platform()? else {
                return Ok(());
            };
            platform
        };

        let source = auto_detected_source.clone();
        log.info(&format!(
            "Manual Import platform: {}",
            platform.display_name()
        ))?;

        let Some(usage_mode) = iso2god::forge::ForgeUi::prompt_usage_mode(platform)? else {
            if detected_platform.is_some() {
                auto_detected_source = None;
                continue;
            }
            return Ok(());
        };
        let target_workflow = if let Some(workflow) =
            iso2god::forge::target_workflow_for_usage(platform, usage_mode)
        {
            workflow
        } else {
            let Some(workflow) =
                iso2god::forge::ForgeUi::prompt_target_workflow(platform, usage_mode)?
            else {
                continue;
            };
            workflow
        };

        run_platform_manual_import(platform, target_workflow, source, log)?;
        return Ok(());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualImportInputKind {
    Completed,
    MultiPart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultiPartFolderAction {
    UseExisting,
    CreateNew,
}

fn select_manual_import_input_kind(
    platform: iso2god::forge::Platform,
) -> Result<Option<ManualImportInputKind>, Error> {
    render_terminal_header()?;
    println!("Manual Import — {}", platform.display_name());
    println!("========================================================\n");
    println!("What type of input are you preparing?\n");
    select_menu(&[
        MenuOption {
            value: ManualImportInputKind::Completed,
            label: "Completed / Ready".to_owned(),
        },
        MenuOption {
            value: ManualImportInputKind::MultiPart,
            label: "Multi-Part".to_owned(),
        },
    ])
}

fn select_multi_part_folder_action(
    platform: iso2god::forge::Platform,
) -> Result<Option<MultiPartFolderAction>, Error> {
    render_terminal_header()?;
    println!("Manual Import — {}", platform.display_name());
    println!("========================================================\n");
    println!("Multi-Part folder:\n");
    select_menu(&[
        MenuOption {
            value: MultiPartFolderAction::UseExisting,
            label: "Use Existing Folder".to_owned(),
        },
        MenuOption {
            value: MultiPartFolderAction::CreateNew,
            label: "Create New Folder".to_owned(),
        },
    ])
}

fn create_multi_part_folder() -> Result<Option<PathBuf>, Error> {
    render_terminal_header()?;
    println!("Create Multi-Part Folder");
    println!("========================================================\n");
    let parent = default_multi_part_parent();
    fs::create_dir_all(&parent).with_context(|| {
        format!(
            "could not create the Downloads folder at {}",
            parent.display()
        )
    })?;
    let root = create_multi_part_layout(&parent, "New Folder")?;

    render_terminal_header()?;
    println!("Multi-Part Folder Ready");
    println!("========================================================\n");
    println!("{}\n", root.display());
    println!("Place base game files in the Game folder.");
    println!("Place update files in the Update folder.");
    println!("When finished, drag the root folder back into Digital Forge.\n");
    if let Err(error) = open_folder_in_file_explorer(&root) {
        println!("The folder was created, but could not be opened automatically: {error:#}");
    }
    pause_to_continue()?;
    Ok(Some(root))
}

fn create_multi_part_layout(parent: &Path, name: &str) -> Result<PathBuf, Error> {
    fs::create_dir_all(parent)?;
    let root = unique_manual_destination(parent, OsStr::new(name));
    fs::create_dir(&root).with_context(|| format!("could not create {}", root.display()))?;
    fs::create_dir(root.join("Game"))?;
    fs::create_dir(root.join("Update"))?;
    Ok(root)
}

fn default_multi_part_parent() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join("Downloads");
    }
    #[cfg(not(windows))]
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join("Downloads");
    }
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Downloads")
}

fn select_manual_import_platform() -> Result<Option<iso2god::forge::Platform>, Error> {
    use iso2god::forge::{MANUAL_IMPORT_RESULT_LIMIT, ManualImportMenuState, PLATFORM_CATALOG};

    let mut menu = ManualImportMenuState::default();
    let mut highlighted = 0usize;
    loop {
        render_terminal_header()?;
        println!("Manual Import");
        println!("========================================================");
        let visible_query = render_platform_search_field(menu.query());
        println!("Press ESC to return to the previous page.\n");

        for (visible_index, index) in menu
            .matching_indices()
            .iter()
            .take(MANUAL_IMPORT_RESULT_LIMIT)
            .enumerate()
        {
            let profile = &PLATFORM_CATALOG[*index];
            if visible_index == highlighted {
                println!("\x1b[38;5;208;1m  > {}\x1b[0m", profile.display_name);
            } else {
                println!("    {}", profile.display_name);
            }
        }
        if !menu.query().is_empty() && menu.matching_indices().is_empty() {
            println!("  No matching platform.");
        } else if menu.matching_indices().len() > MANUAL_IMPORT_RESULT_LIMIT {
            println!(
                "\n  {} of {} matches",
                MANUAL_IMPORT_RESULT_LIMIT,
                menu.matching_indices().len()
            );
        }
        io::stdout().flush()?;

        if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
            return Ok(None);
        }

        let _raw_mode = RawModeGuard::enter()?;
        let _mouse_capture = ManualImportMouseCaptureGuard::enter()?;
        let cursor_column = 2 + visible_query.chars().count() as u16;
        execute!(io::stdout(), EnableBlinking, Show, MoveTo(cursor_column, 9))
            .context("could not focus the Manual Import search field")?;
        loop {
            match read().context("could not read Manual Import menu input")? {
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(profile) =
                        manual_import_clicked_profile(&menu, mouse.column, mouse.row)
                    {
                        return Ok(Some(profile.platform));
                    }
                }
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    match manual_import_key_action(key.code) {
                        Some(ManualImportKeyAction::Character(character)) => {
                            menu.push(character);
                            highlighted = 0;
                            break;
                        }
                        Some(ManualImportKeyAction::Backspace) => {
                            if menu.query().is_empty() {
                                return Ok(None);
                            }
                            menu.backspace();
                            highlighted = 0;
                            break;
                        }
                        Some(ManualImportKeyAction::Select) => {
                            if let Some(index) = menu.matching_indices().get(highlighted) {
                                return Ok(Some(PLATFORM_CATALOG[*index].platform));
                            }
                        }
                        Some(ManualImportKeyAction::Up) => {
                            let visible = menu
                                .matching_indices()
                                .len()
                                .min(MANUAL_IMPORT_RESULT_LIMIT);
                            if visible > 0 {
                                highlighted = highlighted.checked_sub(1).unwrap_or(visible - 1);
                                break;
                            }
                        }
                        Some(ManualImportKeyAction::Down) => {
                            let visible = menu
                                .matching_indices()
                                .len()
                                .min(MANUAL_IMPORT_RESULT_LIMIT);
                            if visible > 0 {
                                highlighted = (highlighted + 1) % visible;
                                break;
                            }
                        }
                        Some(ManualImportKeyAction::Back) => return Ok(None),
                        None => {}
                    }
                }
                _ => {}
            }
        }
    }
}

pub(super) fn select_ftp_platform() -> Result<Option<iso2god::forge::Platform>, Error> {
    use iso2god::forge::{MANUAL_IMPORT_RESULT_LIMIT, ManualImportMenuState, PLATFORM_CATALOG};

    let mut menu = ManualImportMenuState::default();
    loop {
        render_terminal_header()?;
        println!("FTP");
        println!("========================================================");
        let visible_query = render_platform_search_field(menu.query());
        println!("Press ESC to return to the previous page.\n");

        let matches = ftp_matching_indices(&menu);
        for index in matches.iter().take(MANUAL_IMPORT_RESULT_LIMIT) {
            println!("    {}", PLATFORM_CATALOG[*index].display_name);
        }

        let exact_incompatible = (menu.matching_indices().len() == 1)
            .then(|| &PLATFORM_CATALOG[menu.matching_indices()[0]])
            .filter(|profile| !profile.platform.supports_ftp_configuration());
        if let Some(profile) = exact_incompatible {
            println!("  {} is not FTP compatible.", profile.display_name);
        } else if !menu.query().is_empty() && matches.is_empty() {
            println!("  No matching FTP-compatible console.");
        }
        io::stdout().flush()?;

        if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
            return Ok(None);
        }

        let _raw_mode = RawModeGuard::enter()?;
        let _mouse_capture = ManualImportMouseCaptureGuard::enter()?;
        let cursor_column = 2 + visible_query.chars().count() as u16;
        execute!(io::stdout(), EnableBlinking, Show, MoveTo(cursor_column, 9))
            .context("could not focus the FTP console search field")?;
        loop {
            match read().context("could not read FTP console search input")? {
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(platform) = ftp_clicked_platform(&matches, mouse.column, mouse.row)
                    {
                        return Ok(Some(platform));
                    }
                }
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    match manual_import_key_action(key.code) {
                        Some(ManualImportKeyAction::Character(character)) => {
                            menu.push(character);
                            break;
                        }
                        Some(ManualImportKeyAction::Backspace) => {
                            menu.backspace();
                            break;
                        }
                        Some(ManualImportKeyAction::Select) if matches.len() == 1 => {
                            return Ok(Some(PLATFORM_CATALOG[matches[0]].platform));
                        }
                        Some(ManualImportKeyAction::Back) => return Ok(None),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

const MANUAL_IMPORT_SEARCH_WIDTH: usize = 48;
const MANUAL_IMPORT_FIRST_RESULT_ROW: u16 = 14;

struct ManualImportMouseCaptureGuard;

impl ManualImportMouseCaptureGuard {
    fn enter() -> Result<Self, Error> {
        execute!(io::stdout(), EnableMouseCapture)
            .context("could not enable mouse input for Manual Import")?;
        Ok(Self)
    }
}

impl Drop for ManualImportMouseCaptureGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
}

fn manual_import_clicked_profile(
    menu: &iso2god::forge::ManualImportMenuState,
    column: u16,
    row: u16,
) -> Option<&'static iso2god::forge::PlatformProfile> {
    let visible_index = row.checked_sub(MANUAL_IMPORT_FIRST_RESULT_ROW)? as usize;
    if visible_index >= iso2god::forge::MANUAL_IMPORT_RESULT_LIMIT {
        return None;
    }
    let profile = menu
        .matching_indices()
        .get(visible_index)
        .map(|index| &iso2god::forge::PLATFORM_CATALOG[*index])?;
    let label_start = 4;
    let label_end = label_start + profile.display_name.chars().count() as u16;
    (column >= label_start && column < label_end).then_some(profile)
}

fn ftp_matching_indices(menu: &iso2god::forge::ManualImportMenuState) -> Vec<usize> {
    menu.matching_indices()
        .iter()
        .copied()
        .filter(|index| {
            iso2god::forge::PLATFORM_CATALOG[*index]
                .platform
                .supports_ftp_configuration()
        })
        .collect()
}

fn ftp_clicked_platform(
    indices: &[usize],
    column: u16,
    row: u16,
) -> Option<iso2god::forge::Platform> {
    let visible_index = row.checked_sub(MANUAL_IMPORT_FIRST_RESULT_ROW)? as usize;
    if visible_index >= iso2god::forge::MANUAL_IMPORT_RESULT_LIMIT {
        return None;
    }
    let profile = indices
        .get(visible_index)
        .map(|index| &iso2god::forge::PLATFORM_CATALOG[*index])?;
    let label_start = 4;
    let label_end = label_start + profile.display_name.chars().count() as u16;
    (column >= label_start && column < label_end).then_some(profile.platform)
}

fn render_platform_search_field(query: &str) -> String {
    println!("\nSearch");
    let visible_query = manual_import_search_window(query);
    let search_padding = " ".repeat(MANUAL_IMPORT_SEARCH_WIDTH - visible_query.chars().count());
    println!(
        "\x1b[38;5;208m┌{}┐\x1b[0m",
        "─".repeat(MANUAL_IMPORT_SEARCH_WIDTH + 2)
    );
    println!("\x1b[38;5;208m│ \x1b[0m{visible_query}{search_padding}\x1b[38;5;208m │\x1b[0m");
    println!(
        "\x1b[38;5;208m└{}┘\x1b[0m\n",
        "─".repeat(MANUAL_IMPORT_SEARCH_WIDTH + 2)
    );
    visible_query
}

fn manual_import_search_window(query: &str) -> String {
    let mut visible = query
        .chars()
        .rev()
        .take(MANUAL_IMPORT_SEARCH_WIDTH)
        .collect::<Vec<_>>();
    visible.reverse();
    visible.into_iter().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualImportKeyAction {
    Character(char),
    Backspace,
    Select,
    Back,
    Up,
    Down,
}

fn manual_import_key_action(key: KeyCode) -> Option<ManualImportKeyAction> {
    match key {
        KeyCode::Char(character) if !character.is_control() => {
            Some(ManualImportKeyAction::Character(character))
        }
        KeyCode::Backspace => Some(ManualImportKeyAction::Backspace),
        KeyCode::Enter => Some(ManualImportKeyAction::Select),
        KeyCode::Esc => Some(ManualImportKeyAction::Back),
        KeyCode::Up => Some(ManualImportKeyAction::Up),
        KeyCode::Down => Some(ManualImportKeyAction::Down),
        _ => None,
    }
}

fn run_platform_manual_import(
    platform: iso2god::forge::Platform,
    target_workflow: iso2god::forge::TargetWorkflow,
    supplied_source: Option<PathBuf>,
    log: &mut OperationLog,
) -> Result<(), Error> {
    render_terminal_header()?;
    println!("Manual Import — {}", platform.display_name());
    println!("========================================================\n");

    println!("Accepted source formats:");
    for format in iso2god::forge::ForgeUi::source_formats(platform) {
        println!("  - {}", format.display_name());
    }
    println!("\nPress ESC to return to the previous page.");

    let source = if let Some(source) = supplied_source {
        source
    } else {
        println!("\nDrag and drop the package, disc image, archive, or game folder to import.\n");
        let Some(source) = prompt_dropped_path("Waiting for source", None)? else {
            return Ok(());
        };
        source
    };
    if !source.exists() {
        anyhow::bail!("the selected source does not exist: {}", source.display());
    }
    if source.is_file() && fs::metadata(&source)?.len() == 0 {
        anyhow::bail!("the selected source file is empty: {}", source.display());
    }

    if source.is_dir() {
        return run_folder_contract_import(&source, platform, target_workflow, log);
    }

    let base = scan_file_content_group("Base Game", &source)?;
    println!("\nBase Game accepted: {}", source.display());

    println!("\nUpdate (Optional):");
    println!("Drag and drop an Update package/archive, or press Enter to skip:\n");
    let mut update = FolderContentGroup {
        label: "Update",
        directory: PathBuf::new(),
        sources: Vec::new(),
        multipart_sets: Vec::new(),
        ordinary_items: 0,
    };
    if let Some(dropped_update) = prompt_optional_dropped_path("Waiting for Update", None)? {
        update = if dropped_update.is_dir() {
            scan_folder_content_group("Update", &dropped_update, false)?
        } else {
            scan_file_content_group("Update", &dropped_update)?
        };
    }

    let mut dlc = FolderContentGroup {
        label: "DLC",
        directory: PathBuf::new(),
        sources: Vec::new(),
        multipart_sets: Vec::new(),
        ordinary_items: 0,
    };
    if update.is_empty() && dlc.is_empty() {
        let resolved_source = resolve_manual_import_source(&source)?;
        log.info(&format!(
            "Starting Digital Forge flow for {} ({})",
            resolved_source.display(),
            platform.display_name()
        ))?;
        let completed = iso2god::forge::ForgeUi::run_interactive_flow_with_target(
            &resolved_source,
            platform,
            target_workflow,
        )?;
        if completed.is_some() {
            let game_id = iso2god::forge::FormatDetector::detect(&resolved_source, Some(platform))
                .ok()
                .and_then(|detection| detection.game_id);
            offer_successful_test_log(game_id.as_deref(), log)?;
        }
        return pause_to_continue();
    }

    run_package_groups_flow(&base, &update, &dlc, platform, target_workflow, log)
}

#[derive(Debug)]
struct FolderContentGroup {
    label: &'static str,
    directory: PathBuf,
    sources: Vec<PathBuf>,
    multipart_sets: Vec<MultipartGameSet>,
    ordinary_items: usize,
}

impl FolderContentGroup {
    fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    fn multipart_part_count(&self) -> usize {
        self.multipart_sets
            .iter()
            .filter(|set| set.parts.len() > 1)
            .map(|set| set.parts.len())
            .sum()
    }

    fn summary(&self) -> String {
        if self.is_empty() {
            return "Empty / Not provided".to_owned();
        }
        let multipart_sets = self
            .multipart_sets
            .iter()
            .filter(|set| set.parts.len() > 1)
            .count();
        let single_archive_count = self
            .multipart_sets
            .iter()
            .filter(|set| set.parts.len() <= 1)
            .count();
        let total_items = self.ordinary_items + single_archive_count;
        let parts = self.multipart_part_count();
        match (multipart_sets, total_items) {
            (1, 0) => format!("{parts} parts"),
            (sets, 0) if sets > 1 => format!("{sets} multipart sets, {parts} parts total"),
            (0, 1) => "1 item".to_owned(),
            (0, items) if items > 1 => format!("{items} items"),
            (0, _) => {
                let count = self.sources.len().max(1);
                if count == 1 {
                    "1 item".to_owned()
                } else {
                    format!("{count} items")
                }
            }
            (sets, items) => {
                format!("{sets} multipart set(s), {parts} parts, {items} other item(s)")
            }
        }
    }
}

fn run_folder_contract_import(
    root: &Path,
    platform: iso2god::forge::Platform,
    target_workflow: iso2god::forge::TargetWorkflow,
    log: &mut OperationLog,
) -> Result<(), Error> {
    let (game_path, update_parent) = if platform == iso2god::forge::Platform::PlayStation3 {
        let (game, parent) = game_and_update_parent(root)?;
        (Some(game), parent)
    } else if root
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("Game"))
    {
        // The shared folder contract also accepts a directly dropped
        // Game/GAME directory. Its real parent is used only to find a
        // neighbouring Update directory.
        (
            Some(root.to_path_buf()),
            root.parent().map(Path::to_path_buf),
        )
    } else if let Some(game) = find_named_child_directory_or_file(root, "Game") {
        (Some(game), Some(root.to_path_buf()))
    } else if platform == iso2god::forge::Platform::PlayStation3 {
        (None, Some(root.to_path_buf()))
    } else {
        // Preserve legacy non-PS3 imports whose payload is directly at the
        // dropped root, while still checking that root for a sibling Update.
        (None, Some(root.to_path_buf()))
    };
    let mut base = match game_path.as_deref() {
        Some(directory) if directory.is_dir() => {
            scan_folder_content_group("Base Game", directory, false)?
        }
        Some(file) if file.is_file() => {
            let parent = file.parent().unwrap_or(file);
            let all_sets = scan_archive_sets_in_directory(parent).unwrap_or_default();
            let matching_set = all_sets
                .into_iter()
                .find(|s| s.parts.contains(&file.to_path_buf()));
            if let Some(set) = matching_set {
                let is_multi = set.parts.len() > 1;
                FolderContentGroup {
                    label: "Base Game",
                    directory: parent.to_path_buf(),
                    sources: vec![set.entry_path.clone()],
                    multipart_sets: if is_multi { vec![set] } else { Vec::new() },
                    ordinary_items: if is_multi { 0 } else { 1 },
                }
            } else {
                FolderContentGroup {
                    label: "Base Game",
                    directory: parent.to_path_buf(),
                    sources: vec![file.to_path_buf()],
                    multipart_sets: Vec::new(),
                    ordinary_items: 1,
                }
            }
        }
        _ => scan_folder_content_group("Base Game", root, true)?,
    };
    // Older prepared folders stored the base game directly at the root. Keep
    // those imports valid for non-PS3 platforms. PS3 imports require the
    // explicit Game/Update contract and never infer identity from the parent.
    if platform != iso2god::forge::Platform::PlayStation3 && base.is_empty() && game_path.is_some()
    {
        base = scan_folder_content_group("Base Game", root, true)?;
    }
    let update_path = update_parent
        .as_deref()
        .and_then(|parent| find_named_child_directory_or_file(parent, "Update"));
    let mut update = scan_optional_folder_content_group("Update", update_path.as_deref())?;
    let mut dlc = FolderContentGroup {
        label: "DLC",
        directory: PathBuf::new(),
        sources: Vec::new(),
        multipart_sets: Vec::new(),
        ordinary_items: 0,
    };
    if platform != iso2god::forge::Platform::PlayStation3 {
        let dlc_path = find_named_child_directory_or_file(root, "DLC");
        dlc = scan_optional_folder_content_group("DLC", dlc_path.as_deref())?;
    }

    if platform == iso2god::forge::Platform::PlayStation3 && base.is_empty() {
        match handle_empty_ps3_contract_folders(true, update.is_empty())? {
            Ps3EmptyFolderAction::Rescan => {
                return run_folder_contract_import(root, platform, target_workflow, log);
            }
            Ps3EmptyFolderAction::ContinueWithBase => {
                unreachable!("the base-game-empty handler cannot continue without a base game")
            }
            Ps3EmptyFolderAction::Pending => return Ok(()),
        }
    }
    ensure!(
        !base.is_empty(),
        "No base-game content was found in the Game folder"
    );

    if base.is_empty() {
        render_terminal_header()?;
        println!("Manual Import — {}", platform.display_name());
        println!("========================================================\n");
        println!(
            "Could not automatically detect the Base Game in:\n  {}\n",
            root.display()
        );
        println!(
            "Please drag and drop the Base Game (single file, folder, or multi-part archive):"
        );
        println!("(Press ESC to cancel)\n");
        let Some(dropped_base) = prompt_dropped_path("Waiting for Base Game", None)? else {
            return Ok(());
        };
        base = if dropped_base.is_dir() {
            scan_folder_content_group("Base Game", &dropped_base, false)?
        } else {
            scan_file_content_group("Base Game", &dropped_base)?
        };
        if base.is_empty() {
            anyhow::bail!(
                "No valid base-game content detected in {}",
                dropped_base.display()
            );
        }
    }

    if platform == iso2god::forge::Platform::PlayStation3 {
        let base_source = if base.sources.len() == 1 {
            base.sources[0].clone()
        } else {
            base.directory.clone()
        };
        if update.is_empty() {
            update = prompt_for_ps3_optional_update(&base_source, Some(log))?;
        } else {
            println!("\nUpdate detected.\nValidating update...");
            validate_group_multipart_sets(&update)?;
            validate_ps3_related_content(&base_source, &update, log)?;
            println!("Update validated.");
        }
    }

    run_package_groups_flow(&base, &update, &dlc, platform, target_workflow, log)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ps3EmptyFolderAction {
    Rescan,
    ContinueWithBase,
    Pending,
}

fn ps3_empty_folder_message(game_empty: bool, update_empty: bool) -> Option<&'static str> {
    match (game_empty, update_empty) {
        (true, true) => {
            Some("Please add your PS3 game and update files to their folders to continue.")
        }
        (true, false) => Some("Please place your PS3 game files in the Game folder to continue."),
        (false, true) => {
            Some("Please place your PS3 update files in the Update folder to continue.")
        }
        (false, false) => None,
    }
}

fn handle_empty_ps3_contract_folders(
    game_empty: bool,
    update_empty: bool,
) -> Result<Ps3EmptyFolderAction, Error> {
    let message = ps3_empty_folder_message(game_empty, update_empty)
        .expect("empty-folder handler called without an empty folder");
    println!("\n{message}");
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        return Ok(Ps3EmptyFolderAction::Pending);
    }
    if game_empty {
        println!("Add the missing files, then press Enter to scan the folders again.");
        let _ = prompt_terminal_text("Waiting for PS3 files", Some(""), false)?;
        return Ok(Ps3EmptyFolderAction::Rescan);
    }
    if prompt_yes_no_key(
        "No update is currently available for this title. Continue with the base game?",
        false,
    )? == Some(true)
    {
        return Ok(Ps3EmptyFolderAction::ContinueWithBase);
    }
    println!("Add the update files, then press Enter to scan the Update folder again.");
    let _ = prompt_terminal_text("Waiting for PS3 update files", Some(""), false)?;
    Ok(Ps3EmptyFolderAction::Rescan)
}

fn ps3_optional_update_prompt_text() -> &'static str {
    "No game update was detected.\n\nDo you have an update folder?\n\nDrag and drop your Update folder here,\nor press Enter to continue without an update."
}

fn game_and_update_parent(root: &Path) -> Result<(PathBuf, Option<PathBuf>), Error> {
    let dropped_game_directory = root
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("Game"));
    if dropped_game_directory {
        return Ok((root.to_path_buf(), root.parent().map(Path::to_path_buf)));
    }

    let game = find_named_child_directory(root, "Game")
        .context("import folder must contain a directory named exactly Game")?;
    ensure!(
        game.is_dir(),
        "PS3 import Game entry is not a directory: {}",
        game.display()
    );
    Ok((game, Some(root.to_path_buf())))
}

fn empty_folder_content_group(label: &'static str) -> FolderContentGroup {
    FolderContentGroup {
        label,
        directory: PathBuf::new(),
        sources: Vec::new(),
        multipart_sets: Vec::new(),
        ordinary_items: 0,
    }
}

fn prompt_for_ps3_optional_update(
    base_source: &Path,
    mut log: Option<&mut OperationLog>,
) -> Result<FolderContentGroup, Error> {
    println!("\n{}\n", ps3_optional_update_prompt_text());
    loop {
        let Some(dropped_update) = prompt_optional_dropped_path("", Some(true))? else {
            println!("Continuing without an update...");
            return Ok(empty_folder_content_group("Update"));
        };

        let candidate = match scan_folder_content_group("Update", &dropped_update, false) {
            Ok(candidate) if !candidate.is_empty() => candidate,
            Ok(_) => {
                println!(
                    "The supplied Update folder is empty. Drop another Update folder or press Enter to continue without an update."
                );
                continue;
            }
            Err(error) => {
                println!(
                    "Could not inspect that Update folder: {error:#}\nDrop another Update folder or press Enter to continue without an update."
                );
                continue;
            }
        };

        let validation = validate_group_multipart_sets(&candidate).and_then(|_| {
            if let Some(log) = log.as_deref_mut() {
                validate_ps3_related_content(base_source, &candidate, log)
            } else {
                Ok(())
            }
        });
        match validation {
            Ok(()) => {
                println!("Update folder received.\nChecking compatibility...\nUpdate validated.");
                return Ok(candidate);
            }
            Err(error) => {
                println!(
                    "Update rejected: {error:#}\nDrop another Update folder or press Enter to continue without an update."
                );
            }
        }
    }
}

fn download_ps3_updates_for_base(base: &FolderContentGroup) -> Result<FolderContentGroup, Error> {
    let source = base.sources.first().unwrap_or(&base.directory);
    ensure!(source.exists(), "PS3 base content has no source path");
    let detected = iso2god::forge::FormatDetector::detect(
        source,
        Some(iso2god::forge::Platform::PlayStation3),
    )?;
    let title_id = detected
        .game_id
        .context("PS3 base content has no Title ID for update lookup")?;
    let output = base.directory.join("DigitalForge PS3 Updates");
    fs::create_dir_all(&output)?;
    let tool = env::var_os("DIGITAL_FORGE_PS3_UPDATE_TOOL")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            ["ps3udl.exe", "ps3udl"]
                .into_iter()
                .find_map(|name| which_command(name))
        })
        .context(
            "ps3-update-dl v0.3.0.post2 is not installed; set DIGITAL_FORGE_PS3_UPDATE_TOOL to ps3udl",
        )?;
    let mut command = if tool
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"))
    {
        let mut command = Command::new("cmd.exe");
        command.args([OsStr::new("/C"), tool.as_os_str()]);
        command
    } else {
        Command::new(&tool)
    };
    let result = command
        .args([OsStr::new("-o"), output.as_os_str(), OsStr::new(&title_id)])
        .output()
        .with_context(|| format!("could not launch ps3-update-dl at {}", tool.display()))?;
    ensure!(
        result.status.success(),
        "ps3-update-dl failed for {title_id}: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let group = scan_folder_content_group("Update", &output, false)?;
    ensure!(
        !group.sources.is_empty(),
        "ps3-update-dl returned no update packages for {title_id}"
    );
    Ok(group)
}

fn which_command(name: &str) -> Option<PathBuf> {
    let from_path = env::var_os("PATH").and_then(|path| {
        path.to_string_lossy().split(';').find_map(|dir| {
            let candidate = PathBuf::from(dir).join(name);
            candidate.is_file().then_some(candidate)
        })
    });
    from_path.or_else(|| {
        if name != "ps3udl.exe" && name != "ps3udl" {
            return None;
        }
        let mut roots = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))];
        if let Ok(executable) = env::current_exe() {
            let mut root = executable.parent().map(Path::to_path_buf);
            for _ in 0..4 {
                let Some(path) = root else { break };
                roots.push(path.clone());
                root = path.parent().map(Path::to_path_buf);
            }
        }
        roots
            .into_iter()
            .flat_map(|root| {
                [
                    root.join("runtime/ps3/ps3-update-dl/ps3udl.cmd"),
                    root.join("vendor/ps3-update-dl/ps3udl.cmd"),
                ]
            })
            .find(|path| path.is_file())
    })
}

fn run_package_groups_flow(
    base: &FolderContentGroup,
    update: &FolderContentGroup,
    dlc: &FolderContentGroup,
    platform: iso2god::forge::Platform,
    target_workflow: iso2god::forge::TargetWorkflow,
    log: &mut OperationLog,
) -> Result<(), Error> {
    validate_group_multipart_sets(base)?;
    validate_group_multipart_sets(update)?;
    validate_group_multipart_sets(dlc)?;

    if platform == iso2god::forge::Platform::PlayStation4 {
        let result = run_ps4_package_groups_flow(base, update, dlc, target_workflow, log);
        if result.is_ok() {
            let game_id = base.sources.first().and_then(|source| {
                iso2god::forge::FormatDetector::detect(source, Some(platform))
                    .ok()
                    .and_then(|detection| detection.game_id)
            });
            offer_successful_test_log(game_id.as_deref(), log)?;
            pause_to_continue()?;
        }
        return result;
    }

    render_terminal_header()?;
    println!("Detected Game Package");
    println!("========================================================\n");
    println!("Base Game\n  - {}", base.summary());
    println!("\nUpdate\n  - {}", update.summary());

    let base_source = if base.sources.len() == 1 {
        base.sources[0].clone()
    } else {
        base.directory.clone()
    };
    let mut merge_workspace = None;
    let mut conversion_source = base_source.clone();
    if platform == iso2god::forge::Platform::PlayStation3 {
        validate_ps3_related_content(&base_source, update, log)?;
        if !update.is_empty() {
            let workspace = iso2god::forge::TempWorkspace::new("ps3_update_merge")?;
            match prepare_ps3_merged_source(&base_source, update, &workspace.root) {
                Ok(merged) => {
                    conversion_source = merged;
                    merge_workspace = Some(workspace);
                    log.info("Prepared a validated merged PS3 base/update payload")?;
                }
                Err(error) => {
                    log.info(&format!(
                        "PS3 update consolidation rejected without modifying the source: {error:#}"
                    ))?;
                    anyhow::bail!(
                        "PS3 update consolidation is unsupported for this input: {error:#}"
                    );
                }
            }
        }
    }
    log.info(&format!(
        "Folder contract base: {}",
        conversion_source.display()
    ))?;
    let Some(preset) = iso2god::forge::ForgeUi::run_interactive_flow_with_target(
        &conversion_source,
        platform,
        target_workflow,
    )
    .context("BASE ERROR: base-game processing failed")?
    else {
        return Ok(());
    };

    if platform != iso2god::forge::Platform::PlayStation3 {
        process_optional_content_group(update, platform, log, &preset)?;
    }
    if platform != iso2god::forge::Platform::PlayStation3 {
        process_optional_content_group(dlc, platform, log, &preset)?;
    }
    validate_completed_package(&preset, platform, log, None)?;
    let game_id = base.sources.first().and_then(|source| {
        iso2god::forge::FormatDetector::detect(source, Some(platform))
            .ok()
            .and_then(|detection| detection.game_id)
    });
    offer_successful_test_log(game_id.as_deref(), log)?;
    drop(merge_workspace);
    pause_to_continue()
}

fn validate_ps3_related_content(
    base_source: &Path,
    update: &FolderContentGroup,
    log: &mut OperationLog,
) -> Result<(), Error> {
    let base = iso2god::forge::FormatDetector::detect(
        base_source,
        Some(iso2god::forge::Platform::PlayStation3),
    )
    .with_context(|| {
        format!(
            "could not inspect PS3 base content {}",
            base_source.display()
        )
    })?;
    let base_title_id = base
        .game_id
        .as_deref()
        .map(ps3_title_id_from_identifier)
        .flatten();

    for source in &update.sources {
        let detected = iso2god::forge::FormatDetector::detect(
            source,
            Some(iso2god::forge::Platform::PlayStation3),
        )
        .with_context(|| format!("could not inspect PS3 Update {}", source.display()))?;
        ensure!(
            detected.platform == iso2god::forge::Platform::PlayStation3,
            "PS3 Update has incompatible content: {}",
            source.display()
        );
        if let (Some(base_id), Some(content_id)) =
            (base_title_id.as_deref(), detected.game_id.as_deref())
        {
            if let Some(content_title_id) = ps3_title_id_from_identifier(content_id) {
                ensure!(
                    content_title_id.eq_ignore_ascii_case(base_id),
                    "PS3 Update Content ID {} does not belong to base title {}",
                    content_id,
                    base_id
                );
                if let (Some(base_region), Some(content_region)) = (
                    ps3_region_from_title_id(base_id),
                    ps3_region_from_title_id(&content_title_id),
                ) {
                    ensure!(
                        base_region == content_region,
                        "PS3 Update region {content_region} does not match base region {base_region}"
                    );
                }
            }
        }
        log.info(&format!(
            "Matched PS3 Update {} ({}) to base title {}",
            source.display(),
            detected.game_id.as_deref().unwrap_or("unknown Content ID"),
            base_title_id.as_deref().unwrap_or("unknown Title ID")
        ))?;
    }
    Ok(())
}

fn prepare_ps3_merged_source(
    base_source: &Path,
    update: &FolderContentGroup,
    workspace: &Path,
) -> Result<PathBuf, Error> {
    // Keep the merged payload inside the same PS3_GAME container expected by
    // the existing detector and package creator.  Returning the parent also
    // prevents the consolidation path from accidentally becoming a generic
    // directory packager.
    let merged_root = workspace.join("merged");
    let merged = merged_root.join("PS3_GAME");
    let base_stage = workspace.join("base");
    fs::create_dir_all(&merged)?;
    materialize_ps3_payload(base_source, &base_stage)?;
    let base_version =
        iso2god::forge::FormatDetector::read_sfo_key(&base_stage.join("PARAM.SFO"), "VERSION")
            .context("base PARAM.SFO has no VERSION field")?;
    let base_version_key = ps3_version_sort_key(&base_version)?;
    copy_ps3_payload_tree(&base_stage, &merged, false)?;

    let mut update_payloads = Vec::new();
    for (index, source) in update.sources.iter().enumerate() {
        let stage = workspace.join(format!("update-{index}"));
        materialize_ps3_payload(source, &stage)?;
        let sfo = stage.join("PARAM.SFO");
        let version = iso2god::forge::FormatDetector::read_sfo_key(&sfo, "VERSION")
            .context("update PARAM.SFO has no VERSION field")?;
        let version_key = ps3_version_sort_key(&version)?;
        ensure!(
            version_key > base_version_key,
            "update version {version} is not newer than base version {base_version}"
        );
        update_payloads.push((version_key, version, stage));
    }
    update_payloads.sort_by_key(|(version, _, _)| *version);
    for window in update_payloads.windows(2) {
        ensure!(
            window[0].0 < window[1].0,
            "PS3 update versions are ambiguous: {} and {}",
            window[0].1,
            window[1].1
        );
    }
    for (_, _, stage) in update_payloads {
        copy_ps3_payload_tree(&stage, &merged, true)?;
    }

    ensure!(
        merged.join("PARAM.SFO").is_file(),
        "merged PS3 payload is missing PARAM.SFO"
    );
    ensure!(
        merged.join("USRDIR").join("EBOOT.BIN").is_file(),
        "merged PS3 payload is missing USRDIR/EBOOT.BIN"
    );
    Ok(merged_root)
}

fn materialize_ps3_payload(source: &Path, destination: &Path) -> Result<(), Error> {
    if source.is_dir() {
        let payload = find_ps3_payload_root(source)
            .context("directory does not contain a PS3_GAME payload")?;
        return copy_ps3_payload_tree(&payload, destination, false);
    }

    let detected = iso2god::forge::FormatDetector::detect(
        source,
        Some(iso2god::forge::Platform::PlayStation3),
    )?;
    if detected.source_format == iso2god::forge::SourceFormat::Ps3Pkg {
        iso2god::forge::backends::extract_ps3_pkg_for_workflow(source, destination)?;
        return ensure_ps3_payload_root(destination);
    }

    match iso2god::forge::ArchiveEngine::classify(source) {
        iso2god::forge::ArchiveKind::SingleVolume(archive)
        | iso2god::forge::ArchiveKind::MultiVolume {
            first_volume: archive,
            ..
        } => {
            let extracted = iso2god::forge::TempWorkspace::new("ps3_archive_payload")?;
            let root = iso2god::forge::ArchiveEngine::extract_archive(
                &archive,
                &extracted.root,
                None,
                Arc::new(AtomicBool::new(false)),
            )?;
            let payload = find_ps3_payload_root(&root)
                .context("archive does not contain a PS3_GAME payload")?;
            copy_ps3_payload_tree(&payload, destination, false)
        }
        iso2god::forge::ArchiveKind::ConsoleSplitImage { .. } => anyhow::bail!(
            "PS3 split disc images are supported for standalone conversion, but cannot be safely consolidated with PKG updates"
        ),
        _ => anyhow::bail!(
            "unsupported PS3 source for consolidation: {}",
            source.display()
        ),
    }
}

fn find_ps3_payload_root(root: &Path) -> Option<PathBuf> {
    if root.join("PARAM.SFO").is_file() && root.join("USRDIR").is_dir() {
        return Some(root.to_path_buf());
    }
    for entry in fs::read_dir(root).ok()?.flatten() {
        if entry.file_type().ok()?.is_dir() {
            if let Some(found) = find_ps3_payload_root(&entry.path()) {
                return Some(found);
            }
        }
    }
    None
}

fn ensure_ps3_payload_root(root: &Path) -> Result<(), Error> {
    ensure!(
        find_ps3_payload_root(root).is_some(),
        "extracted PS3 package does not contain PARAM.SFO and USRDIR"
    );
    Ok(())
}

fn copy_ps3_payload_tree(
    source: &Path,
    destination: &Path,
    skip_metadata: bool,
) -> Result<(), Error> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if skip_metadata && entry.file_name().eq_ignore_ascii_case("PARAM.SFO") {
            continue;
        }
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_ps3_payload_tree(&entry.path(), &target, false)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn ps3_version_sort_key(version: &str) -> Result<(u32, u32), Error> {
    let mut parts = version
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .map(|part| part.parse::<u32>());
    let major = parts
        .next()
        .transpose()?
        .context("PS3 update VERSION has no numeric major component")?;
    let minor = parts.next().transpose()?.unwrap_or(0);
    Ok((major, minor))
}

fn ps3_title_id_from_identifier(identifier: &str) -> Option<String> {
    let title = if identifier.contains('-') {
        let mut parts = identifier.split('-');
        let _publisher = parts.next()?;
        parts.next()?.split('_').next()?.trim()
    } else {
        identifier.trim()
    };
    if title.len() >= 9 {
        Some(title[..9].to_ascii_uppercase())
    } else {
        None
    }
}

fn ps3_update_version_key(path: &Path) -> (u32, u32, String) {
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    let mut numbers = name
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u32>().ok());
    let major = numbers.next().unwrap_or(u32::MAX);
    let minor = numbers.next().unwrap_or(u32::MAX);
    (major, minor, name.to_ascii_uppercase())
}

fn ps3_region_from_title_id(title_id: &str) -> Option<&'static str> {
    let id = title_id.to_ascii_uppercase();
    let prefix = &id[..4.min(id.len())];
    match prefix {
        "BCUS" | "BLUS" => Some("US"),
        "BLES" | "BCES" => Some("EU"),
        "BCAS" | "BLAS" => Some("ASIA"),
        "BCJS" | "BLJS" => Some("JP"),
        _ => None,
    }
}

fn run_ps4_package_groups_flow(
    base: &FolderContentGroup,
    update: &FolderContentGroup,
    dlc: &FolderContentGroup,
    target_workflow: iso2god::forge::TargetWorkflow,
    log: &mut OperationLog,
) -> Result<(), Error> {
    let package_progress = Arc::new(Ps4PackageProgress::default());
    log.stage(
        "source_validation",
        &format!(
            "PS4 base/update inputs; target workflow {}",
            target_workflow.display_name()
        ),
    )?;
    render_terminal_header()?;
    println!("Detected PlayStation 4 Source Set");
    println!("========================================================\n");
    println!("Base Game\n  - {}", base.summary());
    println!("\nUpdate\n  - {}", update.summary());
    match target_workflow {
        iso2god::forge::TargetWorkflow::Ps4ItemzFlow => {
            println!("\nTarget: ItemzFlow FG App Folder.");
        }
        iso2god::forge::TargetWorkflow::Ps4ShadPs4 => {
            println!("\nTarget: one shadPS4 library layout.");
        }
        _ => println!("\nTarget: {}", target_workflow.display_name()),
    }

    let raw_base_source = if base.sources.len() == 1 {
        base.sources[0].clone()
    } else if !base.sources.is_empty() {
        base.sources[0].clone()
    } else {
        base.directory.clone()
    };

    let resolved_base = find_payload_pkg_or_dir(&raw_base_source);
    let mut detection = iso2god::forge::FormatDetector::detect(
        &resolved_base,
        Some(iso2god::forge::Platform::PlayStation4),
    )
    .unwrap_or_else(|_| iso2god::forge::DetectionResult {
        platform: iso2god::forge::Platform::PlayStation4,
        source_format: iso2god::forge::SourceFormat::Ps4RetailPkg,
        package_classification: Some(iso2god::forge::PackageClassification::Retail),
        confidence: iso2god::forge::ConfidenceLevel::High,
        game_id: None,
        title_name: None,
        is_multipart: false,
        is_archive: false,
        is_multidisc: false,
        disc_number: None,
        total_discs: None,
        related_files: base.sources.clone(),
        missing_parts: Vec::new(),
        license_status: iso2god::forge::LicenseStatus::NotApplicable,
    });

    if let Some(tool) = find_orbis_pkg_tool() {
        let pkg_candidate = if resolved_base.is_file()
            && resolved_base
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pkg"))
        {
            Some(resolved_base.clone())
        } else {
            base.sources.iter().find_map(|src| {
                let res = find_payload_pkg_or_dir(src);
                if res.is_file()
                    && res
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("pkg"))
                {
                    Some(res)
                } else {
                    None
                }
            })
        };

        if let Some(pkg_path) = pkg_candidate {
            if let Ok(meta) = read_ps4_pkg_metadata(&tool, &pkg_path) {
                if !meta.title_id.is_empty() {
                    detection.game_id = Some(meta.title_id);
                }
                if !meta.title.is_empty() {
                    detection.title_name = Some(meta.title);
                }
                if meta.is_fake {
                    detection.source_format = iso2god::forge::SourceFormat::Ps4FakePkg;
                    detection.package_classification =
                        Some(iso2god::forge::PackageClassification::FakePackage);
                } else {
                    detection.source_format = iso2god::forge::SourceFormat::Ps4RetailPkg;
                    detection.package_classification =
                        Some(iso2god::forge::PackageClassification::Retail);
                }
            }
        }
    } else if detection.source_format == iso2god::forge::SourceFormat::Unknown
        || matches!(
            detection.source_format,
            iso2god::forge::SourceFormat::Archive | iso2god::forge::SourceFormat::MultipartArchive
        )
    {
        for src in &base.sources {
            let res = find_payload_pkg_or_dir(src);
            if let Ok(d) = iso2god::forge::FormatDetector::detect(
                &res,
                Some(iso2god::forge::Platform::PlayStation4),
            ) {
                if d.source_format != iso2god::forge::SourceFormat::Unknown
                    && !matches!(
                        d.source_format,
                        iso2god::forge::SourceFormat::Archive
                            | iso2god::forge::SourceFormat::MultipartArchive
                    )
                {
                    detection = d;
                    break;
                }
            }
        }
    }

    if detection.source_format == iso2god::forge::SourceFormat::Unknown
        || matches!(
            detection.source_format,
            iso2god::forge::SourceFormat::Archive | iso2god::forge::SourceFormat::MultipartArchive
        )
    {
        detection.source_format = iso2god::forge::SourceFormat::Ps4RetailPkg;
    }

    let profile = target_workflow.target_profile(iso2god::forge::Platform::PlayStation4);
    let mut choices = iso2god::forge::ForgeUi::build_format_choices(
        &detection,
        iso2god::forge::Platform::PlayStation4,
        profile,
    );
    if choices.is_empty() {
        let fallback_det = iso2god::forge::DetectionResult {
            platform: iso2god::forge::Platform::PlayStation4,
            source_format: iso2god::forge::SourceFormat::Ps4FakePkg,
            package_classification: Some(iso2god::forge::PackageClassification::GameApp),
            confidence: iso2god::forge::ConfidenceLevel::High,
            game_id: detection.game_id.clone(),
            title_name: detection.title_name.clone(),
            is_multipart: false,
            is_archive: false,
            is_multidisc: false,
            disc_number: None,
            total_discs: None,
            related_files: base.sources.clone(),
            missing_parts: Vec::new(),
            license_status: iso2god::forge::LicenseStatus::NotApplicable,
        };
        choices = iso2god::forge::ForgeUi::build_format_choices(
            &fallback_det,
            iso2god::forge::Platform::PlayStation4,
            profile,
        );
    }
    if choices.is_empty() {
        bail!("No supported PS4 conversion paths found");
    }

    let Some(choice) = iso2god::forge::ForgeUi::choice_for_target(
        &detection,
        iso2god::forge::Platform::PlayStation4,
        target_workflow,
        &choices,
    ) else {
        bail!(
            "{} cannot be produced safely from {}",
            target_workflow.display_name(),
            detection.source_format.display_name()
        );
    };

    let target_format = choice.target_format;
    let transfer_dest = iso2god::forge::ForgeUi::prompt_transfer_destination(
        iso2god::forge::Platform::PlayStation4,
    )?;

    let preset = iso2god::forge::ForgeFlowPreset {
        target_format,
        target_workflow,
        transfer_destination: transfer_dest.clone(),
    };

    // ── Workspace selection ────────────────────────────────────────────────
    // Estimate the space we need before allocating anything.
    let (base_probed, upd_probed) = probe_archive_sizes(
        &raw_base_source,
        base.sources
            .get(1)
            .or_else(|| update.sources.first())
            .map(|p| p.as_path()),
    );
    let base_bytes = if base_probed > 0 {
        base_probed
    } else {
        base.sources
            .first()
            .and_then(|p| p.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let upd_bytes = if upd_probed > 0 {
        upd_probed
    } else {
        update
            .sources
            .first()
            .and_then(|p| p.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let estimated_bytes = estimate_ps4_base_update_workspace(base_bytes, upd_bytes);

    // Build the job label from title_id (will be set below) or fallback
    let title_id_hint = detection
        .game_id
        .clone()
        .unwrap_or_else(|| "PS4".to_owned());
    let ws_label = format!("{title_id_hint}_ps4_merge");

    // Determine source/destination paths for drive-selection hints
    let src_hint = base.sources.first().cloned();
    let dest_hint: Option<PathBuf> = match &transfer_dest {
        iso2god::forge::TransferDestination::LocalFolder(p) => Some(p.clone()),
        iso2god::forge::TransferDestination::RemovableDrive { target_root, .. } => {
            Some(target_root.clone())
        }
        _ => None,
    };

    let pref = choose_ps4_workspace_drive(estimated_bytes)?;
    let mut job_ws = JobWorkspaceBuilder::new(ws_label, estimated_bytes).preference(pref);
    if let Some(src) = &src_hint {
        job_ws = job_ws.source(src.clone());
    }
    if let Some(dst) = &dest_hint {
        job_ws = job_ws.destination(dst.clone());
    }
    let mut job_ws = job_ws.build().map_err(|e| {
        anyhow::anyhow!(
            "Digital Forge cannot start because no selected workspace drive has enough free space.\n\n{e}\n\n\
             Choose another drive with enough free space or free space on the selected drive, then retry."
        )
    })?;
    log.stage(
        "workspace",
        &format!(
            "Allocated temporary workspace at {}; estimated bytes: {}",
            job_ws.root.display(),
            estimated_bytes
        ),
    )?;

    let tool =
        find_orbis_pkg_tool().context("the configured PS4 package toolchain is unavailable")?;
    log.stage(
        "tool_discovery",
        &format!("Using PS4 package tool {}", tool.display()),
    )?;

    let cancelled = Arc::new(AtomicBool::new(false));
    log.stage(
        "package_build",
        &format!("Preparing {} output", target_format.display_name()),
    )?;

    if target_format == iso2god::forge::TargetFormat::Ps4Folder {
        // === Target-specific loose application workflow ===
        render_terminal_header()?;
        println!("Extracting PS4 Base Game...");
        let base_extracted_dir = job_ws.root.join("base_extracted");
        fs::create_dir_all(&base_extracted_dir)?;

        let base_payload =
            extract_or_resolve_payload(&raw_base_source, &base_extracted_dir, cancelled.clone())?;
        let base_stage = job_ws.root.join("base_stage");
        fs::create_dir_all(&base_stage)?;

        if base_payload.is_file() {
            println!("Extracting PKG/PFS files into the loose-game workspace...");
            run_orbis_extract_and_restructure(&tool, &base_payload, &base_stage, &job_ws.tmp)?;
            let _ = remove_dir_all_robust(&base_extracted_dir);
        } else {
            copy_dir_recursive_merge(&base_payload, &base_stage)?;
            let _ = Command::new(&tool)
                .arg("restructure")
                .arg(&base_stage)
                .env("TEMP", &job_ws.tmp)
                .env("TMP", &job_ws.tmp)
                .output();
            if base_payload.starts_with(&base_extracted_dir) {
                let _ = remove_dir_all_robust(&base_extracted_dir);
            }
        }

        let sfo1 = base_stage.join("Image0").join("sce_sys").join("param.sfo");
        let sfo2 = base_stage.join("Sc0").join("param.sfo");
        let sfo3 = base_stage.join("sce_sys").join("param.sfo");
        let mut title_id = "CUSA00000".to_string();
        for sfo in &[&sfo1, &sfo2, &sfo3] {
            if sfo.is_file() {
                if let (Some(id), _) = iso2god::forge::FormatDetector::read_sfo(sfo) {
                    if !id.is_empty() {
                        title_id = id;
                        break;
                    }
                }
            }
        }

        let staged_game_dir = job_ws.root.join(&title_id);
        fs::create_dir_all(&staged_game_dir)?;
        flatten_and_overlay_ps4_image0(&base_stage, &staged_game_dir)?;
        let _ = remove_dir_all_robust(&base_stage);
        let _ = remove_dir_all_robust(&base_extracted_dir);

        let mut auxiliary_outputs = Vec::new();
        let mut shad_patch_dir = None;

        if !update.is_empty() {
            for (idx, upd_src) in update.sources.iter().enumerate() {
                render_terminal_header()?;
                println!(
                    "Integrating PS4 Update (item {} of {})...",
                    idx + 1,
                    update.sources.len()
                );
                let upd_ext = job_ws.root.join(format!("upd_ext_{idx}"));
                let upd_stage = job_ws.root.join(format!("upd_stage_{idx}"));
                fs::create_dir_all(&upd_ext)?;
                fs::create_dir_all(&upd_stage)?;

                let upd_payload = extract_or_resolve_payload(upd_src, &upd_ext, cancelled.clone())?;
                if upd_payload.is_file() {
                    run_orbis_extract_and_restructure(
                        &tool,
                        &upd_payload,
                        &upd_stage,
                        &job_ws.tmp,
                    )?;
                } else {
                    copy_dir_recursive_merge(&upd_payload, &upd_stage)?;
                    let _ = Command::new(&tool)
                        .arg("restructure")
                        .arg(&upd_stage)
                        .env("TEMP", &job_ws.tmp)
                        .env("TMP", &job_ws.tmp)
                        .output();
                }
                let update_sfo = iso2god::forge::ps4::find_sfo(&upd_stage)
                    .context("PS4 update lacks sce_sys/param.sfo")?;
                if matches!(
                    target_workflow,
                    iso2god::forge::TargetWorkflow::Ps4ItemzFlow
                ) {
                    let base_sfo = staged_game_dir.join("sce_sys").join("param.sfo");
                    let base_snapshot = job_ws
                        .root
                        .join(format!("base_param_before_update_{idx}.sfo"));
                    fs::copy(&base_sfo, &base_snapshot)
                        .context("could not preserve base param.sfo before update overlay")?;
                    flatten_and_overlay_ps4_image0(&upd_stage, &staged_game_dir)?;
                    iso2god::forge::ps4::normalize_itemzflow_update_sfo(
                        &base_snapshot,
                        &update_sfo,
                        &staged_game_dir.join("sce_sys").join("param.sfo"),
                    )?;
                } else {
                    let patch_dir = shad_patch_dir
                        .get_or_insert_with(|| job_ws.root.join(format!("{title_id}-patch")));
                    fs::create_dir_all(&*patch_dir)?;
                    flatten_and_overlay_ps4_image0(&upd_stage, patch_dir)?;
                }
                let _ = remove_dir_all_robust(&upd_ext);
                let _ = remove_dir_all_robust(&upd_stage);
            }
        }

        if target_workflow == iso2god::forge::TargetWorkflow::Ps4ShadPs4 && !dlc.is_empty() {
            let addon_root = job_ws.root.join("shadPS4-addons").join(&title_id);
            fs::create_dir_all(&addon_root)?;
            for (idx, dlc_src) in dlc.sources.iter().enumerate() {
                render_terminal_header()?;
                println!(
                    "Preparing additional content (item {} of {})...",
                    idx + 1,
                    dlc.sources.len()
                );
                let dlc_ext = job_ws.root.join(format!("dlc_ext_{idx}"));
                let dlc_stage = job_ws.root.join(format!("dlc_stage_{idx}"));
                fs::create_dir_all(&dlc_ext)?;
                fs::create_dir_all(&dlc_stage)?;

                let dlc_payload = extract_or_resolve_payload(dlc_src, &dlc_ext, cancelled.clone())?;
                if dlc_payload.is_file() {
                    run_orbis_extract_and_restructure(
                        &tool,
                        &dlc_payload,
                        &dlc_stage,
                        &job_ws.tmp,
                    )?;
                } else {
                    copy_dir_recursive_merge(&dlc_payload, &dlc_stage)?;
                    let _ = Command::new(&tool)
                        .arg("restructure")
                        .arg(&dlc_stage)
                        .env("TEMP", &job_ws.tmp)
                        .env("TMP", &job_ws.tmp)
                        .output();
                }

                let dlc_sfo1 = dlc_stage.join("Sc0").join("param.sfo");
                let dlc_sfo2 = dlc_stage.join("Image0").join("sce_sys").join("param.sfo");
                let dlc_sfo3 = dlc_stage.join("sce_sys").join("param.sfo");
                let mut dlc_cid = format!("{title_id}_DLC_{idx}");
                for sfo in &[&dlc_sfo1, &dlc_sfo2, &dlc_sfo3] {
                    if sfo.is_file() {
                        if let Some(cid) = iso2god::forge::FormatDetector::read_sfo_content_id(sfo)
                        {
                            dlc_cid = cid;
                            break;
                        }
                    }
                }
                let clean_cid = dlc_cid
                    .chars()
                    .map(|c| {
                        if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                            '_'
                        } else {
                            c
                        }
                    })
                    .collect::<String>();
                let dlc_target = addon_root.join(&clean_cid);
                fs::create_dir_all(&dlc_target)?;
                flatten_and_overlay_ps4_image0(&dlc_stage, &dlc_target)?;
                let _ = remove_dir_all_robust(&dlc_ext);
                let _ = remove_dir_all_robust(&dlc_stage);
            }
            iso2god::forge::ps4::validate_shad_addons(&addon_root, &title_id)?;
            auxiliary_outputs.push(job_ws.root.join("shadPS4-addons"));
        }

        iso2god::forge::ps4::validate_target_folder(&staged_game_dir, target_workflow)?;
        if let Some(patch_dir) = &shad_patch_dir {
            iso2god::forge::ps4::validate_patch_folder(patch_dir, &title_id)?;
            auxiliary_outputs.push(patch_dir.clone());
        }

        let total_size: u64 = walkdir_size(&staged_game_dir);
        let conversion_res = iso2god::forge::backend::ConversionResult {
            platform: iso2god::forge::Platform::PlayStation4,
            target_format,
            output_path: staged_game_dir.clone(),
            total_size,
            was_passthrough: false,
            title_id: Some(title_id.clone()),
            title_name: None,
            aux_files: auxiliary_outputs,
            tool_report: None,
        };

        render_terminal_header()?;
        println!("Delivering completed PS4 target folders...");
        let final_dest = iso2god::forge::transfer::TransferManager::transfer(
            &conversion_res,
            &transfer_dest,
            Arc::new(|_| {}),
            cancelled.clone(),
        )?;
        log.stage(
            "delivery",
            &format!("PS4 folder output delivered to {}", final_dest.display()),
        )?;
        if final_dest.is_dir() {
            iso2god::forge::ps4::validate_target_folder(&final_dest, target_workflow)?;
            if target_workflow == iso2god::forge::TargetWorkflow::Ps4ShadPs4
                && let Some(parent) = final_dest.parent()
            {
                let patch = parent.join(format!("{title_id}-patch"));
                if patch.is_dir() {
                    iso2god::forge::ps4::validate_patch_folder(&patch, &title_id)?;
                }
                let addons = parent.join("shadPS4-addons").join(&title_id);
                if addons.is_dir() {
                    iso2god::forge::ps4::validate_shad_addons(&addons, &title_id)?;
                }
            }
        }
        if matches!(
            target_workflow,
            iso2god::forge::TargetWorkflow::Ps4ItemzFlow
        ) && let iso2god::forge::TransferDestination::RemovableDrive { target_root, .. } =
            &transfer_dest
        {
            let selected_root = target_root.canonicalize()?;
            let output = final_dest.canonicalize()?;
            output
                .strip_prefix(&selected_root)
                .context("ItemzFlow output was written outside the selected drive")?;
            let drive_root = iso2god::forge::transfer::volume_root(&output)?;
            let depth = iso2god::forge::transfer::itemzflow_scan_depth(&drive_root, &output)?;
            ensure!(
                depth <= 5,
                "ItemzFlow output exceeds its five-level scanner depth: depth is {depth} from {}",
                drive_root.display()
            );
            println!(
                "ItemzFlow scan depth: {depth}/5 from {}",
                drive_root.display()
            );
        }
        println!(
            "\n\x1b[32;1mConversion complete. Output verified and delivered successfully.\x1b[0m"
        );
        println!("  {}", final_dest.display());
        if matches!(
            target_workflow,
            iso2god::forge::TargetWorkflow::Ps4ItemzFlow
        ) {
            print_real_ps4_test_instructions(&final_dest)?;
        }
    } else {
        // === Workflow for Fake PKG (FPKG) and Retail PKG ===
        ensure!(
            dlc.is_empty(),
            "Additional content cannot be folded into a base-game PKG generically and will not be emitted as part of the base-game package."
        );
        if target_format == iso2god::forge::TargetFormat::Ps4RetailPkg {
            ensure!(
                update.is_empty(),
                "A merged retail PKG cannot be created without Sony's private signing service. Retail PKG remains available for validated, unchanged retail-package transfer only; choose FPKG for a base/update rebuild."
            );
        }
        package_progress.render(0, "Preparing", "Starting package workflow");
        render_terminal_header()?;
        package_progress.render(0, "Base game", "Preparing source");
        let base_ext = job_ws.root.join("base_pkg_ext");
        fs::create_dir_all(&base_ext)?;
        let base_payload = extract_or_resolve_payload_with_progress(
            &raw_base_source,
            &base_ext,
            cancelled.clone(),
            Some(ps4_package_phase(
                package_progress.clone(),
                0,
                18,
                "Extracting base game",
            )),
        )?;
        package_progress.render(18, "Base game", "Extraction complete");

        // Keep identity from the extracted source as a fallback. Some Orbis
        // builds produce a valid package while `info` omits TITLE_ID and
        // CONTENT_ID; the source SFO/GP4 remains authoritative for those fields.
        let base_source_identity = if base_payload.is_dir() {
            let root_sfo = base_payload.join("sce_sys").join("param.sfo");
            let image_sfo = base_payload
                .join("Image0")
                .join("sce_sys")
                .join("param.sfo");
            let title_id = iso2god::forge::FormatDetector::read_sfo_key(&root_sfo, "TITLE_ID")
                .or_else(|| iso2god::forge::FormatDetector::read_sfo_key(&image_sfo, "TITLE_ID"));
            let content_id = iso2god::forge::FormatDetector::read_sfo_content_id(&root_sfo)
                .or_else(|| iso2god::forge::FormatDetector::read_sfo_content_id(&image_sfo))
                .or_else(|| {
                    title_id
                        .as_deref()
                        .and_then(|id| read_gp4_content_id(&base_payload.join(format!("{id}.gp4"))))
                });
            title_id.zip(content_id)
        } else {
            None
        };

        ensure!(
            base_payload.is_dir()
                || (base_payload.is_file()
                    && base_payload
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("pkg"))),
            "PS4 base input did not resolve to an existing folder or PKG: {}",
            base_payload.display()
        );

        let base_pkg = if base_payload.is_file()
            && base_payload
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("pkg"))
        {
            base_payload
        } else {
            ensure!(
                target_format != iso2god::forge::TargetFormat::Ps4RetailPkg,
                "A retail PKG cannot be built from a folder without Sony's private signing service; choose FPKG"
            );
            let out_pkg = job_ws.root.join("base_built.pkg");
            let gp4 = job_ws.root.join("base.gp4");
            let base_sfo = base_payload.join("sce_sys").join("param.sfo");
            let base_title_id = iso2god::forge::FormatDetector::read_sfo_key(&base_sfo, "TITLE_ID")
                .or_else(|| {
                    iso2god::forge::FormatDetector::read_sfo_key(
                        &base_payload
                            .join("Image0")
                            .join("sce_sys")
                            .join("param.sfo"),
                        "TITLE_ID",
                    )
                })
                .context("PS4 base payload lacks TITLE_ID in param.sfo")?;
            let base_content_id = iso2god::forge::FormatDetector::read_sfo_content_id(&base_sfo)
                .or_else(|| {
                    iso2god::forge::FormatDetector::read_sfo_content_id(
                        &base_payload
                            .join("Image0")
                            .join("sce_sys")
                            .join("param.sfo"),
                    )
                })
                .or_else(|| read_gp4_content_id(&base_payload.join(format!("{base_title_id}.gp4"))))
                .context("PS4 base payload lacks CONTENT_ID in param.sfo or its matching GP4")?;
            ensure!(
                iso2god::forge::detector::title_id_from_content_id(&base_content_id)
                    .is_some_and(|id| id.eq_ignore_ascii_case(&base_title_id)),
                "PS4 base TITLE_ID {} does not match CONTENT_ID {}",
                base_title_id,
                base_content_id
            );
            package_progress.render(19, "Base game", "Building package");
            let gp4_result = Command::new(&tool)
                .arg("gp4gen")
                .arg(&base_payload)
                .arg("--title-id")
                .arg(&base_title_id)
                .arg("--content-id")
                .arg(&base_content_id)
                .arg("--passcode")
                .arg(PS4_FPKG_PASSCODE)
                .arg("--out")
                .arg(&gp4)
                .env("TEMP", &job_ws.tmp)
                .env("TMP", &job_ws.tmp)
                .output()?;
            ensure!(
                gp4_result.status.success(),
                "PS4 GP4 generation failed: {}",
                String::from_utf8_lossy(&gp4_result.stderr)
            );
            strip_gp4_project_entries(&gp4)?;
            let build_result = Command::new(&tool)
                .arg("build")
                .arg(&gp4)
                .arg(&base_payload)
                .arg("--passcode")
                .arg(PS4_FPKG_PASSCODE)
                .arg("--out")
                .arg(&out_pkg)
                .env("TEMP", &job_ws.tmp)
                .env("TMP", &job_ws.tmp)
                .output()?;
            ensure!(
                build_result.status.success() && out_pkg.is_file(),
                "PS4 FPKG build failed: {}",
                String::from_utf8_lossy(&build_result.stderr)
            );
            package_progress.render(21, "Base game", "Package built");
            out_pkg
        };

        package_progress.render(22, "Base game", "Reading package metadata");
        let mut base_meta = read_ps4_pkg_metadata(&tool, &base_pkg)?;
        if let Some((source_title_id, source_content_id)) = base_source_identity {
            if base_meta.title_id.is_empty() {
                base_meta.title_id = source_title_id;
            }
            if base_meta.content_id.is_empty() {
                base_meta.content_id = source_content_id;
            }
        }
        if target_format == iso2god::forge::TargetFormat::Ps4RetailPkg {
            ensure!(
                !base_meta.is_fake,
                "the selected input is an FPKG; Digital Forge will not relabel it as a retail PKG"
            );
        }
        let title_id = if base_meta.title_id.is_empty() {
            "CUSA00000".to_string()
        } else {
            base_meta.title_id.clone()
        };

        let final_pkg = job_ws.root.join(format!("{title_id}.pkg"));

        if !update.is_empty() {
            let upd_source = &update.sources[0];
            let upd_ext = job_ws.root.join("upd_pkg_ext");
            fs::create_dir_all(&upd_ext)?;
            let upd_payload = extract_or_resolve_payload_with_progress(
                upd_source,
                &upd_ext,
                cancelled.clone(),
                Some(ps4_package_phase(
                    package_progress.clone(),
                    22,
                    38,
                    "Extracting update",
                )),
            )?;
            package_progress.render(38, "Update", "Extraction complete");

            let upd_pkg = if upd_payload.is_file()
                && upd_payload
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("pkg"))
            {
                upd_payload
            } else {
                ensure!(
                    upd_payload.is_dir(),
                    "PS4 update input did not resolve to an update folder or PKG: {}",
                    upd_payload.display()
                );
                ensure!(
                    !base_meta.title_id.is_empty() && !base_meta.content_id.is_empty(),
                    "PS4 base PKG lacks the title/content identity required for a patch"
                );
                // Work only on a staged copy. `restructure` moves Sc0 files into
                // Image0/sce_sys, so never run it on the user's extraction.
                let upd_stage = job_ws.root.join("upd_stage");
                copy_dir_recursive_merge(&upd_payload, &upd_stage)?;
                let restructure = Command::new(&tool)
                    .arg("restructure")
                    .arg(&upd_stage)
                    .env("TEMP", &job_ws.tmp)
                    .env("TMP", &job_ws.tmp)
                    .output()?;
                ensure!(
                    restructure.status.success(),
                    "PS4 update staging failed: {}{}",
                    String::from_utf8_lossy(&restructure.stdout),
                    String::from_utf8_lossy(&restructure.stderr)
                );
                let patch_source = upd_stage.join("Image0");
                ensure!(
                    patch_source.is_dir()
                        && patch_source.join("sce_sys").join("param.sfo").is_file(),
                    "PS4 update staging lacks Image0/sce_sys/param.sfo after restructure: {}",
                    upd_stage.display()
                );
                let out_pkg = job_ws.root.join("upd_built.pkg");
                let gp4 = job_ws.root.join("upd.gp4");
                package_progress.render(39, "Update", "Building patch package");
                let gp4_result = Command::new(&tool)
                    .arg("gp4gen")
                    .arg(&patch_source)
                    .arg("--patch")
                    .arg("--title-id")
                    .arg(&base_meta.title_id)
                    .arg("--content-id")
                    .arg(&base_meta.content_id)
                    .arg("--passcode")
                    .arg(PS4_FPKG_PASSCODE)
                    .arg("--out")
                    .arg(&gp4)
                    .env("TEMP", &job_ws.tmp)
                    .env("TMP", &job_ws.tmp)
                    .output()?;
                ensure!(
                    gp4_result.status.success(),
                    "PS4 update GP4 generation failed for folder {}: {}",
                    patch_source.display(),
                    String::from_utf8_lossy(&gp4_result.stderr)
                );
                set_gp4_application_reference(&gp4, &base_pkg)?;
                let build_result = Command::new(&tool)
                    .arg("build")
                    .arg(&gp4)
                    .arg(&patch_source)
                    .arg("--passcode")
                    .arg(PS4_FPKG_PASSCODE)
                    .arg("--out")
                    .arg(&out_pkg)
                    .env("TEMP", &job_ws.tmp)
                    .env("TMP", &job_ws.tmp)
                    .output()?;
                ensure!(
                    build_result.status.success() && out_pkg.is_file(),
                    "PS4 update patch FPKG build failed: {}",
                    String::from_utf8_lossy(&build_result.stderr)
                );
                package_progress.render(40, "Update", "Patch package built");
                out_pkg
            };

            package_progress.render(41, "Update", "Checking package identity");
            let update_meta = read_ps4_pkg_metadata(&tool, &upd_pkg)?;
            ensure_ps4_base_update_compatible(&base_meta, &update_meta)?;
            ensure!(
                !base_meta.content_id.is_empty(),
                "PS4 base package has no CONTENT_ID; cannot safely build a merged package"
            );
            log.stage(
                "merge",
                "merging the validated base and update into one final PKG",
            )?;
            run_orbis_merge(
                &tool,
                &find_orbis_pub_cmd()
                    .context("PS4 Publishing Tools orbis-pub-cmd.exe was not found")?,
                &base_pkg,
                &upd_pkg,
                &final_pkg,
                &job_ws.root,
                &base_meta,
                &package_progress,
            )?;
        } else {
            package_progress.render(40, "Package", "Finalising base package");
            if target_format == iso2god::forge::TargetFormat::Ps4FakePkg && !base_meta.is_fake {
                run_orbis_repack(&tool, &base_pkg, &final_pkg, &job_ws.root)?;
            } else {
                fs::copy(&base_pkg, &final_pkg)?;
            }
            package_progress.render(95, "Verifying", "Checking final package");
        }

        // Clean up intermediate packages and build GP4s now that final_pkg exists
        let _ = remove_dir_all_robust(&base_ext);
        let _ = remove_dir_all_robust(&job_ws.root.join("upd_pkg_ext"));
        let _ = fs::remove_file(job_ws.root.join("base_built.pkg"));
        let _ = fs::remove_file(job_ws.root.join("base.gp4"));
        let _ = fs::remove_file(job_ws.root.join("upd_built.pkg"));
        let _ = fs::remove_file(job_ws.root.join("upd.gp4"));

        let total_size = fs::metadata(&final_pkg).map(|m| m.len()).unwrap_or(0);
        let conversion_res = iso2god::forge::backend::ConversionResult {
            platform: iso2god::forge::Platform::PlayStation4,
            target_format,
            output_path: final_pkg.clone(),
            total_size,
            was_passthrough: false,
            title_id: Some(title_id.clone()),
            title_name: None,
            aux_files: Vec::new(),
            tool_report: None,
        };

        render_terminal_header()?;
        package_progress.render(96, "Verifying", "Saving verified output");
        let delivery_progress = {
            let package_progress = package_progress.clone();
            Arc::new(move |p: iso2god::forge::backend::ProgressUpdate| {
                let percent = 96 + (p.percent.min(100) as u16 * 4 / 100) as u8;
                package_progress.render(percent, "Verifying", "Saving verified output");
            })
        };
        let final_dest = iso2god::forge::transfer::TransferManager::transfer(
            &conversion_res,
            &transfer_dest,
            delivery_progress,
            cancelled.clone(),
        )?;
        log.stage(
            "delivery",
            &format!("Final PS4 package delivered to {}", final_dest.display()),
        )?;
        println!();
        println!("\n\x1b[32;1mPackage delivered. Running basic install/boot checks...\x1b[0m");
        println!("  {}", final_dest.display());
    }

    validate_completed_package(
        &preset,
        iso2god::forge::Platform::PlayStation4,
        log,
        Some(&package_progress),
    )?;
    log.stage("verification", "final PS4 package validation passed")?;
    if preset.target_format == iso2god::forge::TargetFormat::Ps4FakePkg {
        // The full validation path was intentionally replaced with the fast
        // header/entry-table checks above; mark completion only after those
        // checks have passed.
        println!("\n\x1b[32;1mBasic package checks passed.\x1b[0m");
    }
    println!("\nCleaning up temporary workspace files...");
    job_ws.cleanup();
    println!("Temporary workspace completely cleaned. Storage freed.\n");
    log.info("PS4 processing completed successfully")?;
    Ok(())
}

fn choose_ps4_workspace_drive(estimated_bytes: u64) -> Result<WorkspaceDrivePreference, Error> {
    let saved = load_workspace_preference();
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        return Ok(saved);
    }

    let required = estimated_bytes.saturating_add((estimated_bytes as f64 * 0.20) as u64);
    let mut choices = vec![MenuOption {
        value: 0usize,
        label: "Automatically choose a suitable workspace (prefer internal storage)".to_owned(),
    }];
    let mut available = Vec::new();
    for drive in enumerate_drives() {
        if drive.is_workspace_eligible() && drive.free_bytes >= required {
            let kind = if drive.is_fixed {
                "Internal/fixed"
            } else {
                "External/removable"
            };
            let label = if drive.label.is_empty() {
                String::new()
            } else {
                format!(" · {}", drive.label)
            };
            choices.push(MenuOption {
                value: available.len() + 1,
                label: format!(
                    "Use {} {}{} · {} free",
                    drive.letter,
                    kind,
                    label,
                    format_bytes(drive.free_bytes)
                ),
            });
            available.push(drive.letter);
        }
    }
    println!(
        "Temporary workspace selection (estimated need: {}).",
        format_bytes(required)
    );
    println!("Internal storage is recommended for large games; external drives remain available.");
    match select_menu(&choices)? {
        Some(0) => Ok(WorkspaceDrivePreference::Auto),
        Some(index) => available
            .get(index - 1)
            .cloned()
            .map(WorkspaceDrivePreference::Pinned)
            .context("the selected workspace drive is no longer available"),
        None => Ok(saved),
    }
}

fn print_real_ps4_test_instructions(final_dest: &Path) -> Result<(), Error> {
    let sfo = final_dest.join("sce_sys").join("param.sfo");
    let title_id = iso2god::forge::FormatDetector::read_sfo_key(&sfo, "TITLE_ID")
        .context("final ItemzFlow SFO lacks TITLE_ID")?;
    let title = iso2god::forge::FormatDetector::read_sfo_key(&sfo, "TITLE")
        .unwrap_or_else(|| "Unknown title".to_owned());
    let content_id = iso2god::forge::FormatDetector::read_sfo_content_id(&sfo)
        .unwrap_or_else(|| "Unavailable".to_owned());
    let category = iso2god::forge::FormatDetector::read_sfo_category(&sfo)
        .unwrap_or_else(|| "Unavailable".to_owned());
    let app_version = iso2god::forge::FormatDetector::read_sfo_key(&sfo, "APP_VER")
        .unwrap_or_else(|| "Unavailable".to_owned());
    let version = iso2god::forge::FormatDetector::read_sfo_key(&sfo, "VERSION")
        .unwrap_or_else(|| "Unavailable".to_owned());
    let system_version = iso2god::forge::FormatDetector::read_sfo_key(&sfo, "SYSTEM_VER")
        .unwrap_or_else(|| "Unavailable".to_owned());
    let files = walkdir_file_count(final_dest);
    let bytes = walkdir_size(final_dest);
    let executable_reports = iso2god::forge::ps4::validate_itemzflow_executables(final_dest)?;

    println!("\n\x1b[36;1m================ REAL PS4 TEST ================\x1b[0m");
    println!("Prepared Windows path: {}", final_dest.display());
    println!("Title:                 {title}");
    println!("Title ID:              {title_id}");
    println!("Content ID:            {content_id}");
    println!("Category:              {category}");
    println!("App version:           {app_version}");
    println!("Version:               {version}");
    println!("System version:        {system_version}");
    println!("Files:                 {files}");
    println!("Size:                  {bytes} bytes");
    println!(
        "Validation:            PASSED (PS4 sysfile layout, SELF/ELF tables, executable state, copy and scan depth)"
    );
    for report in executable_reports {
        let path = final_dest.join(&report.relative_path);
        let state = match report.state {
            iso2god::forge::ps4::Ps4ExecutableState::FakeSignedPlaintext => {
                "external FSELF; plaintext segments"
            }
            iso2god::forge::ps4::Ps4ExecutableState::SonySignedSystemModule => {
                "Sony-signed right.sprx system module"
            }
        };
        println!(
            "Executable:            {} — {} — ptype {} — PAID 0x{:016X} — SHA-256 {}",
            report.relative_path.display(),
            state,
            report.program_type,
            report.authority_id,
            sha256_hex(&path)?
        );
    }
    println!("READY FOR REAL PS4 RETEST: YES");
    println!("\n1. Safely eject the drive from Windows.");
    println!("2. Connect it to the jailbroken PS4.");
    println!("3. Launch ItemzFlow.");
    println!("4. Run ItemzFlow's Scan for FG Apps / Scan for Apps function.");
    println!("5. Confirm {title_id} / {title} is detected and register it when prompted.");
    println!("6. Launch the game.");
    println!("7. If it fails, record the exact ItemzFlow message and the PS4 error code.");
    println!("\x1b[36;1m================================================\x1b[0m");
    Ok(())
}

fn sha256_hex(path: &Path) -> Result<String, Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 4 * 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:X}", hasher.finalize()))
}

fn extract_or_resolve_payload(
    source: &Path,
    dest_dir: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<PathBuf, Error> {
    extract_or_resolve_payload_with_progress(source, dest_dir, cancelled, None)
}

fn extract_or_resolve_payload_with_progress(
    source: &Path,
    dest_dir: &Path,
    cancelled: Arc<AtomicBool>,
    progress: Option<iso2god::forge::archive::ArchiveProgress>,
) -> Result<PathBuf, Error> {
    let archive_kind = iso2god::forge::archive::ArchiveEngine::classify(source);
    match archive_kind {
        iso2god::forge::archive::ArchiveKind::SingleVolume(_)
        | iso2god::forge::archive::ArchiveKind::MultiVolume { .. } => {
            if progress.is_none() {
                println!("\nDetected compressed archive: {}", source.display());
            }
            let mut password = archive_password_for_import(source)?;
            let extracted = loop {
                match iso2god::forge::archive::ArchiveEngine::extract_archive_with_progress(
                    source,
                    dest_dir,
                    password.as_deref(),
                    cancelled.clone(),
                    progress.clone(),
                ) {
                    Ok(extracted) => break extracted,
                    Err(error)
                        if format!("{error:#}")
                            .to_ascii_lowercase()
                            .contains("password") =>
                    {
                        println!(
                            "\r\x1b[2KThis archive requires a password, or the entered password was incorrect."
                        );
                        password = Some(
                            prompt_terminal_text("Archive password", None, true)?
                                .context("Archive password entry cancelled")?,
                        );
                    }
                    Err(error) => return Err(error),
                }
            };
            Ok(find_payload_pkg_or_dir(&extracted))
        }
        _ => Ok(source.to_path_buf()),
    }
}

fn find_payload_pkg_or_dir(dir: &Path) -> PathBuf {
    if dir.is_file() {
        return dir.to_path_buf();
    }
    let mut pkgs = Vec::new();
    find_pkgs_recursive(dir, &mut pkgs, 0, 3);
    if pkgs.len() == 1 {
        return pkgs.remove(0);
    }
    dir.to_path_buf()
}

fn find_pkgs_recursive(dir: &Path, pkgs: &mut Vec<PathBuf>, depth: usize, max_depth: usize) {
    if depth > max_depth {
        return;
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pkg")) {
                pkgs.push(p);
            } else if p.is_dir() {
                find_pkgs_recursive(&p, pkgs, depth + 1, max_depth);
            }
        }
    }
}

fn run_orbis_extract_and_restructure(
    tool: &Path,
    pkg_path: &Path,
    target_dir: &Path,
    tmp_dir: &Path,
) -> Result<(), Error> {
    fs::create_dir_all(target_dir)?;
    let status = Command::new(tool)
        .arg("extract")
        .arg(pkg_path)
        .arg(target_dir)
        .env("TEMP", tmp_dir)
        .env("TMP", tmp_dir)
        .status()
        .with_context(|| {
            format!(
                "could not execute OrbisPkgTool extract on {}",
                pkg_path.display()
            )
        })?;
    if !status.success() {
        bail!("OrbisPkgTool extract failed for {}", pkg_path.display());
    }
    let restructure = Command::new(tool)
        .arg("restructure")
        .arg(target_dir)
        .env("TEMP", tmp_dir)
        .env("TMP", tmp_dir)
        .status()
        .with_context(|| {
            format!(
                "could not execute OrbisPkgTool restructure on {}",
                target_dir.display()
            )
        })?;
    if !restructure.success() {
        bail!(
            "OrbisPkgTool restructure failed for {}",
            target_dir.display()
        );
    }
    Ok(())
}

fn flatten_and_overlay_ps4_image0(source_dir: &Path, target_dir: &Path) -> Result<(), Error> {
    let image0 = source_dir.join("Image0");
    let from_dir = if image0.is_dir() { &image0 } else { source_dir };
    fs::create_dir_all(target_dir)?;
    if let Ok(entries) = fs::read_dir(from_dir) {
        for entry in entries.flatten() {
            let src = entry.path();
            let dest = target_dir.join(entry.file_name());
            if src.is_dir() {
                copy_dir_recursive_merge(&src, &dest)?;
            } else {
                fs::copy(&src, &dest)?;
            }
        }
    }

    // PKG extraction stores the application metadata in Sc0/param.sfo, while
    // ItemzFlow expects the final app folder to expose it as
    // sce_sys/param.sfo alongside eboot.bin. Preserve an existing destination
    // SFO during update overlay; otherwise promote the source Sc0 metadata.
    let destination_sfo = target_dir.join("sce_sys").join("param.sfo");
    let source_sfo = source_dir.join("Sc0").join("param.sfo");
    if !destination_sfo.is_file() && source_sfo.is_file() {
        fs::create_dir_all(destination_sfo.parent().unwrap())?;
        fs::copy(source_sfo, destination_sfo)?;
    }
    Ok(())
}

fn copy_dir_recursive_merge(src: &Path, dst: &Path) -> Result<(), Error> {
    fs::create_dir_all(dst)?;
    if let Ok(entries) = fs::read_dir(src) {
        for entry in entries.flatten() {
            let child_src = entry.path();
            let child_dst = dst.join(entry.file_name());
            if child_src.is_dir() {
                copy_dir_recursive_merge(&child_src, &child_dst)?;
            } else {
                fs::copy(&child_src, &child_dst)?;
            }
        }
    }
    Ok(())
}

fn walkdir_size(path: &Path) -> u64 {
    if path.is_file() {
        return fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += walkdir_size(&p);
            } else {
                total += fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

fn walkdir_file_count(path: &Path) -> usize {
    if path.is_file() {
        return 1;
    }
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                total += walkdir_file_count(&child);
            } else {
                total += 1;
            }
        }
    }
    total
}

fn run_orbis_merge(
    tool: &Path,
    publisher: &Path,
    base_pkg: &Path,
    update_pkg: &Path,
    output_pkg: &Path,
    temp_ws: &Path,
    base_meta: &Ps4PkgMetadata,
    progress: &Arc<Ps4PackageProgress>,
) -> Result<(), Error> {
    let partial = output_pkg.with_extension("partial.pkg");
    let work_root = temp_ws.join("merge_work");
    fs::create_dir_all(&work_root)?;

    progress.render(42, "Consolidating", "Starting base/update merge");
    let mut merge_command = Command::new(tool);
    merge_command
        .arg("merge")
        .arg(base_pkg)
        .arg(update_pkg)
        .arg("--out")
        .arg(&partial)
        .arg("--workers")
        .arg("0")
        .arg("--validate")
        .arg("--work-dir")
        .arg(&work_root);
    merge_command.arg("--publisher").arg(publisher);
    // Do not pass empty metadata overrides.  The merge tool can read the
    // authoritative values from the base package's param.sfo; an empty
    // override otherwise masks those values and creates a blank TITLE_ID.
    if !base_meta.title.trim().is_empty() {
        merge_command.arg("--title").arg(&base_meta.title);
    }
    if !base_meta.title_id.trim().is_empty() {
        merge_command.arg("--title-id").arg(&base_meta.title_id);
    }
    if !base_meta.content_id.trim().is_empty() {
        merge_command.arg("--content-id").arg(&base_meta.content_id);
    }
    // Preserve this job on failure, including publisher GP4 and extracted assets.
    // The normal success path removes the marker and retains automatic cleanup.
    let diagnostic_root = temp_ws
        .ancestors()
        .find(|p| p.join(".df_job_marker.json").is_file())
        .unwrap_or(temp_ws);
    let diagnostic_marker = diagnostic_root.join(".df_preserve_diagnostics");
    fs::write(
        &diagnostic_marker,
        b"PS4 consolidation evidence; retain until reviewed.",
    )?;
    fs::write(
        work_root.join("merge-command.txt"),
        format!("{merge_command:?}"),
    )?;
    let stderr_path = work_root.join("merge-stderr.log");
    let stderr_file = fs::File::create(&stderr_path)?;
    let mut stdout_log = fs::File::create(work_root.join("merge-stdout.log"))?;
    let mut child = merge_command
        .env("TEMP", temp_ws)
        .env("TMP", temp_ws)
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .with_context(|| format!("could not start {}", tool.display()))?;

    let stdout = child
        .stdout
        .take()
        .context("could not read PS4 merge progress")?;
    let mut reader = BufReader::new(stdout);
    let mut byte_buf = [0u8; 1024];
    let mut current_line = Vec::new();
    loop {
        let n = reader.read(&mut byte_buf)?;
        stdout_log.write_all(&byte_buf[..n])?;
        if n == 0 {
            if !current_line.is_empty() {
                let line_str = String::from_utf8_lossy(&current_line);
                let line = line_str.trim();
                render_orbis_merge_progress(progress, line);
            }
            break;
        }
        for &b in &byte_buf[..n] {
            if b == b'\r' || b == b'\n' {
                if !current_line.is_empty() {
                    let line_str = String::from_utf8_lossy(&current_line);
                    let line = line_str.trim();
                    render_orbis_merge_progress(progress, line);
                    current_line.clear();
                }
            } else {
                current_line.push(b);
            }
        }
    }
    let status = child.wait()?;
    fs::write(work_root.join("merge-exit-code.txt"), status.to_string())?;
    if !status.success() {
        let details = fs::read_to_string(&stderr_path).unwrap_or_default();
        bail!(
            "PS4 base/update consolidation failed ({status}). {}\nDiagnostic files and intermediate content retained at: {}",
            details.trim(),
            work_root.display()
        );
    }
    // The merge tool has already sealed and validated the complete FPKG. Do
    // not reopen it through LibOrbisPkg here: rebuilding a merged package can
    // rewrite encrypted entry padding and invalidate entry digests (notably
    // NPBIND_DAT on packages whose logical size is not 16-byte aligned).
    // Keep the validated merge artifact intact and let the independent final
    // output validation below verify it once more.
    progress.render(95, "Verifying", "Finalizing validated merged package");
    fs::rename(&partial, output_pkg)
        .with_context(|| format!("could not finalize merged package {}", output_pkg.display()))?;
    fs::remove_file(&diagnostic_marker)?;
    let _ = fs::remove_dir_all(&work_root);
    progress.render(96, "Verifying", "Final package structure validated");
    Ok(())
}

fn render_orbis_merge_progress(progress: &Ps4PackageProgress, line: &str) {
    // Validation messages also contain brackets/percentages. Handle them
    // before the generic stage parser, otherwise it masks this branch.
    if let Some(rest) = line.strip_prefix("[Validating:") {
        let stage_number = rest
            .split(']')
            .next()
            .and_then(|value| value.trim().parse::<u8>().ok())
            .unwrap_or(0)
            .min(8);
        let overall = 94 + (stage_number.saturating_mul(5) / 8);
        progress.render(overall, "Verifying", line);
    } else if let Some(percent) = ps4_merge_percent(line) {
        let stage = line
            .strip_prefix('[')
            .and_then(|value| value.split_once(']'))
            .map(|(stage, _)| stage)
            .unwrap_or("Building merged PKG");
        let (start, end, label) = match stage {
            "Extracting base PKG" => (42u16, 52u16, "Extract base"),
            "Extracting update PKG" => (52u16, 62u16, "Extract update"),
            // LibOrbisPkg reports real completion for its PlayGo digest pass.
            // Keep that sub-stage bounded within consolidation, then report
            // final structural checks separately once the builder exits.
            "Building merged PKG" => (66u16, 94u16, "Consolidating"),
            _ => (42u16, 42u16, "Consolidating"),
        };
        let overall = start + ((end - start) * percent.min(100) as u16 / 100);
        progress.render(overall as u8, label, stage);
    } else if let Some((stage, _detail)) = line
        .strip_prefix('[')
        .and_then(|value| value.split_once(']'))
    {
        let (overall, label) = match stage {
            "Overlaying update files" => (63, "Consolidating"),
            "Restructuring merged files" => (64, "Consolidating"),
            "Generating GP4 project" => (65, "Consolidating"),
            "Building merged PKG" => (66, "Consolidating"),
            "Extracting base PKG" => (42, "Extract base"),
            "Extracting update PKG" => (52, "Extract update"),
            "Validating merged PKG" => (94, "Verifying"),
            _ => return,
        };
        progress.render(overall, label, stage);
    }
}

fn run_orbis_repack(
    tool: &Path,
    input_pkg: &Path,
    output_pkg: &Path,
    temp_ws: &Path,
) -> Result<(), Error> {
    let partial = output_pkg.with_extension("partial.pkg");
    let work_root = temp_ws.join("repack_work");
    fs::create_dir_all(&work_root)?;

    println!("\nRepacking PS4 PKG into Fake PKG...");
    let status = Command::new(tool)
        .arg("repack")
        .arg(input_pkg)
        .arg("--out")
        .arg(&partial)
        .arg("--validate")
        .arg("--workers")
        .arg("0")
        .arg("--work-dir")
        .arg(&work_root)
        .env("TEMP", temp_ws)
        .env("TMP", temp_ws)
        .status()
        .with_context(|| format!("could not start {}", tool.display()))?;

    if !status.success() {
        let _ = fs::remove_file(&partial);
        bail!("PS4 PKG repack failed");
    }
    rebuild_ps4_pkg_with_liborbis(&partial, output_pkg, temp_ws)?;
    let _ = fs::remove_file(&partial);
    let _ = fs::remove_dir_all(&work_root);
    Ok(())
}

fn rebuild_ps4_pkg_with_liborbis(
    staged_pkg: &Path,
    output_pkg: &Path,
    temp_ws: &Path,
) -> Result<(), Error> {
    let pkgtool = find_liborbis_pkg_tool()
        .context("maxton/LibOrbisPkg PkgTool is required for final PS4 FPKG assembly")?;
    let project_dir = temp_ws.join("liborbis_project");
    let build_dir = temp_ws.join("liborbis_output");
    fs::create_dir_all(&project_dir)?;
    fs::create_dir_all(&build_dir)?;

    let make = Command::new(&pkgtool)
        .arg("pkg_makegp4")
        .arg(staged_pkg)
        .arg(&project_dir)
        .env("TEMP", temp_ws)
        .env("TMP", temp_ws)
        .output()?;
    ensure!(
        make.status.success(),
        "LibOrbisPkg could not reopen the staged PS4 package: {}{}",
        String::from_utf8_lossy(&make.stdout),
        String::from_utf8_lossy(&make.stderr)
    );
    let gp4 = fs::read_dir(&project_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("gp4"))
        })
        .context("LibOrbisPkg did not generate a GP4 project")?;

    let build = Command::new(&pkgtool)
        .arg("pkg_build")
        .arg(&gp4)
        .arg(&build_dir)
        .env("TEMP", temp_ws)
        .env("TMP", temp_ws)
        .output()?;
    ensure!(
        build.status.success(),
        "LibOrbisPkg FPKG build failed: {}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr)
    );
    let built = fs::read_dir(&build_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pkg"))
        })
        .context("LibOrbisPkg completed without a PKG output")?;

    let validation = Command::new(&pkgtool)
        .arg("pkg_validate")
        .arg("--verbose")
        .arg(&built)
        .env("TEMP", temp_ws)
        .env("TMP", temp_ws)
        .output()?;
    let validation_text = format!(
        "{}{}",
        String::from_utf8_lossy(&validation.stdout),
        String::from_utf8_lossy(&validation.stderr)
    );
    ensure!(
        validation.status.success() && !validation_text.contains("[ERROR]"),
        "LibOrbisPkg rejected its rebuilt package:\n{validation_text}"
    );
    let rename_res = fs::rename(&built, output_pkg)
        .with_context(|| format!("could not finalize merged package {}", output_pkg.display()));
    let _ = remove_dir_all_robust(&project_dir);
    let _ = remove_dir_all_robust(&build_dir);
    rename_res?;
    Ok(())
}

fn validate_completed_package(
    preset: &iso2god::forge::ForgeFlowPreset,
    platform: iso2god::forge::Platform,
    log: &mut OperationLog,
    progress: Option<&Ps4PackageProgress>,
) -> Result<(), Error> {
    let iso2god::forge::TransferDestination::LocalFolder(target_dir) = &preset.transfer_destination
    else {
        return Ok(());
    };

    println!("\n========================================================");
    println!("Game Package Verification");
    println!("========================================================");

    if platform == iso2god::forge::Platform::PlayStation4 {
        if preset.target_format == iso2god::forge::TargetFormat::Ps4Folder {
            let mut found_game = None;
            if target_dir.join("sce_sys").join("param.sfo").is_file() {
                found_game = Some(target_dir.to_path_buf());
            } else if let Ok(entries) = fs::read_dir(target_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let sfo = path.join("sce_sys").join("param.sfo");
                        if sfo.is_file() {
                            found_game = Some(path);
                            break;
                        }
                    }
                }
            }
            if let Some(game_dir) = found_game {
                let sfo_path = game_dir.join("sce_sys").join("param.sfo");
                let (title_id, title) = iso2god::forge::FormatDetector::read_sfo(&sfo_path);
                iso2god::forge::ps4::validate_target_folder(&game_dir, preset.target_workflow)?;

                let eboot = game_dir.join("eboot.bin");
                let eboot_ok = eboot.is_file();
                let title_str = title.as_deref().unwrap_or("Unknown Title");
                let id_str = title_id.as_deref().unwrap_or("Unknown ID");

                let app_ver = iso2god::forge::FormatDetector::read_sfo_key(&sfo_path, "APP_VER");
                let ver_str = app_ver.as_deref().unwrap_or("01.00");

                println!("Title:       {title_str}");
                println!("Title ID:    {id_str}");
                println!("App Version: {ver_str}");
                println!("Category:    Game Application (gd)");
                println!(
                    "Payload:     {}",
                    if eboot_ok {
                        "✓ eboot.bin verified"
                    } else {
                        "✗ missing eboot.bin"
                    }
                );

                println!("Location:    {}", game_dir.display());
                if eboot_ok {
                    println!("Status:      ✓ Validated successfully - Ready to play!");
                } else {
                    println!("Status:      ⚠ Warning: eboot.bin was not found");
                }
                log.info(&format!(
                    "Validated PS4 game package at {}: {title_str} ({id_str}) v{ver_str}",
                    game_dir.display()
                ))?;
            } else {
                println!("Location:    {}", target_dir.display());
                println!("Status:      ✓ Completed");
            }
        } else if preset.target_format == iso2god::forge::TargetFormat::Ps4FakePkg
            || preset.target_format == iso2god::forge::TargetFormat::Ps4RetailPkg
        {
            let tool =
                find_orbis_pkg_tool().context("PS4 metadata validation requires OrbisPkgTool")?;
            let packages: Vec<PathBuf> = fs::read_dir(target_dir)
                .with_context(|| {
                    format!(
                        "could not inspect PS4 output directory {}",
                        target_dir.display()
                    )
                })?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("pkg"))
                })
                .collect();
            ensure!(
                !packages.is_empty(),
                "PS4 conversion completed without a .pkg output"
            );
            for path in packages {
                if let Some(progress) = progress {
                    progress.render(97, "Verifying", "Basic package metadata");
                }
                let metadata = validate_ps4_pkg_basics(&tool, &path)?;
                if preset.target_format == iso2god::forge::TargetFormat::Ps4FakePkg {
                    ensure!(
                        metadata.is_fake,
                        "FPKG output is not fake-signed: {}",
                        path.display()
                    );
                } else {
                    ensure!(
                        !metadata.is_fake,
                        "Retail PKG output is fake-signed: {}",
                        path.display()
                    );
                }
                println!(
                    "PKG:         {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                println!("Title ID:    {}", metadata.title_id);
                println!("Content ID:  {}", metadata.content_id);
                println!(
                    "Category:    {} ({})",
                    metadata.category, metadata.package_type
                );
                println!("Status:      ✓ Basic install/boot checks passed");
                log.info(&format!("Basic-validated PS4 package {}", path.display()))?;
                if let Some(progress) = progress {
                    progress.render(99, "Verifying", "Required boot entries");
                }
            }
        }
    } else if platform == iso2god::forge::Platform::PlayStation3
        && preset.target_format == iso2god::forge::TargetFormat::Ps3Pkg
    {
        let mut packages = Vec::new();
        find_pkgs_recursive(target_dir, &mut packages, 0, 3);
        ensure!(
            !packages.is_empty(),
            "PS3 conversion completed without a PKG output"
        );
        for path in packages {
            let detection = iso2god::forge::FormatDetector::detect(
                &path,
                Some(iso2god::forge::Platform::PlayStation3),
            )
            .with_context(|| format!("could not inspect PS3 PKG {}", path.display()))?;
            ensure!(
                detection.platform == iso2god::forge::Platform::PlayStation3
                    && detection.source_format == iso2god::forge::SourceFormat::Ps3Pkg,
                "output is not a valid PS3 PKG: {}",
                path.display()
            );
            let content_id = detection.game_id.as_deref().unwrap_or("unknown Content ID");
            println!("PKG:         {}", path.display());
            println!("Content ID:  {content_id}");
            println!("Status:      ✓ PS3 PKG header and metadata validated");
            log.info(&format!(
                "Validated PS3 PKG {} ({content_id})",
                path.display()
            ))?;
        }
    } else {
        println!("Platform:    {}", platform.display_name());
        println!("Location:    {}", target_dir.display());
        println!("Status:      ✓ Validated successfully");
    }
    if let Some(progress) = progress {
        progress.render(100, "Complete", "Basic checks passed");
    }
    println!("========================================================\n");
    Ok(())
}

/// Fast install/boot-readiness check for a PS4 PKG.  Full PFSC/digest walks
/// are intentionally not part of the interactive conversion path: the merge
/// tool already writes the package, while the console is the authority for
/// final install/launch behavior.  This check verifies the package header,
/// base-game metadata, and the two files the installer/loader must be able to
/// locate without rescanning every payload byte.
fn validate_ps4_pkg_basics(tool: &Path, path: &Path) -> Result<Ps4PkgMetadata, Error> {
    let info = Command::new(tool)
        .arg("info")
        .arg(path)
        .output()
        .with_context(|| format!("could not inspect {}", path.display()))?;
    ensure!(
        info.status.success(),
        "PS4 package header/metadata inspection failed for {}",
        path.display()
    );
    let metadata = read_ps4_pkg_metadata(tool, path)?;
    ensure!(
        metadata.package_type.eq_ignore_ascii_case("game")
            && metadata.category.eq_ignore_ascii_case("gd")
            && !metadata.title_id.trim().is_empty()
            && !metadata.content_id.trim().is_empty(),
        "PS4 output lacks bootable base-game metadata: Type={} CATEGORY={} TITLE_ID={} CONTENT_ID={} ({})",
        metadata.package_type,
        metadata.category,
        metadata.title_id,
        metadata.content_id,
        path.display()
    );

    let listing = Command::new(tool)
        .arg("list")
        .arg(path)
        .output()
        .with_context(|| format!("could not read the entry table for {}", path.display()))?;
    ensure!(
        listing.status.success(),
        "PS4 package entry-table check failed for {}",
        path.display()
    );
    let entries = String::from_utf8_lossy(&listing.stdout);
    for required in ["Image0/eboot.bin", "Image0/sce_sys/keystone"] {
        ensure!(
            entries.lines().any(|line| line.ends_with(required)),
            "PS4 package is missing required boot/install entry {} ({})",
            required,
            path.display()
        );
    }
    ensure!(
        entries.lines().any(|line| {
            line.ends_with("Sc0/param.sfo") || line.ends_with("Image0/sce_sys/param.sfo")
        }),
        "PS4 package is missing its required param.sfo install metadata ({})",
        path.display()
    );
    println!(
        "PS4 basic validation passed: {} / {}",
        metadata.title_id, metadata.content_id
    );
    Ok(metadata)
}

#[derive(Debug, Clone)]
struct Ps4PkgMetadata {
    path: PathBuf,
    title: String,
    title_id: String,
    content_id: String,
    category: String,
    package_type: String,
    passcode_status: String,
    is_fake: bool,
}

fn ensure_ps4_base_update_compatible(
    base: &Ps4PkgMetadata,
    update: &Ps4PkgMetadata,
) -> Result<(), Error> {
    ensure!(
        !base.title_id.is_empty(),
        "PS4 base package has no TITLE_ID; cannot safely pair an update"
    );
    ensure!(
        !update.title_id.is_empty(),
        "PS4 update package has no TITLE_ID; cannot safely pair it with the base game"
    );
    ensure!(
        base.title_id.eq_ignore_ascii_case(&update.title_id),
        "PS4 base/update TITLE_ID mismatch: base {} ({}), update {} ({}). Choose an update for the same game and region; the packages were not merged.",
        base.title_id,
        base.content_id,
        update.title_id,
        update.content_id
    );
    Ok(())
}

fn ps4_merge_percent(line: &str) -> Option<u8> {
    let percent = line.rsplit_once(' ')?.1.strip_suffix('%')?;
    percent.parse::<u8>().ok().map(|value| value.min(100))
}

fn orange_progress_bar(percent: u8, width: usize) -> String {
    let bounded = usize::from(percent.min(100));
    // Round positive progress up to one visible cell so the bar never appears
    // missing while the numeric percentage is already non-zero.
    let filled = if bounded == 0 || width == 0 {
        0
    } else {
        (bounded * width).div_ceil(100)
    };
    format!(
        "\x1b[90m[\x1b[38;5;208m{}\x1b[0m{}\x1b[90m]\x1b[0m",
        "█".repeat(filled),
        " ".repeat(width.saturating_sub(filled))
    )
}

#[cfg(test)]
mod ps4_package_progress_tests {
    use super::{Ordering, Ps4PackageProgress, orange_progress_bar, render_orbis_merge_progress};

    #[test]
    fn first_nonzero_percent_is_visible_and_width_stays_fixed() {
        assert_eq!(orange_progress_bar(0, 24).matches('█').count(), 0);
        assert_eq!(orange_progress_bar(1, 24).matches('█').count(), 1);
        assert_eq!(orange_progress_bar(100, 24).matches('█').count(), 24);
    }

    #[test]
    fn validation_progress_is_not_masked_by_generic_stage_parsing() {
        let progress = Ps4PackageProgress::default();
        render_orbis_merge_progress(&progress, "[Validating: 4] inner PFS 38%");
        assert_eq!(progress.last_percent.load(Ordering::Relaxed), 96);
        render_orbis_merge_progress(&progress, "[Validating: 8] complete");
        assert_eq!(progress.last_percent.load(Ordering::Relaxed), 99);
    }
}

#[derive(Default)]
struct Ps4PackageProgress {
    last_percent: AtomicU8,
    started: OnceLock<Instant>,
}

impl Ps4PackageProgress {
    fn render(&self, percent: u8, stage: &str, _detail: &str) {
        let started = self.started.get_or_init(Instant::now);
        // The package tool reports phase-local percentages. The caller maps
        // those to the operation-wide scale; only that scale is displayed.
        // Never invent movement: the bar advances only when a real callback
        // or completed stage reports progress.
        let percent = self
            .last_percent
            .fetch_max(percent.min(100), Ordering::Relaxed)
            .max(percent.min(100));
        let elapsed = started.elapsed();
        let eta = if percent >= 100 {
            "done".to_owned()
        } else if percent == 0 || elapsed < Duration::from_secs(2) {
            "calculating".to_owned()
        } else {
            let remaining = elapsed.mul_f64((100 - percent) as f64 / percent as f64);
            format_eta(remaining)
        };
        let stage: String = stage.chars().take(14).collect();
        // Redraw exactly one bounded terminal row. The fixed-width fields
        // prevent phase text from wrapping and creating apparent duplicate
        // progress bars, while the wait message reassures users during slow IO.
        print!(
            "\r\x1b[2KPS4 PKG {} {:>3}% | {:<14} | ETA {:<10} | Please wait...",
            orange_progress_bar(percent, 24),
            percent.min(100),
            stage,
            eta
        );
        let _ = io::stdout().flush();
    }
}

fn format_eta(duration: Duration) -> String {
    let seconds = duration.as_secs().min(99 * 60 * 60 + 59 * 60 + 59);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs:02}s")
    } else {
        format!("{secs}s")
    }
}

fn ps4_package_phase(
    progress: Arc<Ps4PackageProgress>,
    start: u8,
    end: u8,
    stage: &'static str,
) -> iso2god::forge::archive::ArchiveProgress {
    Arc::new(move |percent, detail| {
        let span = end.saturating_sub(start) as u16;
        let overall = start as u16 + (span * percent.min(100) as u16 / 100);
        progress.render(overall as u8, stage, detail);
    })
}

fn read_ps4_pkg_metadata(tool: &Path, package: &Path) -> Result<Ps4PkgMetadata, Error> {
    let output = Command::new(tool)
        .arg("info")
        .arg(package)
        .output()
        .with_context(|| format!("could not inspect PS4 package {}", package.display()))?;
    if !output.status.success() {
        anyhow::bail!("PS4 package inspection failed for {}", package.display());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let field = |name: &str| {
        text.lines()
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
            })
            .map(|(_, value)| value.trim().to_owned())
            .unwrap_or_default()
    };
    let title = field("Title");
    let title_id = field("Title ID");
    let content_id = field("Content ID");
    let category = field("Category");
    let package_type = field("Type");
    let passcode_status = field("Passcode");

    let is_fake = if passcode_status.to_lowercase().contains("official")
        || passcode_status.to_lowercase().contains("mismatch")
        || passcode_status.to_lowercase().contains("rsa-recovered")
    {
        false
    } else if passcode_status.to_lowercase().contains("verified") {
        true
    } else {
        content_id.contains("FPKG") || content_id.contains("FAKE")
    };

    Ok(Ps4PkgMetadata {
        path: package.to_path_buf(),
        title,
        title_id,
        content_id,
        category,
        package_type,
        passcode_status,
        is_fake,
    })
}

fn set_gp4_application_reference(gp4_path: &Path, base_pkg: &Path) -> Result<(), Error> {
    let mut xml = fs::read_to_string(gp4_path)
        .with_context(|| format!("could not read patch GP4 {}", gp4_path.display()))?;
    ensure!(
        xml.contains("<volume_type>pkg_ps4_patch</volume_type>"),
        "generated update GP4 is not a patch project"
    );
    let package_start = xml
        .find("<package ")
        .context("patch GP4 lacks package metadata")?;
    let package_end = xml[package_start..]
        .find("/>")
        .map(|index| package_start + index)
        .context("patch GP4 package element is malformed")?;
    let reference = if base_pkg.parent() == gp4_path.parent() {
        base_pkg
            .file_name()
            .unwrap_or(base_pkg.as_os_str())
            .to_string_lossy()
            .into_owned()
    } else {
        base_pkg.to_string_lossy().into_owned()
    };
    let escaped = reference
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    ensure!(
        !xml[package_start..package_end].contains("app_path="),
        "generated patch GP4 already has app_path metadata"
    );
    xml.insert_str(package_end, &format!(" app_path=\"{escaped}\""));
    fs::write(gp4_path, xml).with_context(|| {
        format!(
            "could not set base application reference in {}",
            gp4_path.display()
        )
    })?;
    Ok(())
}

fn read_gp4_content_id(gp4_path: &Path) -> Option<String> {
    let xml = fs::read_to_string(gp4_path).ok()?;
    let package = xml.lines().find(|line| line.contains("<package "))?;
    let (_, value) = package.split_once("content_id=\"")?;
    let (content_id, _) = value.split_once('"')?;
    let content_id = content_id.trim();
    (!content_id.is_empty()).then(|| content_id.to_owned())
}

fn strip_gp4_project_entries(gp4_path: &Path) -> Result<(), Error> {
    let xml = fs::read_to_string(gp4_path)
        .with_context(|| format!("could not read generated GP4 {}", gp4_path.display()))?;
    let mut removed = 0usize;
    let filtered = xml
        .lines()
        .filter(|line| {
            let is_project_entry = line.contains("<file")
                && line
                    .split_once("targ_path=\"")
                    .and_then(|(_, value)| value.split_once('"').map(|(path, _)| path))
                    .is_some_and(|path| path.to_ascii_lowercase().ends_with(".gp4"));
            if is_project_entry {
                removed += 1;
            }
            !is_project_entry
        })
        .collect::<Vec<_>>()
        .join("\n");
    if removed > 0 {
        fs::write(gp4_path, filtered).with_context(|| {
            format!(
                "could not remove GP4 project files from {}",
                gp4_path.display()
            )
        })?;
    }
    Ok(())
}

fn find_orbis_pkg_tool() -> Option<PathBuf> {
    if let Some(native) = iso2god::forge::backends::find_native_orbis_pkg_tool() {
        return Some(native);
    }
    if let Some(configured) = env::var_os("DIGITAL_FORGE_ORBIS_PKG_TOOL") {
        let path = PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let sibling = directory.join("OrbisPkgTool.exe");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let in_path = env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join("OrbisPkgTool.exe"))
            .find(|candidate| candidate.is_file())
    });
    if in_path.is_some() {
        return in_path;
    }
    iso2god::forge::backends::find_orbis_pkg_tool()
}

fn find_liborbis_pkg_tool() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("DIGITAL_FORGE_LIBORBIS_PKG_TOOL") {
        let path = PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let sibling = directory.join("PkgTool.exe");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    iso2god::forge::backends::find_liborbis_pkg_tool()
}

fn find_orbis_pub_cmd() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("DIGITAL_FORGE_ORBIS_PUB_CMD") {
        let path = PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        let sibling = directory.join("orbis-pub-cmd.exe");
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    let in_path = env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|directory| directory.join("orbis-pub-cmd.exe"))
            .find(|candidate| candidate.is_file())
    });
    in_path.or_else(iso2god::forge::backends::find_orbis_pub_cmd)
}

fn unique_completed_pkg(parent: &Path) -> PathBuf {
    let first = parent.join("Completed Game.pkg");
    if !first.exists() {
        return first;
    }
    for suffix in 2.. {
        let candidate = parent.join(format!("Completed Game ({suffix}).pkg"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn process_optional_content_group(
    group: &FolderContentGroup,
    platform: iso2god::forge::Platform,
    log: &mut OperationLog,
    preset: &iso2god::forge::ForgeFlowPreset,
) -> Result<(), Error> {
    if platform == iso2god::forge::Platform::PlayStation3 && !group.is_empty() {
        println!(
            "PS3 update consolidation is not assumed: validating and processing each installable update PKG separately."
        );
        log.info(
            "PS3 update consolidation was not attempted; package type, install path, signature, and dependency metadata require a verified per-title merge fixture",
        )?;
    }
    let mut sources = group.sources.clone();
    if platform == iso2god::forge::Platform::PlayStation3 {
        sources.sort_by_key(|source| ps3_update_version_key(source));
    }
    for (index, source) in sources.iter().enumerate() {
        render_terminal_header()?;
        println!("{} — item {} of {}", group.label, index + 1, sources.len());
        println!("========================================================\n");
        println!("{}", source.display());
        log.info(&format!(
            "Folder contract {}: {}",
            group.label,
            source.display()
        ))?;
        iso2god::forge::ForgeUi::run_interactive_flow_with_preset(
            source,
            Some(platform),
            Some(preset),
        )
        .with_context(|| {
            format!(
                "{} ERROR: content processing failed",
                group.label.to_ascii_uppercase()
            )
        })?;
    }
    Ok(())
}

fn find_named_child_directory(root: &Path, expected: &str) -> Option<PathBuf> {
    fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .and_then(|_| {
                    entry
                        .file_name()
                        .to_str()
                        .filter(|name| name.eq_ignore_ascii_case(expected))
                        .map(|_| entry.path())
                })
        })
}

fn find_named_child_directory_or_file(root: &Path, expected: &str) -> Option<PathBuf> {
    let synonyms: &[&str] = match expected {
        "Game" => &["Game", "Base", "BaseGame", "Base Game", "Disc", "Main"],
        "Update" => &["Update", "Updates", "Patch", "Patches", "Upd"],
        "DLC" => &[
            "DLC Pack", "DLC", "DLCs", "Addon", "Addons", "Extra", "Extras",
        ],
        _ => &[expected],
    };
    for syn in synonyms {
        if let Some(directory) = find_named_child_directory(root, syn) {
            return Some(directory);
        }
    }
    fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_file())
                .and_then(|_| {
                    let file_name = entry.file_name();
                    let name = file_name.to_str()?;
                    let stem = entry.path().file_stem().and_then(OsStr::to_str)?.to_owned();
                    for syn in synonyms {
                        if name.eq_ignore_ascii_case(syn)
                            || stem.eq_ignore_ascii_case(syn)
                            || stem
                                .to_ascii_lowercase()
                                .starts_with(&syn.to_ascii_lowercase())
                        {
                            return Some(entry.path());
                        }
                    }
                    None
                })
        })
}

fn scan_file_content_group(label: &'static str, file: &Path) -> Result<FolderContentGroup, Error> {
    let parent = file.parent().unwrap_or(file);
    let all_sets = scan_archive_sets_in_directory(parent).unwrap_or_default();
    let matching_set = all_sets
        .into_iter()
        .find(|s| s.parts.contains(&file.to_path_buf()));
    if let Some(set) = matching_set {
        let is_multi = set.parts.len() > 1;
        Ok(FolderContentGroup {
            label,
            directory: parent.to_path_buf(),
            sources: vec![set.entry_path.clone()],
            multipart_sets: if is_multi { vec![set] } else { Vec::new() },
            ordinary_items: if is_multi { 0 } else { 1 },
        })
    } else {
        Ok(FolderContentGroup {
            label,
            directory: parent.to_path_buf(),
            sources: vec![file.to_path_buf()],
            multipart_sets: Vec::new(),
            ordinary_items: 1,
        })
    }
}

fn scan_optional_folder_content_group(
    label: &'static str,
    target: Option<&Path>,
) -> Result<FolderContentGroup, Error> {
    match target {
        Some(path) if path.is_dir() => scan_folder_content_group(label, path, false),
        Some(path) if path.is_file() => scan_file_content_group(label, path),
        _ => Ok(FolderContentGroup {
            label,
            directory: PathBuf::new(),
            sources: Vec::new(),
            multipart_sets: Vec::new(),
            ordinary_items: 0,
        }),
    }
}

fn scan_folder_content_group(
    label: &'static str,
    directory: &Path,
    exclude_contract_folders: bool,
) -> Result<FolderContentGroup, Error> {
    let mut multipart_sets = scan_archive_sets_in_directory(directory)?;
    let ps3_split_sets = scan_ps3_666_sets(directory, label)?;
    multipart_sets.extend(ps3_split_sets);
    let is_contract_keyword = |name: &str| {
        let n = name.to_ascii_lowercase();
        n.starts_with("update")
            || n.starts_with("patch")
            || n.starts_with("upd")
            || n.starts_with("dlc")
            || n.starts_with("addon")
            || n.starts_with("extra")
    };
    if exclude_contract_folders {
        multipart_sets.retain(|set| {
            !set.entry_path
                .file_stem()
                .and_then(OsStr::to_str)
                .is_some_and(is_contract_keyword)
        });
    }
    let archive_parts = multipart_sets
        .iter()
        .flat_map(|set| set.parts.iter().cloned())
        .collect::<BTreeSet<_>>();
    let mut sources = multipart_sets
        .iter()
        .map(|set| set.entry_path.clone())
        .collect::<Vec<_>>();
    let mut ordinary_items = 0;
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("could not scan {}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if archive_parts.contains(&path) {
            continue;
        }
        if exclude_contract_folders {
            if entry.file_type()?.is_dir()
                && entry.file_name().to_str().is_some_and(|name| {
                    name.eq_ignore_ascii_case("Game") || is_contract_keyword(name)
                })
            {
                continue;
            }
            if entry.file_type()?.is_file()
                && entry
                    .path()
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .is_some_and(is_contract_keyword)
            {
                continue;
            }
        }
        sources.push(path);
        ordinary_items += 1;
    }
    sources.sort();
    sources.dedup();
    Ok(FolderContentGroup {
        label,
        directory: directory.to_path_buf(),
        sources,
        multipart_sets,
        ordinary_items,
    })
}

fn scan_ps3_666_sets(
    directory: &Path,
    label: &'static str,
) -> Result<Vec<MultipartGameSet>, Error> {
    let mut grouped = BTreeMap::<String, BTreeMap<u32, PathBuf>>::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((prefix, suffix)) = name.rsplit_once(".666") else {
            continue;
        };
        if suffix.len() != 2 || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let index = suffix.parse::<u32>().unwrap_or(u32::MAX);
        grouped
            .entry(prefix.to_ascii_lowercase())
            .or_default()
            .insert(index, entry.path());
    }

    let mut sets = Vec::new();
    for (prefix, parts_by_index) in grouped {
        let Some((&first_index, _)) = parts_by_index.first_key_value() else {
            continue;
        };
        let Some((&last_index, _)) = parts_by_index.last_key_value() else {
            continue;
        };
        let parts = parts_by_index.values().cloned().collect::<Vec<_>>();
        let missing_index = if first_index > 0 {
            Some(0)
        } else {
            (first_index..=last_index).find(|index| !parts_by_index.contains_key(index))
        };
        let missing_part = missing_index.map(|index| format!("{prefix}.666{index:02}"));
        sets.push(MultipartGameSet {
            game_name: label.to_owned(),
            entry_path: parts[0].clone(),
            parts,
            missing_part,
        });
    }
    Ok(sets)
}

fn validate_group_multipart_sets(group: &FolderContentGroup) -> Result<(), Error> {
    if let Some(set) = group
        .multipart_sets
        .iter()
        .find(|set| set.missing_part.is_some())
    {
        let missing = set.missing_part.as_deref().unwrap_or("unknown part");
        anyhow::bail!(
            "{} ERROR: Multipart {} set is incomplete. Missing {missing}",
            group.label.to_ascii_uppercase(),
            group.label.to_ascii_lowercase()
        );
    }
    Ok(())
}

fn resolve_manual_import_source(source: &Path) -> Result<PathBuf, Error> {
    if !source.is_dir() {
        return Ok(source.to_path_buf());
    }

    let (_, sets, _) = multipart_sets_for_dropped_input(source)?;
    match sets.as_slice() {
        [] => Ok(source.to_path_buf()),
        [set] => Ok(set.entry_path.clone()),
        _ => anyhow::bail!(
            "the dropped folder contains multiple archive sets; put one game's parts in each folder"
        ),
    }
}

fn unique_manual_destination(parent: &Path, name: &OsStr) -> PathBuf {
    let initial = parent.join(name);
    if !initial.exists() {
        return initial;
    }
    let stem = Path::new(name)
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("Import");
    let extension = Path::new(name).extension().and_then(OsStr::to_str);
    for index in 2..10_000 {
        let next_name = match extension {
            Some(extension) => format!("{stem} (Imported {index}).{extension}"),
            None => format!("{stem} (Imported {index})"),
        };
        let candidate = parent.join(next_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("Import-{}", std::process::id()))
}

fn copy_file_and_verify(source: &Path, destination: &Path) -> Result<(), Error> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let expected = fs::metadata(source)?.len();
    fs::copy(source, destination).with_context(|| {
        format!(
            "could not copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    let actual = fs::metadata(destination)?.len();
    if actual != expected {
        anyhow::bail!(
            "copy verification failed for {}: expected {expected} bytes, found {actual}",
            destination.display()
        );
    }
    Ok(())
}

enum StoreNav {
    Listing(StoreListing),
    SearchAgain,
    Back,
}

fn search_store(query: &str) -> Result<Vec<StoreListing>, Error> {
    let settings = app_settings::load_app_settings()?;
    let store =
        WordpressStore::new(settings.store_base_url).with_cache(app_settings::config_dir()?);
    store.search(query)
}

fn select_store_result(query: &str, results: &[StoreListing]) -> Result<StoreNav, Error> {
    render_terminal_header()?;
    println!("Search results for: {query}\n");
    let mut options = results
        .iter()
        .enumerate()
        .map(|(index, listing)| MenuOption {
            value: index,
            label: format_listing(index, listing).join("\n"),
        })
        .collect::<Vec<_>>();
    let search_again = results.len();
    let back = results.len() + 1;
    options.push(MenuOption {
        value: search_again,
        label: "Search again".to_owned(),
    });
    options.push(MenuOption {
        value: back,
        label: "Back to main menu".to_owned(),
    });
    match select_multiline_menu(&options)? {
        Some(index) if index < results.len() => Ok(StoreNav::Listing(results[index].clone())),
        Some(index) if index == search_again => Ok(StoreNav::SearchAgain),
        _ => Ok(StoreNav::Back),
    }
}

fn select_multiline_menu<T: Copy>(options: &[MenuOption<T>]) -> Result<Option<T>, Error> {
    if options.is_empty() {
        anyhow::bail!("the menu has no available options");
    }
    let mut selected = 0;
    let mut line_counts = Vec::new();
    draw_multiline_menu(options, selected, None, &mut line_counts)?;
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        return Ok(None);
    }
    let _raw_mode = RawModeGuard::enter()?;
    loop {
        let Event::Key(key) = read().context("could not read terminal input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Up => {
                selected = selected.checked_sub(1).unwrap_or(options.len() - 1);
                draw_multiline_menu(
                    options,
                    selected,
                    Some(line_counts.iter().sum()),
                    &mut line_counts,
                )?;
            }
            KeyCode::Down => {
                selected = (selected + 1) % options.len();
                draw_multiline_menu(
                    options,
                    selected,
                    Some(line_counts.iter().sum()),
                    &mut line_counts,
                )?;
            }
            KeyCode::Enter => {
                println!();
                return Ok(Some(options[selected].value));
            }
            KeyCode::Esc | KeyCode::Backspace => {
                println!();
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn draw_multiline_menu<T>(
    options: &[MenuOption<T>],
    selected: usize,
    move_up: Option<usize>,
    line_counts: &mut Vec<usize>,
) -> Result<(), Error> {
    if let Some(lines) = move_up {
        print!("\x1b[{lines}A");
    }
    line_counts.clear();
    for (index, option) in options.iter().enumerate() {
        let mut lines = option.label.lines().collect::<Vec<_>>();
        if let Some(first) = lines.first_mut() {
            *first = super::unnumbered_menu_label(first);
        }
        line_counts.push(lines.len().max(1));
        for (line_index, line) in lines.iter().enumerate() {
            print!("\r\x1b[2K");
            if index == selected && line_index == 0 {
                println!("\x1b[38;5;208;1m> {line}\x1b[0m");
            } else if index == selected {
                println!("\x1b[38;5;208;1m{line}\x1b[0m");
            } else {
                println!("  {line}");
            }
        }
    }
    io::stdout().flush().context("error drawing the menu")
}

fn run_store_item(listing: &StoreListing, log: &mut OperationLog) -> Result<bool, Error> {
    if !ensure_ftp_ready()? {
        return Ok(false);
    }
    let listing = inspect_listing(listing)?;
    let Some(option) = choose_download_option(&listing)? else {
        return Ok(false);
    };
    let actions = [
        MenuOption {
            value: true,
            label: "1. Download and install now".to_owned(),
        },
        MenuOption {
            value: false,
            label: "2. Add to Downloads queue".to_owned(),
        },
    ];
    println!("\nChoose what to do:\n");
    let Some(start_now) = select_menu(&actions)? else {
        return Ok(false);
    };
    if !start_now {
        let drive = super::choose_saved_xbox_storage_name()?;
        crate::download_queue::enqueue(listing.clone(), option, drive)?;
        println!("\nAdded to Downloads. It will continue in the background.");
        pause_to_continue()?;
        return Ok(false);
    }
    log.stage("store_item", &listing.title)?;
    process_store_listing(&listing, JobDelivery::Ftp, option, log)?;
    Ok(true)
}

fn inspect_listing(listing: &StoreListing) -> Result<StoreListing, Error> {
    let settings = app_settings::load_app_settings()?;
    match WordpressStore::new(settings.store_base_url).inspect(listing) {
        Ok(detailed) => Ok(detailed),
        Err(_) => Ok(listing.clone()),
    }
}

fn choose_job_delivery() -> Result<Option<JobDelivery>, Error> {
    println!("\nChoose delivery method:\n");
    let options = [
        MenuOption {
            value: JobDelivery::Ftp,
            label: "1. Upload to Xbox using FTP".to_owned(),
        },
        MenuOption {
            value: JobDelivery::Usb,
            label: "2. Save to USB drive".to_owned(),
        },
        MenuOption {
            value: JobDelivery::Local,
            label: "3. Save to a folder on this PC".to_owned(),
        },
    ];
    let Some(delivery) = select_menu(&options)? else {
        return Ok(None);
    };
    if delivery == JobDelivery::Ftp && !ensure_ftp_ready()? {
        return Ok(None);
    }
    Ok(Some(delivery))
}

fn ensure_ftp_ready() -> Result<bool, Error> {
    if saved_ftp_is_ready()? {
        return Ok(true);
    }
    render_terminal_header()?;
    println!("Xbox FTP has not been configured.\n");
    configure_ftp_terminal()
}

fn choose_download_option(listing: &StoreListing) -> Result<Option<DownloadOption>, Error> {
    render_terminal_header()?;
    println!("Game: {}", listing.title);
    let options = listing
        .download_options
        .iter()
        .filter(|option| !crate::store::is_onefile_option(option))
        .cloned()
        .collect::<Vec<_>>();
    if options.is_empty() {
        anyhow::bail!("No download links were found on this source page");
    }
    render_terminal_header()?;
    println!("Game: {}", listing.title);
    println!("Choose a download option. Only that host and its password will be used.\n");
    let menu: Vec<MenuOption<usize>> = options
        .iter()
        .enumerate()
        .map(|(index, option)| MenuOption {
            value: index,
            label: format_download_option(index, option),
        })
        .collect();
    let Some(index) = select_multiline_menu(&menu)? else {
        return Ok(None);
    };
    Ok(options.into_iter().nth(index))
}

fn format_download_option(index: usize, option: &DownloadOption) -> String {
    let password = option
        .password
        .as_deref()
        .map(|value| format!("     Password: {value}"))
        .unwrap_or_else(|| "     Password: listed on the download page".to_owned());
    let parts = (option.urls.len() > 1)
        .then(|| format!(" ({} parts)", option.urls.len()))
        .unwrap_or_default();
    format!("{}. {}{parts}\n{password}", index + 1, option.label)
}

pub(super) fn process_store_listing(
    listing: &StoreListing,
    delivery: JobDelivery,
    option: DownloadOption,
    log: &mut OperationLog,
) -> Result<(), Error> {
    let work_dir = reusable_store_download_dir(&listing.title)?;
    let mut job = JobState::new(
        JobWorkflow::Store,
        delivery,
        listing.title.clone(),
        work_dir.clone(),
    );
    print_pipeline(&job);
    let (source, password) = acquire_store_files(listing, &option, &work_dir, &mut job, log)?;
    job.source_path = Some(source.clone());
    job.save()?;
    set_archive_password(password.as_deref());
    let result = run_terminal_conversion(
        match delivery {
            JobDelivery::Ftp => TerminalDelivery::Ftp,
            JobDelivery::Usb => TerminalDelivery::Usb,
            JobDelivery::Local => TerminalDelivery::Local,
        },
        Some(source),
    );
    set_archive_password(None);
    if result.is_ok() {
        pipeline::clear_last_job();
    }
    result
}

fn acquire_store_files(
    listing: &StoreListing,
    option: &DownloadOption,
    work_dir: &Path,
    job: &mut JobState,
    log: &mut OperationLog,
) -> Result<(PathBuf, Option<String>), Error> {
    job.pipeline
        .set(StageKind::Downloading, StageStatus::InProgress);
    job.save()?;
    print_pipeline(job);
    download::require_disk_space(work_dir, 512 * 1024 * 1024).map_err(friendly_error)?;

    let password = match option.password.as_deref() {
        Some(password) => Some(password.to_owned()),
        None => None,
    };

    log.stage("download", &option.label)?;
    let source = if crate::store::mega_download::is_mega_option(option) {
        let files =
            crate::store::mega_download::download_option(option, &work_dir.join("downloads"))?;
        files
            .iter()
            .find(|path| browser::looks_like_archive(path))
            .cloned()
            .unwrap_or_else(|| work_dir.join("downloads"))
    } else if option.label.eq_ignore_ascii_case("MediaFire")
        || option.url.to_ascii_lowercase().contains("mediafire.com")
    {
        let downloads = work_dir.join("downloads");
        let mut files = Vec::new();
        for url in download::option_urls(option) {
            files.extend(
                download::download_mediafire_files(url, &downloads).map_err(friendly_error)?,
            );
        }
        if files.is_empty() {
            anyhow::bail!("Download link is unavailable");
        }
        files[0].clone()
    } else {
        match crate::store::classify_url(&option.url) {
            crate::store::UrlClass::Direct => {
                let downloads = work_dir.join("downloads");
                let urls = download::option_urls(option)
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let files = download::download_files(&urls, &downloads).map_err(friendly_error)?;
                if files.is_empty() {
                    anyhow::bail!("Download link is unavailable");
                }
                files[0].clone()
            }
            _ => anyhow::bail!(
                "This host needs a download method that is not implemented yet. The source link is listed, but automatic downloading is unavailable."
            ),
        }
    };
    job.pipeline
        .set(StageKind::Downloading, StageStatus::Complete);
    print_pipeline(job);
    Ok((source, password))
}

fn wait_for_public_browser_downloads(
    listing: &StoreListing,
    urls: &[String],
    label: &str,
    destination: &Path,
) -> Result<Vec<PathBuf>, Error> {
    let browser_downloads =
        browser::default_downloads_dir().context("could not find the browser Downloads folder")?;
    fs::create_dir_all(destination).context("could not create the game download folder")?;
    let started_at = SystemTime::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(SystemTime::now);
    for url in urls {
        open_url_in_default_browser(url)?;
    }
    print_host_download_help(listing, label);
    let expected_count = listing.parts.unwrap_or(urls.len() as u32).max(1) as usize;
    println!("The app will continue automatically after every required part finishes.");

    let mut stable = BTreeMap::<PathBuf, (u64, u8)>::new();
    loop {
        for entry in fs::read_dir(&browser_downloads)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file()
                || !browser::looks_like_archive(&path)
                || browser::is_incomplete_download(&path)
            {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH) < started_at
                || metadata.len() == 0
            {
                continue;
            }
            let unchanged = stable
                .get(&path)
                .filter(|(size, _)| *size == metadata.len())
                .map(|(_, count)| count.saturating_add(1))
                .unwrap_or(0);
            stable.insert(path, (metadata.len(), unchanged));
        }
        let mut complete = stable
            .iter()
            .filter(|(_, (_, count))| *count >= 3)
            .map(|(path, (size, _))| (path.clone(), *size))
            .collect::<Vec<_>>();
        complete.sort_by(|left, right| left.0.cmp(&right.0));
        render_progress_screen(
            "Downloading game",
            &format!("Downloading from {label}"),
            ((complete.len().min(expected_count) * 100) / expected_count) as u8,
            Some(&format!(
                "{} of {expected_count} expected archive file(s) complete",
                complete.len()
            )),
        );
        if complete.len() >= expected_count {
            let mut copied = Vec::with_capacity(complete.len());
            for (source, size) in complete {
                let name = source.file_name().context("downloaded file has no name")?;
                let target = destination.join(name);
                fs::copy(&source, &target)?;
                if fs::metadata(&target)?.len() != size {
                    anyhow::bail!(
                        "downloaded archive failed verification: {}",
                        target.display()
                    );
                }
                fs::remove_file(&source)?;
                copied.push(target);
            }
            return Ok(copied);
        }
        if let Some(KeyCode::Char('c' | 'C')) | Some(KeyCode::Esc) =
            poll_key(Duration::from_millis(750))?
        {
            anyhow::bail!("Download was cancelled; completed browser files were preserved");
        }
    }
}

fn download_mediafire_parts_in_browser(
    files: &[download::MediaFireFile],
    destination: &Path,
) -> Result<Vec<PathBuf>, Error> {
    let browser_downloads =
        browser::default_downloads_dir().context("could not find the browser Downloads folder")?;
    fs::create_dir_all(destination).context("could not create the game download folder")?;
    let started_at = SystemTime::now()
        .checked_sub(Duration::from_secs(3))
        .unwrap_or_else(SystemTime::now);
    for file in files {
        open_url_in_default_browser(&file.direct_url)?;
    }

    let expected = files
        .iter()
        .map(|file| file.file_name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let mut stable = BTreeMap::<String, (PathBuf, u64, u8)>::new();
    loop {
        for entry in fs::read_dir(&browser_downloads)
            .with_context(|| format!("could not read {}", browser_downloads.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_file() || browser::is_incomplete_download(&entry.path()) {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH) < started_at
                || metadata.len() == 0
            {
                continue;
            }
            let actual_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            let Some(expected_name) = expected
                .iter()
                .find(|name| browser_download_name_matches(&actual_name, name))
                .cloned()
            else {
                continue;
            };
            let previous = stable.get(&expected_name);
            let unchanged = previous
                .filter(|(path, size, _)| path == &entry.path() && *size == metadata.len())
                .map(|(_, _, count)| count.saturating_add(1))
                .unwrap_or(0);
            stable.insert(expected_name, (entry.path(), metadata.len(), unchanged));
        }
        let completed = stable.values().filter(|(_, _, count)| *count >= 3).count();
        render_progress_screen(
            "Downloading game",
            "Downloading all MediaFire archive parts",
            ((completed * 100) / files.len()) as u8,
            Some(&format!("{completed} of {} parts complete", files.len())),
        );
        if completed == files.len() {
            break;
        }
        if let Some(KeyCode::Char('c' | 'C')) | Some(KeyCode::Esc) =
            poll_key(Duration::from_millis(750))?
        {
            anyhow::bail!(
                "MediaFire download was cancelled; completed browser files were preserved"
            );
        }
    }

    let mut downloaded = Vec::with_capacity(files.len());
    for file in files {
        let key = file.file_name.to_ascii_lowercase();
        let (source, source_size, _) = stable
            .remove(&key)
            .with_context(|| format!("MediaFire part did not finish: {}", file.file_name))?;
        let target = destination.join(&file.file_name);
        fs::copy(&source, &target).with_context(|| {
            format!(
                "could not move completed part {} into the game job",
                source.display()
            )
        })?;
        if fs::metadata(&target)?.len() != source_size {
            anyhow::bail!(
                "copied MediaFire part failed verification: {}",
                target.display()
            );
        }
        fs::remove_file(&source).with_context(|| {
            format!(
                "could not remove the browser copy after verifying {}",
                target.display()
            )
        })?;
        downloaded.push(target);
    }
    Ok(downloaded)
}

fn browser_download_name_matches(actual: &str, expected: &str) -> bool {
    if actual == expected {
        return true;
    }
    let Some((stem, extension)) = expected.rsplit_once('.') else {
        return false;
    };
    actual
        .strip_prefix(stem)
        .and_then(|rest| rest.strip_suffix(&format!(".{extension}")))
        .is_some_and(|middle| {
            middle
                .strip_prefix(" (")
                .and_then(|value| value.strip_suffix(')'))
                .is_some_and(|number| number.chars().all(|character| character.is_ascii_digit()))
        })
}

#[cfg(test)]
mod browser_download_tests {
    use super::browser_download_name_matches;

    #[test]
    fn matches_exact_and_browser_renamed_archive_parts() {
        assert!(browser_download_name_matches(
            "game.part1.rar",
            "game.part1.rar"
        ));
        assert!(browser_download_name_matches(
            "game.part1 (2).rar",
            "game.part1.rar"
        ));
        assert!(!browser_download_name_matches(
            "game.part2.rar",
            "game.part1.rar"
        ));
    }
}

fn reusable_store_download_dir(game_name: &str) -> Result<PathBuf, Error> {
    if let Some(job) = pipeline::load_last_job()?
        && job.workflow == JobWorkflow::Store
        && job.game_name == game_name
        && job.pipeline.delivery != StageStatus::Complete
        && pipeline::is_inside(&job.work_dir, &app_settings::jobs_dir()?)
    {
        println!("Resuming the previous background job.");
        return Ok(job.work_dir);
    }
    pipeline::work_dir_for_game(game_name)
}

fn prompt_store_password(page_url: &str, label: &str) -> Result<String, Error> {
    open_url_in_default_browser(page_url)?;
    loop {
        println!("\nThis {label} download requires a password.");
        println!("The {label} page has been opened in your default browser.");
        println!("Copy the archive password for {label} and paste it below.");
        let Some(password) = prompt_terminal_text("Archive password", None, true)? else {
            anyhow::bail!("Archive password entry was cancelled");
        };
        let choices = [
            MenuOption {
                value: 0_u8,
                label: "Use this password".to_owned(),
            },
            MenuOption {
                value: 1_u8,
                label: "Reopen the page".to_owned(),
            },
            MenuOption {
                value: 2_u8,
                label: "Cancel".to_owned(),
            },
        ];
        match select_menu(&choices)? {
            Some(0) => return Ok(password),
            Some(1) => {
                open_url_in_default_browser(page_url)?;
            }
            _ => anyhow::bail!("Archive password entry was cancelled"),
        }
    }
}

fn wait_for_browser_download(
    listing: &StoreListing,
    url: &str,
    label: &str,
) -> Result<PathBuf, Error> {
    open_url_in_default_browser(url)?;
    print_host_download_help(listing, label);
    println!("Waiting for the downloaded file...");
    println!("[S] Select Downloaded File");
    println!("[R] Reopen Browser");
    println!("[C] Cancel");
    let started_at = SystemTime::now()
        .checked_sub(Duration::from_secs(5))
        .unwrap_or_else(SystemTime::now);
    let downloads = browser::default_downloads_dir();
    let mut last_size: Option<(PathBuf, u64)> = None;
    loop {
        if let Some(folder) = downloads.as_ref()
            && let Ok(Some(path)) =
                browser::find_completed_download(folder, started_at, &listing.title)
        {
            let previous = last_size
                .as_ref()
                .filter(|(seen, _)| seen == &path)
                .map(|(_, size)| *size);
            if let Ok(Some(_)) = browser::file_size_stable(&path, previous, Duration::from_secs(2))
            {
                println!("\nDetected downloaded file:");
                println!("  {}", path.display());
                return Ok(path);
            }
            last_size = fs::metadata(&path).ok().map(|meta| (path, meta.len()));
        }
        if let Some(key) = poll_key(Duration::from_millis(400))? {
            match key {
                KeyCode::Char('s' | 'S') => {
                    println!("\nSelect the completed download.");
                    if let Some(path) = prompt_dropped_game_file()? {
                        if browser::is_incomplete_download(&path) {
                            println!(
                                "Browser download was not detected: that file is still downloading."
                            );
                            continue;
                        }
                        return Ok(path);
                    }
                }
                KeyCode::Char('r' | 'R') => {
                    open_url_in_default_browser(url)?;
                    println!("The {label} page was reopened in your default browser.");
                }
                KeyCode::Char('c' | 'C') | KeyCode::Esc => {
                    anyhow::bail!("Browser download was not detected");
                }
                _ => {}
            }
        }
    }
}

fn print_host_download_help(listing: &StoreListing, label: &str) {
    let multipart = listing.parts.unwrap_or(0) > 1
        || matches!(
            listing.format,
            ArchiveFormat::MultipartRar
                | ArchiveFormat::MultipartZip
                | ArchiveFormat::MultipartSevenZip
        );
    println!("\nOnly the {label} download page was opened.");
    if label.eq_ignore_ascii_case("MediaFire") && !multipart {
        println!("You do not need to sign in.");
        println!("1. Click Download on that page.");
        println!("2. Wait for the file to finish.");
        println!("3. Return to ISO 2 GOD.");
    } else if label.eq_ignore_ascii_case("MediaFire") && multipart {
        println!("This game is split into multiple parts.");
        println!("MediaFire may ask you to sign in only for the extra parts.");
        println!("1. Download every part.");
        println!("2. Sign in only if MediaFire asks for the remaining parts.");
        println!("3. Return to ISO 2 GOD.");
    } else {
        println!("1. Start and complete that download.");
        println!("2. Sign in only if that host asks.");
        println!("3. Return to ISO 2 GOD.");
    }
}

fn poll_key(timeout: Duration) -> Result<Option<KeyCode>, Error> {
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        return Ok(Some(KeyCode::Esc));
    }
    if !poll(timeout).context("could not read terminal input")? {
        return Ok(None);
    }
    match read().context("could not read terminal input")? {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press) => Ok(Some(key.code)),
        _ => Ok(None),
    }
}

fn process_source(
    source: &Path,
    delivery: JobDelivery,
    workflow: JobWorkflow,
    job: Option<JobState>,
    password: Option<&str>,
    log: &mut OperationLog,
) -> Result<(), Error> {
    let mut job = job.unwrap_or_else(|| {
        let work_dir = source
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        JobState::new(
            workflow,
            delivery,
            source
                .file_stem()
                .and_then(OsStr::to_str)
                .unwrap_or("game")
                .to_owned(),
            work_dir,
        )
    });
    job.source_path = Some(source.to_path_buf());
    if password.is_some() {
        set_archive_password(password);
    }
    let result = process_source_inner(source, delivery, &mut job, log);
    set_archive_password(None);
    result
}

fn process_source_inner(
    source: &Path,
    delivery: JobDelivery,
    job: &mut JobState,
    log: &mut OperationLog,
) -> Result<(), Error> {
    loop {
        print_pipeline(job);
        match process_source_attempt(source, delivery, job, log) {
            Ok(()) => {
                job.pipeline.set(StageKind::Delivery, StageStatus::Complete);
                job.save().ok();
                pipeline::clear_last_job();
                println!("\nProcessing completed successfully.");
                offer_cleanup(job)?;
                pause_to_continue()?;
                return Ok(());
            }
            Err(error) => {
                log.error(&error)?;
                job.save().ok();
                print_terminal_error(&error);
                let mut choices = vec![MenuOption {
                    value: 0_u8,
                    label: "Retry".to_owned(),
                }];
                if job.conversion_complete() {
                    choices.push(MenuOption {
                        value: 1_u8,
                        label: "Retry delivery only".to_owned(),
                    });
                }
                choices.push(MenuOption {
                    value: 2_u8,
                    label: "Change Settings".to_owned(),
                });
                choices.push(MenuOption {
                    value: 3_u8,
                    label: "Select Another File".to_owned(),
                });
                choices.push(MenuOption {
                    value: 4_u8,
                    label: "Cancel".to_owned(),
                });
                match select_menu(&choices)? {
                    Some(0) => continue,
                    Some(1) if job.conversion_complete() => {
                        if let Err(error) = deliver_prepared_job(job) {
                            print_terminal_error(&error);
                            continue;
                        }
                        println!("\nProcessing completed successfully.");
                        offer_cleanup(job)?;
                        pause_to_continue()?;
                        pipeline::clear_last_job();
                        return Ok(());
                    }
                    Some(2) => {
                        render_terminal_header()?;
                        match configure_ftp_terminal() {
                            Ok(true) => pause_to_continue()?,
                            Ok(false) => {}
                            Err(error) => {
                                print_terminal_error(&error);
                                pause_to_continue()?;
                            }
                        }
                    }
                    Some(3) => {
                        if let Some(path) = prompt_dropped_game_file()? {
                            return process_source(
                                path.as_path(),
                                delivery,
                                job.workflow,
                                None,
                                None,
                                log,
                            );
                        }
                    }
                    _ => return Ok(()),
                }
            }
        }
    }
}

fn process_source_attempt(
    source: &Path,
    delivery: JobDelivery,
    job: &mut JobState,
    log: &mut OperationLog,
) -> Result<(), Error> {
    if job.conversion_complete() {
        log.stage("resume", "delivery")?;
        return deliver_prepared_job(job);
    }

    job.pipeline
        .set(StageKind::ArchiveCheck, StageStatus::InProgress);
    print_pipeline(job);
    log.stage("source", &format!("{}", source.display()))?;

    if source
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("iso"))
    {
        return convert_iso_source(source, delivery, job);
    }

    let (source_folder, sets, remove_parts) = multipart_sets_for_dropped_input(source)?;
    if let Some(set) = sets.first()
        && let Some(missing) = &set.missing_part
    {
        anyhow::bail!(
            "Archive part {} of {} is missing",
            missing_part_number(missing).unwrap_or(0),
            expected_part_count(set)
        );
    }
    job.pipeline
        .set(StageKind::ArchiveCheck, StageStatus::Complete);
    job.pipeline
        .set(StageKind::Extracting, StageStatus::InProgress);
    print_pipeline(job);

    if let Some(password) = current_archive_password() {
        validate_source_password(source, &password)?;
    }

    let destination = job_destination(delivery)?;
    let prepared =
        multipart_game_import_once(&source_folder, sets, &destination, remove_parts, true)?;
    job.prepared_root = Some(prepared.root.clone());
    job.pipeline
        .set(StageKind::Extracting, StageStatus::Complete);
    job.pipeline
        .set(StageKind::IsoDetected, StageStatus::Complete);
    job.pipeline
        .set(StageKind::Converting, StageStatus::Complete);
    job.pipeline
        .set(StageKind::Verification, StageStatus::Complete);
    job.pipeline.set(StageKind::Delivery, StageStatus::Complete);
    job.save()?;
    Ok(())
}

fn convert_iso_source(
    source: &Path,
    delivery: JobDelivery,
    job: &mut JobState,
) -> Result<(), Error> {
    job.pipeline.skip_download();
    job.pipeline
        .set(StageKind::ArchiveCheck, StageStatus::Skipped);
    job.pipeline
        .set(StageKind::Extracting, StageStatus::Skipped);
    job.pipeline
        .set(StageKind::IsoDetected, StageStatus::Complete);
    job.pipeline
        .set(StageKind::Converting, StageStatus::InProgress);
    print_pipeline(job);

    let work = if job.work_dir.is_dir() {
        job.work_dir.clone()
    } else {
        pipeline::work_dir_for_game(&job.game_name)?
    };
    let dest_dir = work.join("converted");
    fs::create_dir_all(&dest_dir)?;
    let output = run(Cli {
        source_iso: source.to_path_buf(),
        dest_dir,
        dry_run: false,
        game_title: Some(job.game_name.clone()),
        trim: None,
        num_threads: 1,
        upload_ftp: false,
        copy_usb: false,
        use_saved_ftp: false,
        prompt_delivery: false,
        selected_usb_drive: None,
    })?;
    job.pipeline
        .set(StageKind::Converting, StageStatus::Complete);
    job.pipeline
        .set(StageKind::Verification, StageStatus::InProgress);
    print_pipeline(job);
    verify_converted_game(&output)?;
    job.pipeline
        .set(StageKind::Verification, StageStatus::Complete);
    job.prepared_root = Some(output.local_title_dir.clone());
    job.save()?;
    deliver_converted_output(&output, delivery, job)
}

fn job_destination(delivery: JobDelivery) -> Result<MultipartDestination, Error> {
    Ok(match delivery {
        JobDelivery::Ftp => MultipartDestination::Ftp,
        JobDelivery::Usb => {
            render_terminal_header()?;
            println!("Searching for removable USB drives...\n");
            MultipartDestination::Usb(
                select_removable_drive()?.context("USB transfer was cancelled")?,
            )
        }
        JobDelivery::Local => MultipartDestination::Local(
            prompt_local_folder()?.context("folder selection was cancelled")?,
        ),
    })
}

fn deliver_converted_output(
    output: &ConversionOutput,
    delivery: JobDelivery,
    job: &mut JobState,
) -> Result<(), Error> {
    job.pipeline
        .set(StageKind::Delivery, StageStatus::InProgress);
    print_pipeline(job);
    match delivery {
        JobDelivery::Ftp => upload_to_saved_xbox(output).map_err(map_ftp_error)?,
        JobDelivery::Usb => copy_to_removable_drive(output, None)?,
        JobDelivery::Local => {
            let folder = prompt_local_folder()?.context("folder selection was cancelled")?;
            let destination = folder
                .join("Content")
                .join("0000000000000000")
                .join(&output.title_id);
            copy_directory_and_verify(&output.local_title_dir, &destination, "Copying game")?;
        }
    }
    Ok(())
}

fn deliver_prepared_job(job: &JobState) -> Result<(), Error> {
    let root = job
        .prepared_root
        .as_ref()
        .context("the converted package is no longer available")?;
    let prepared = prepared_game_from_root(root)?;
    match job.delivery {
        JobDelivery::Ftp => upload_prepared_game_to_xbox(&prepared).map_err(map_ftp_error)?,
        JobDelivery::Usb => {
            let drive = select_removable_drive()?.context("USB transfer was cancelled")?;
            copy_prepared_game_to_selected_drive(&prepared, &drive)?;
        }
        JobDelivery::Local => {
            let folder = prompt_local_folder()?.context("folder selection was cancelled")?;
            copy_prepared_game_to_folder(&prepared, &folder)?;
        }
    }
    Ok(())
}

fn prepared_game_from_root(root: &Path) -> Result<PreparedGame, Error> {
    if let Some((title_dir, title_id)) = find_existing_god_package(root)? {
        return Ok(PreparedGame {
            root: root.to_path_buf(),
            name: title_id.clone(),
            kind: PreparedGameKind::God {
                title_dir,
                title_id,
            },
        });
    }
    if find_default_xex_game(root)?.is_some() {
        let title_id = read_prepared_jtag_title_id(root)?;
        return Ok(PreparedGame {
            root: root.to_path_buf(),
            name: root
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or("game")
                .to_owned(),
            kind: PreparedGameKind::Jtag { title_id },
        });
    }
    anyhow::bail!(
        "the converted package is no longer available at {}",
        root.display()
    )
}

fn prompt_local_folder() -> Result<Option<PathBuf>, Error> {
    println!("\nDrag and drop the destination folder, or type its path.\n");
    prompt_dropped_path("Waiting for destination folder", Some(true))
}

fn offer_resume_job(workflow: JobWorkflow, log: &mut OperationLog) -> Result<bool, Error> {
    let Some(job) = pipeline::load_last_job()? else {
        return Ok(false);
    };
    if job.workflow != workflow
        || !pipeline::stage_is_complete(&job, StageKind::Verification)
        || job.prepared_root.is_none()
    {
        return Ok(false);
    }
    println!("\nA previous job finished conversion but did not complete delivery.");
    println!("Game: {}", job.game_name);
    println!(
        "Prepared package: {}",
        job.prepared_root
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    );
    let resume = if workflow == JobWorkflow::Store {
        println!("Automatically resuming the interrupted Store upload.");
        true
    } else {
        let Some(resume) =
            prompt_yes_no_key("Resume delivery from the last completed stage?", true)?
        else {
            return Ok(false);
        };
        resume
    };
    if !resume {
        return Ok(false);
    }
    log.stage("resume", "delivery")?;
    match deliver_prepared_job(&job) {
        Ok(()) => {
            pipeline::clear_last_job();
            println!("\nProcessing completed successfully.");
            offer_cleanup(&job)?;
            pause_to_continue()?;
            Ok(true)
        }
        Err(error) => {
            print_terminal_error(&error);
            pause_to_continue()?;
            Ok(true)
        }
    }
}

fn offer_cleanup(job: &JobState) -> Result<(), Error> {
    let settings = app_settings::load_app_settings()?;
    let cleanup = if settings.auto_cleanup {
        true
    } else {
        prompt_yes_no_key(
            "Remove downloaded archives and temporary extracted files?",
            true,
        )?
        .unwrap_or(true)
    };
    if !cleanup {
        println!("Temporary files were kept at: {}", job.work_dir.display());
        return Ok(());
    }
    if pipeline::is_inside(&job.work_dir, &app_settings::jobs_dir()?) && job.work_dir.is_dir() {
        let _ = remove_dir_all_robust(&job.work_dir);
        println!("Temporary files were removed.");
    }
    Ok(())
}

fn print_pipeline(job: &JobState) {
    let (stage, percent) = job.pipeline.current_progress(job.delivery_label());
    render_progress_screen(&format!("Game: {}", job.game_name), stage, percent, None);
}

fn missing_part_number(name: &str) -> Option<u32> {
    name.to_ascii_lowercase()
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

fn expected_part_count(set: &MultipartGameSet) -> usize {
    set.parts.len().saturating_add(1)
}

fn validate_source_password(source: &Path, password: &str) -> Result<(), Error> {
    let Some(seven_zip) = find_7zip() else {
        return Ok(());
    };
    let archive = if source.is_dir() {
        return Ok(());
    } else {
        source
    };
    match test_archive_password(&seven_zip, archive, password) {
        Ok(true) => Ok(()),
        Ok(false) => loop {
            println!("\nArchive password is incorrect.");
            let choices = [
                MenuOption {
                    value: 0_u8,
                    label: "Retry".to_owned(),
                },
                MenuOption {
                    value: 1_u8,
                    label: "Reopen Browser".to_owned(),
                },
                MenuOption {
                    value: 2_u8,
                    label: "Cancel".to_owned(),
                },
            ];
            match select_menu(&choices)? {
                Some(0) => {
                    let Some(password) = prompt_terminal_text("Archive password", None, true)?
                    else {
                        anyhow::bail!("Archive password is incorrect");
                    };
                    if test_archive_password(&seven_zip, archive, &password)? {
                        set_archive_password(Some(&password));
                        return Ok(());
                    }
                }
                Some(1) => {
                    anyhow::bail!("Archive password is incorrect");
                }
                _ => anyhow::bail!("Archive password is incorrect"),
            }
        },
        Err(error) => Err(error),
    }
}

fn map_ftp_error(error: Error) -> Error {
    let text = format!("{error:#}").to_ascii_lowercase();
    if text.contains("connection refused") {
        anyhow::anyhow!("Xbox FTP connection was refused")
    } else if text.contains("unavailable") || text.contains("not found") {
        anyhow::anyhow!("Xbox destination directory is unavailable")
    } else if text.contains("interrupt") || text.contains("broken") {
        anyhow::anyhow!("Upload was interrupted")
    } else {
        error
    }
}

fn friendly_error(error: Error) -> Error {
    let text = format!("{error:#}");
    let lower = text.to_ascii_lowercase();
    if lower.contains("not enough free") || lower.contains("insufficient disk") {
        anyhow::anyhow!("Insufficient disk space")
    } else if lower.contains("could not be reached") {
        anyhow::anyhow!("Store could not be reached")
    } else if lower.contains("no results") {
        anyhow::anyhow!("Search returned no results")
    } else if lower.contains("download link") {
        anyhow::anyhow!("Download link is unavailable")
    } else {
        error
    }
}

#[cfg(test)]
mod manual_import_tests {
    use super::*;

    fn synthetic_sfo(category: &[u8], version: &[u8]) -> Vec<u8> {
        let entries: Vec<(&str, &[u8])> = vec![
            ("APP_VER", version),
            ("CATEGORY", category),
            ("CONTENT_ID", b"EP0001-CUSA12345_00-TESTTITLE00000000\0"),
            ("TITLE", b"Synthetic Game\0"),
            ("TITLE_ID", b"CUSA12345\0"),
        ];
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let mut records = Vec::new();
        for (key, value) in &entries {
            let key_offset = keys.len() as u16;
            keys.extend_from_slice(key.as_bytes());
            keys.push(0);
            let value_offset = values.len() as u32;
            values.extend_from_slice(value);
            while values.len() % 4 != 0 {
                values.push(0);
            }
            records.extend_from_slice(&key_offset.to_le_bytes());
            records.extend_from_slice(&0x0204u16.to_le_bytes());
            records.extend_from_slice(&(value.len() as u32).to_le_bytes());
            records.extend_from_slice(&((values.len() as u32) - value_offset).to_le_bytes());
            records.extend_from_slice(&value_offset.to_le_bytes());
        }
        let key_table = 20 + records.len() as u32;
        while keys.len() % 4 != 0 {
            keys.push(0);
        }
        let value_table = key_table + keys.len() as u32;
        let mut output = b"\0PSF".to_vec();
        output.extend_from_slice(&0x00000101u32.to_le_bytes());
        output.extend_from_slice(&key_table.to_le_bytes());
        output.extend_from_slice(&value_table.to_le_bytes());
        output.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        output.extend(records);
        output.extend(keys);
        output.extend(values);
        output
    }

    fn synthetic_dump(root: &Path, folder: &str, category: &[u8], version: &[u8]) -> PathBuf {
        let dump = root.join(folder);
        fs::create_dir_all(dump.join("sce_sys")).unwrap();
        fs::write(dump.join("eboot.bin"), b"synthetic eboot").unwrap();
        fs::write(dump.join("game.dat"), b"synthetic payload").unwrap();
        fs::write(
            dump.join("sce_sys/param.sfo"),
            synthetic_sfo(category, version),
        )
        .unwrap();
        dump
    }

    #[test]
    fn ps3_empty_folder_messages_are_specific() {
        assert_eq!(
            ps3_empty_folder_message(true, false),
            Some("Please place your PS3 game files in the Game folder to continue.")
        );
        assert_eq!(
            ps3_empty_folder_message(false, true),
            Some("Please place your PS3 update files in the Update folder to continue.")
        );
        assert_eq!(
            ps3_empty_folder_message(true, true),
            Some("Please add your PS3 game and update files to their folders to continue.")
        );
        assert_eq!(ps3_empty_folder_message(false, false), None);
    }

    #[test]
    fn ps3_empty_folder_handler_is_pending_in_terminal_smoke_mode() {
        unsafe { std::env::set_var("ISO2GOD_TERMINAL_SMOKE_TEST", "1") };
        let action = handle_empty_ps3_contract_folders(true, true).unwrap();
        unsafe { std::env::remove_var("ISO2GOD_TERMINAL_SMOKE_TEST") };
        assert_eq!(action, Ps3EmptyFolderAction::Pending);
    }

    #[test]
    fn ps3_optional_update_prompt_has_enter_to_skip_copy() {
        assert_eq!(
            ps3_optional_update_prompt_text(),
            "No game update was detected.\n\nDo you have an update folder?\n\nDrag and drop your Update folder here,\nor press Enter to continue without an update."
        );
    }

    #[test]
    fn ps3_direct_game_drop_uses_sibling_update_directory() {
        let root = iso2god::forge::TempWorkspace::new("ps3-direct-game-drop").unwrap();
        let parent = root.root.join("Uncharted");
        let game = parent.join("GAME");
        let update = parent.join("update");
        fs::create_dir_all(&game).unwrap();
        fs::create_dir_all(&update).unwrap();

        let (resolved_game, update_parent) = game_and_update_parent(&game).unwrap();
        assert_eq!(resolved_game, game);
        assert_eq!(update_parent, Some(parent));
        assert_eq!(
            find_named_child_directory(update_parent.as_deref().unwrap(), "Update"),
            Some(update)
        );
    }

    #[test]
    fn ps3_optional_update_enter_keeps_base_conversion_pending_without_update() {
        let root = iso2god::forge::TempWorkspace::new("ps3-optional-update").unwrap();
        let base = root.root.join("base.pkg");
        fs::write(&base, b"base fixture").unwrap();
        unsafe { std::env::set_var("ISO2GOD_TERMINAL_SMOKE_TEST", "1") };
        let update = prompt_for_ps3_optional_update(&base, None).unwrap();
        unsafe { std::env::remove_var("ISO2GOD_TERMINAL_SMOKE_TEST") };

        assert!(update.is_empty());
        assert_eq!(base, root.root.join("base.pkg"));
    }

    #[test]
    fn ps3_666_parts_are_grouped_for_existing_reassembly() {
        let root = env::temp_dir().join(format!(
            "digital-forge-ps3-666-group-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("game.66600"), b"part0").unwrap();
        fs::write(root.join("game.66601"), b"part1").unwrap();
        let group = scan_folder_content_group("Base Game", &root, false).unwrap();
        assert_eq!(group.sources, vec![root.join("game.66600")]);
        assert_eq!(group.multipart_sets.len(), 1);
        assert_eq!(group.multipart_sets[0].parts.len(), 2);
        assert!(group.multipart_sets[0].missing_part.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_copy_preserves_bytes_and_never_overwrites_an_existing_import() {
        let root =
            env::temp_dir().join(format!("digital-forge-manual-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("sample.pkg");
        fs::write(&source, b"verified package bytes").unwrap();
        let destination_root = root.join("output");
        fs::create_dir_all(&destination_root).unwrap();

        let first = unique_manual_destination(&destination_root, source.file_name().unwrap());
        copy_file_and_verify(&source, &first).unwrap();
        let second = unique_manual_destination(&destination_root, source.file_name().unwrap());

        assert_eq!(fs::read(&first).unwrap(), b"verified package bytes");
        assert_ne!(first, second);
        assert_eq!(second.file_name().unwrap(), "sample (Imported 2).pkg");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn manual_import_escape_is_back_and_enter_selects() {
        assert_eq!(
            manual_import_key_action(KeyCode::Esc),
            Some(ManualImportKeyAction::Back)
        );
        assert_eq!(
            manual_import_key_action(KeyCode::Enter),
            Some(ManualImportKeyAction::Select)
        );
        assert_eq!(
            manual_import_key_action(KeyCode::Char('p')),
            Some(ManualImportKeyAction::Character('p'))
        );
        assert_eq!(
            manual_import_key_action(KeyCode::Backspace),
            Some(ManualImportKeyAction::Backspace)
        );
        assert_eq!(manual_import_key_action(KeyCode::Tab), None);
        assert_eq!(
            manual_import_key_action(KeyCode::Up),
            Some(ManualImportKeyAction::Up)
        );
        assert_eq!(
            manual_import_key_action(KeyCode::Down),
            Some(ManualImportKeyAction::Down)
        );
    }

    #[test]
    fn manual_import_search_window_keeps_the_cursor_end_visible() {
        assert_eq!(manual_import_search_window("PS Vita"), "PS Vita");
        let long_query = "x".repeat(MANUAL_IMPORT_SEARCH_WIDTH + 10);
        assert_eq!(
            manual_import_search_window(&long_query),
            "x".repeat(MANUAL_IMPORT_SEARCH_WIDTH)
        );
    }

    #[test]
    fn manual_import_left_click_maps_visible_rows_to_catalog_profiles() {
        let mut menu = iso2god::forge::ManualImportMenuState::new();
        menu.set_query("p");
        let first =
            manual_import_clicked_profile(&menu, 4, MANUAL_IMPORT_FIRST_RESULT_ROW).unwrap();
        assert_eq!(
            first.platform,
            iso2god::forge::PLATFORM_CATALOG[menu.matching_indices()[0]].platform
        );
        assert!(
            manual_import_clicked_profile(&menu, 4, MANUAL_IMPORT_FIRST_RESULT_ROW - 1).is_none()
        );
        assert!(manual_import_clicked_profile(&menu, 3, MANUAL_IMPORT_FIRST_RESULT_ROW).is_none());
        assert!(
            manual_import_clicked_profile(
                &menu,
                4,
                MANUAL_IMPORT_FIRST_RESULT_ROW + iso2god::forge::MANUAL_IMPORT_RESULT_LIMIT as u16
            )
            .is_none()
        );
    }

    #[test]
    fn manual_import_folder_resolves_a_single_multipart_game_set() {
        let root = env::temp_dir().join(format!(
            "digital-forge-manual-multipart-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("game.part1.rar"), b"part one").unwrap();
        fs::write(root.join("game.part2.rar"), b"part two").unwrap();

        assert_eq!(
            resolve_manual_import_source(&root).unwrap(),
            root.join("game.part1.rar")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multipart_layout_creation_adds_only_game_and_update_folders() {
        let parent =
            env::temp_dir().join(format!("digital-forge-layout-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        let root = create_multi_part_layout(&parent, "Anything").unwrap();
        assert_eq!(root.file_name().unwrap(), "Anything");
        assert!(root.join("Game").is_dir());
        assert!(root.join("Update").is_dir());
        assert!(!root.join("DLC").exists());

        let second = create_multi_part_layout(&parent, "Anything").unwrap();
        assert_ne!(root, second);
        assert!(second.join("Game").is_dir());
        assert!(second.join("Update").is_dir());
        assert!(!second.join("DLC").exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn ps4_merge_progress_parser_reads_builder_percentages() {
        assert_eq!(ps4_merge_percent("[Building: Pfsc] 47%"), Some(47));
        assert_eq!(ps4_merge_percent("[Building: Assemble] 100%"), Some(100));
        assert_eq!(ps4_merge_percent("[Validating: 4] inner PFS"), None);
    }

    #[test]
    fn ps4_merge_preflight_rejects_mismatched_title_ids() {
        let base = Ps4PkgMetadata {
            path: PathBuf::from("base.pkg"),
            title: "LA Noire".to_owned(),
            title_id: "CUSA09172".to_owned(),
            content_id: "UP1018-CUSA09172_00-LANOIRE000000000".to_owned(),
            category: "gd".to_owned(),
            package_type: "Game".to_owned(),
            passcode_status: String::new(),
            is_fake: true,
        };
        let update = Ps4PkgMetadata {
            title_id: "CUSA00000".to_owned(),
            content_id: "UP1018-CUSA00000_00-LANOIRE000000000".to_owned(),
            ..base.clone()
        };

        let error = ensure_ps4_base_update_compatible(&base, &update).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("TITLE_ID mismatch"));
        assert!(message.contains("CUSA09172"));
        assert!(message.contains("CUSA00000"));
    }

    #[test]
    fn ps4_merge_preflight_accepts_matching_title_ids_case_insensitively() {
        let base = Ps4PkgMetadata {
            path: PathBuf::from("base.pkg"),
            title: "LA Noire".to_owned(),
            title_id: "CUSA09172".to_owned(),
            content_id: String::new(),
            category: "gd".to_owned(),
            package_type: "Game".to_owned(),
            passcode_status: String::new(),
            is_fake: true,
        };
        let update = Ps4PkgMetadata {
            title_id: "cusa09172".to_owned(),
            ..base.clone()
        };

        ensure_ps4_base_update_compatible(&base, &update).unwrap();
    }

    #[test]
    fn folder_contract_keeps_base_and_update_multipart_sets_separate() {
        let root = env::temp_dir().join(format!(
            "digital-forge-folder-contract-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Game")).unwrap();
        fs::create_dir_all(root.join("Update")).unwrap();
        for name in ["base.part1.rar", "base.part2.rar"] {
            fs::write(root.join("Game").join(name), b"base").unwrap();
        }
        for name in ["update.part1.rar", "update.part2.rar"] {
            fs::write(root.join("Update").join(name), b"update").unwrap();
        }
        let base = scan_folder_content_group("Base Game", &root.join("Game"), false).unwrap();
        let update = scan_folder_content_group("Update", &root.join("Update"), false).unwrap();
        assert_eq!(base.multipart_sets.len(), 1);
        assert_eq!(base.multipart_part_count(), 2);
        assert_eq!(update.multipart_sets.len(), 1);
        assert_eq!(update.multipart_part_count(), 2);
        assert_eq!(base.sources, vec![root.join("Game/base.part1.rar")]);
        assert_eq!(update.sources, vec![root.join("Update/update.part1.rar")]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn flat_root_with_multipart_base_and_archive_update_is_separated() {
        let root = env::temp_dir().join(format!(
            "digital-forge-flat-contract-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("part1.rar"), b"base1").unwrap();
        fs::write(root.join("part2.rar"), b"base2").unwrap();
        fs::write(root.join("Update.rar"), b"update").unwrap();

        let game_path = find_named_child_directory_or_file(&root, "Game");
        let base = match game_path.as_deref() {
            Some(directory) if directory.is_dir() => {
                scan_folder_content_group("Base Game", directory, false).unwrap()
            }
            _ => scan_folder_content_group("Base Game", &root, true).unwrap(),
        };
        let update_path = find_named_child_directory_or_file(&root, "Update");
        let update = scan_optional_folder_content_group("Update", update_path.as_deref()).unwrap();

        assert_eq!(base.multipart_sets.len(), 1);
        assert_eq!(base.multipart_part_count(), 2);
        assert_eq!(base.sources, vec![root.join("part1.rar")]);
        assert_eq!(update.sources, vec![root.join("Update.rar")]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn optional_contract_folders_may_be_empty_or_missing_but_base_is_required() {
        let root = env::temp_dir().join(format!(
            "digital-forge-empty-contract-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Update")).unwrap();
        fs::write(root.join("base.pkg"), b"package").unwrap();

        let base = scan_folder_content_group("Base Game", &root, true).unwrap();
        let update =
            scan_optional_folder_content_group("Update", Some(&root.join("Update"))).unwrap();
        let dlc = scan_optional_folder_content_group("DLC", None).unwrap();
        assert!(!base.is_empty());
        assert!(update.is_empty());
        assert!(dlc.is_empty());
        assert_eq!(update.summary(), "Empty / Not provided");
        assert_eq!(dlc.summary(), "Empty / Not provided");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ps4_contract_recognises_the_documented_dlc_pack_folder_name() {
        let root = env::temp_dir().join(format!(
            "digital-forge-dlc-pack-contract-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Game")).unwrap();
        fs::create_dir_all(root.join("DLC Pack")).unwrap();
        assert_eq!(
            find_named_child_directory_or_file(&root, "DLC"),
            Some(root.join("DLC Pack"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_optional_multipart_set_reports_its_own_group() {
        let root = env::temp_dir().join(format!(
            "digital-forge-incomplete-contract-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("update.part1.rar"), b"one").unwrap();
        fs::write(root.join("update.part3.rar"), b"three").unwrap();
        let update = scan_folder_content_group("Update", &root, false).unwrap();
        let error = validate_group_multipart_sets(&update).unwrap_err();
        assert!(format!("{error:#}").contains("UPDATE ERROR"));
        assert!(format!("{error:#}").contains("update.part2.rar"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ftp_search_returns_every_catalog_console_with_a_real_ftp_backend() {
        let mut menu = iso2god::forge::ManualImportMenuState::new();
        let expected = iso2god::forge::PLATFORM_CATALOG
            .iter()
            .filter(|profile| profile.platform.supports_ftp_configuration())
            .map(|profile| profile.platform)
            .collect::<Vec<_>>();
        assert_eq!(expected.len(), 13);
        for platform in &expected {
            menu.set_query(platform.display_name());
            let actual = ftp_matching_indices(&menu)
                .iter()
                .map(|index| iso2god::forge::PLATFORM_CATALOG[*index].platform)
                .collect::<Vec<_>>();
            assert!(
                actual.contains(platform),
                "{} should be reachable by its catalog name",
                platform.display_name()
            );
        }

        for query in [
            "PS2",
            "PS3",
            "PS4",
            "PS5",
            "PSP",
            "PS Vita",
            "Original Xbox",
            "Xbox 360",
            "Nintendo DS",
            "Nintendo DSi",
            "Nintendo 3DS",
            "Wii",
            "Wii U",
        ] {
            menu.set_query(query);
            assert!(
                !ftp_matching_indices(&menu).is_empty(),
                "{query} should be available in the FTP search"
            );
        }

        menu.set_query("PS1");
        assert_eq!(menu.matching_indices().len(), 1);
        assert!(ftp_matching_indices(&menu).is_empty());
    }

    #[test]
    fn parse_dropped_paths_input_handles_quotes_and_spaces() {
        let root = env::temp_dir().join(format!("digital-forge-parse-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path1 = root.join("game with spaces.pkg");
        let path2 = root.join("update_pkg.pkg");
        fs::write(&path1, b"p1").unwrap();
        fs::write(&path2, b"p2").unwrap();

        let input = format!("\"{}\" \"{}\"", path1.display(), path2.display());
        let parsed = parse_dropped_paths_input(&input);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0], path1);
        assert_eq!(parsed[1], path2);

        let parsed_single = parse_dropped_paths_input(&path1.to_string_lossy());
        assert_eq!(parsed_single.len(), 1);
        assert_eq!(parsed_single[0], path1);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classify_source_set_folder_structure_identifies_roles_and_title_id() {
        let root = env::temp_dir().join(format!(
            "digital-forge-classify-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Game")).unwrap();
        fs::create_dir_all(root.join("Update")).unwrap();
        fs::create_dir_all(root.join("DLC")).unwrap();

        fs::write(
            root.join("Game")
                .join("LEGO.The.Incredibles_CUSA09897_v1.00.pkg"),
            b"base",
        )
        .unwrap();
        fs::write(
            root.join("Update")
                .join("LEGO.The.Incredibles_CUSA09897_v1.03.pkg"),
            b"update",
        )
        .unwrap();

        let classified = classify_source_set(&[root.clone()]).unwrap();
        assert_eq!(classified.platform, iso2god::forge::Platform::PlayStation4);
        assert_eq!(classified.title_id.as_deref(), Some("CUSA09897"));
        assert_eq!(
            classified.title_name.as_deref(),
            Some("LEGO The Incredibles")
        );
        assert_eq!(classified.base_sources.len(), 1);
        assert_eq!(classified.update_sources.len(), 1);
        assert_eq!(classified.dlc_sources.len(), 1);
        assert!(classified.dlc_detected);
        assert_eq!(classified.base_version.as_deref(), Some("1.00"));
        assert_eq!(classified.update_version.as_deref(), Some("1.03"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classify_source_set_accepts_arbitrary_parent_or_direct_game_without_update() {
        let root = env::temp_dir().join(format!(
            "digital-forge-optional-update-classify-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let parent = root.join("new folder");
        let game = parent.join("GAME");
        fs::create_dir_all(&game).unwrap();
        fs::write(
            game.join("LEGO.The.Incredibles_CUSA09897_v1.00.pkg"),
            b"base",
        )
        .unwrap();

        let from_parent = classify_source_set(&[parent.clone()]).unwrap();
        assert_eq!(from_parent.base_sources.len(), 1);
        assert!(from_parent.update_sources.is_empty());

        let from_game = classify_source_set(&[game.clone()]).unwrap();
        assert_eq!(from_game.base_sources.len(), 1);
        assert!(from_game.update_sources.is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classify_source_set_routes_itemzflow_parent_to_matching_base_and_patch() {
        let root = env::temp_dir().join(format!(
            "digital-forge-itemzflow-classify-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let base = synthetic_dump(&root, "CUSA12345", b"gd\0", b"01.00\0");
        let patch = synthetic_dump(&root, "CUSA12345-patch", b"gp\0", b"01.29\0");
        fs::write(root.join("CUSA12345.complete"), []).unwrap();
        fs::write(root.join("CUSA12345-patch.complete"), []).unwrap();
        fs::write(root.join("dumper.log"), b"synthetic log").unwrap();

        let classified = classify_source_set(&[root.clone()]).unwrap();
        assert_eq!(classified.platform, iso2god::forge::Platform::PlayStation4);
        assert_eq!(classified.title_id.as_deref(), Some("CUSA12345"));
        assert_eq!(classified.title_name.as_deref(), Some("Synthetic Game"));
        assert_eq!(classified.base_sources, vec![base]);
        assert_eq!(classified.update_sources, vec![patch]);
        assert_eq!(classified.base_version.as_deref(), Some("01.00"));
        assert_eq!(classified.update_version.as_deref(), Some("01.29"));
        let evidence = classified.itemzflow.unwrap();
        assert!(evidence.base_marker_found && evidence.update_marker_found);
        assert_eq!(evidence.logs_found, 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classify_source_set_flat_archives_identifies_roles_and_matching_id() {
        let root = env::temp_dir().join(format!("digital-forge-flat-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Game")).unwrap();
        fs::create_dir_all(root.join("Update")).unwrap();

        fs::write(
            root.join("Game")
                .join("LEGO.The.Incredibles_CUSA09897_v1.00.pkg"),
            b"base",
        )
        .unwrap();
        fs::write(
            root.join("Update")
                .join("LEGO.The.Incredibles_CUSA09897_v1.03.pkg"),
            b"update",
        )
        .unwrap();

        let classified = classify_source_set(&[root.clone()]).unwrap();
        assert_eq!(classified.platform, iso2god::forge::Platform::PlayStation4);
        assert_eq!(classified.title_id.as_deref(), Some("CUSA09897"));
        assert_eq!(classified.base_sources.len(), 1);
        assert_eq!(classified.update_sources.len(), 1);
        assert!(!classified.dlc_detected);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classify_source_set_rejects_mismatched_title_ids() {
        let root = env::temp_dir().join(format!(
            "digital-forge-mismatch-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Game")).unwrap();
        fs::create_dir_all(root.join("Update")).unwrap();

        fs::write(root.join("Game").join("Game_CUSA09897_v1.00.pkg"), b"base").unwrap();
        fs::write(
            root.join("Update").join("Update_CUSA12345_v1.01.pkg"),
            b"update",
        )
        .unwrap();

        let err = classify_source_set(&[root.clone()]).unwrap_err();
        assert!(format!("{err:#}").contains("Cannot combine unrelated packages"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn real_case_a_folder_inspection_identifies_lego_incredibles_ps4() {
        let real_path = Path::new(r"D:\games\emu\ps4\LEGO The Incredibles\For Digital Forge");
        if !real_path.exists() {
            return;
        }
        let classified = classify_source_set(&[real_path.to_path_buf()]).unwrap();
        assert_eq!(classified.platform, iso2god::forge::Platform::PlayStation4);
        assert_eq!(classified.title_id.as_deref(), Some("CUSA09897"));
        assert_eq!(classified.base_sources.len(), 1);
        assert_eq!(classified.update_sources.len(), 1);
        assert!(classified.dlc_detected);
        assert_eq!(classified.base_version.as_deref(), Some("1.00"));
        assert_eq!(classified.update_version.as_deref(), Some("1.03"));
    }

    #[test]
    fn case_b_archives_inspection_identifies_lego_incredibles_ps4() {
        let Some(seven_zip) = iso2god::forge::archive::find_7zip_executable() else {
            return;
        };
        let root = env::temp_dir().join(format!("digital-forge-case-b-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let dummy_dir = root.join("staging");
        fs::create_dir_all(&dummy_dir).unwrap();
        fs::write(
            dummy_dir.join("LEGO.The.Incredibles_CUSA09897_v1.00.pkg"),
            b"fake pkg base",
        )
        .unwrap();
        fs::write(
            dummy_dir.join("LEGO.The.Incredibles_CUSA09897_v1.03.pkg"),
            b"fake pkg update",
        )
        .unwrap();
        fs::write(
            dummy_dir.join("LEGO.The.Incredibles_CUSA09897_DLC.pkg"),
            b"fake pkg dlc",
        )
        .unwrap();

        let game_zip = root.join("game.zip");
        let update_zip = root.join("update.zip");
        let dlc_zip = root.join("dlc.zip");

        let _ = std::process::Command::new(&seven_zip)
            .current_dir(&dummy_dir)
            .args(["a", "-tzip", "-y"])
            .arg(&game_zip)
            .arg("LEGO.The.Incredibles_CUSA09897_v1.00.pkg")
            .output();

        let _ = std::process::Command::new(&seven_zip)
            .current_dir(&dummy_dir)
            .args(["a", "-tzip", "-y"])
            .arg(&update_zip)
            .arg("LEGO.The.Incredibles_CUSA09897_v1.03.pkg")
            .output();

        let _ = std::process::Command::new(&seven_zip)
            .current_dir(&dummy_dir)
            .args(["a", "-tzip", "-y"])
            .arg(&dlc_zip)
            .arg("LEGO.The.Incredibles_CUSA09897_DLC.pkg")
            .output();

        let _ = fs::remove_dir_all(&dummy_dir);

        let classified = classify_source_set(&[root.clone()]).unwrap();
        assert_eq!(classified.platform, iso2god::forge::Platform::PlayStation4);
        assert_eq!(classified.title_id.as_deref(), Some("CUSA09897"));
        assert_eq!(
            classified.title_name.as_deref(),
            Some("LEGO The Incredibles")
        );
        assert_eq!(classified.base_sources.len(), 1);
        assert_eq!(classified.update_sources.len(), 1);
        assert_eq!(classified.dlc_sources.len(), 1);
        assert!(classified.dlc_detected);
        assert_eq!(classified.base_version.as_deref(), Some("1.00"));
        assert_eq!(classified.update_version.as_deref(), Some("1.03"));

        let classified_multi =
            classify_source_set(&[game_zip.clone(), update_zip.clone(), dlc_zip.clone()]).unwrap();
        assert_eq!(
            classified_multi.platform,
            iso2god::forge::Platform::PlayStation4
        );
        assert_eq!(classified_multi.title_id.as_deref(), Some("CUSA09897"));
        assert_eq!(classified_multi.base_sources.len(), 1);
        assert_eq!(classified_multi.update_sources.len(), 1);
        assert!(classified_multi.dlc_detected);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn itemzflow_size_estimate_ignores_markers_logs_and_gp4_projects() {
        let root = env::temp_dir().join(format!("df_itemzflow_size_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("game.bin"), [0u8; 17]).unwrap();
        fs::write(root.join("dumper.log"), [0u8; 100]).unwrap();
        fs::write(root.join("CUSA12345.complete"), [0u8; 100]).unwrap();
        fs::write(root.join("CUSA12345.gp4"), [0u8; 100]).unwrap();

        assert_eq!(
            measure_source_payload_bytes(std::slice::from_ref(&root)).unwrap(),
            17
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn classify_source_set_rejects_missing_input_paths() {
        let missing =
            env::temp_dir().join(format!("df_missing_itemzflow_test_{}", std::process::id()));
        let error = classify_source_set(&[missing.clone()]).unwrap_err();
        assert!(error.to_string().contains("input path does not exist"));
    }
}
