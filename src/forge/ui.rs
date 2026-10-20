use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Error, bail, ensure};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

use super::archive::{ArchiveEngine, ArchiveKind, TempWorkspace};
use super::backend::ProgressUpdate;
use super::backends::get_backend_for_id;
use super::catalog::{self, PLATFORM_CATALOG, PlatformCategory, PlatformProfile};
use super::detector::{DetectionResult, FormatDetector};
use super::format::{SourceFormat, TargetFormat};
use super::matrix::{FormatConversionRule, FormatMatrix};
use super::platform::Platform;
use super::profile::{
    ConsoleStorage, TargetProfile, TargetWorkflow, UsageMode, target_workflow_for_console_storage,
    target_workflow_for_usage, target_workflows,
};
use super::transfer::{TransferDestination, TransferManager};

pub struct FormatChoiceOption {
    pub target_format: TargetFormat,
    pub label: String,
    pub is_recommended: bool,
    pub is_passthrough: bool,
    pub rule: &'static FormatConversionRule,
}

#[derive(Debug, Clone)]
pub struct ForgeFlowPreset {
    pub target_format: TargetFormat,
    pub target_workflow: TargetWorkflow,
    pub transfer_destination: TransferDestination,
}

struct ForgeRawModeGuard;

impl ForgeRawModeGuard {
    fn enter() -> Result<Self, Error> {
        enable_raw_mode().context("could not enable arrow-key menu input")?;
        print!("\x1b[?25l");
        io::stdout().flush()?;
        Ok(Self)
    }
}

impl Drop for ForgeRawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        print!("\x1b[?25h");
        let _ = io::stdout().flush();
    }
}

pub const MANUAL_IMPORT_RESULT_LIMIT: usize = 12;

fn arrow_menu<T: AsRef<str>>(labels: &[T]) -> Result<Option<usize>, Error> {
    ensure!(!labels.is_empty(), "menu has no selectable entries");
    let mut selected = 0usize;
    draw_arrow_menu(labels, selected, false)?;
    if std::env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").is_some() {
        return Ok(None);
    }
    let _raw_mode = ForgeRawModeGuard::enter()?;
    loop {
        let Event::Key(key) = event::read().context("could not read menu input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Up => {
                selected = selected.checked_sub(1).unwrap_or(labels.len() - 1);
                draw_arrow_menu(labels, selected, true)?;
            }
            KeyCode::Down => {
                selected = (selected + 1) % labels.len();
                draw_arrow_menu(labels, selected, true)?;
            }
            KeyCode::Enter => {
                println!();
                return Ok(Some(selected));
            }
            KeyCode::Esc | KeyCode::Backspace => {
                println!();
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn draw_arrow_menu<T: AsRef<str>>(
    labels: &[T],
    selected: usize,
    redraw: bool,
) -> Result<(), Error> {
    if redraw {
        print!("\x1b[{}A", labels.len());
    }
    for (index, label) in labels.iter().enumerate() {
        print!("\r\x1b[2K");
        if index == selected {
            println!("\x1b[38;5;208;1m  > {}\x1b[0m", label.as_ref());
        } else {
            println!("    {}", label.as_ref());
        }
    }
    io::stdout().flush()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualImportMenuState {
    query: String,
    matching_indices: Vec<usize>,
}

impl Default for ManualImportMenuState {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualImportMenuState {
    pub fn new() -> Self {
        assert!(
            !PLATFORM_CATALOG.is_empty(),
            "the platform catalog must not be empty"
        );
        Self {
            query: String::new(),
            matching_indices: Vec::new(),
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn set_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        self.refresh_matches();
    }

    pub fn push(&mut self, character: char) {
        if !character.is_control() {
            self.query.push(character);
            self.refresh_matches();
        }
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.refresh_matches();
    }

    pub fn matching_indices(&self) -> &[usize] {
        &self.matching_indices
    }

    pub fn selected_profile(&self) -> Option<&'static PlatformProfile> {
        (self.matching_indices.len() == 1).then(|| &PLATFORM_CATALOG[self.matching_indices[0]])
    }

    fn refresh_matches(&mut self) {
        let query = normalise_platform_search(&self.query);
        self.matching_indices = if query.is_empty() {
            Vec::new()
        } else {
            let searchable = PLATFORM_CATALOG.iter().enumerate().map(|(index, profile)| {
                (
                    index,
                    normalise_platform_search(profile.display_name),
                    normalise_platform_search(profile.platform.short_code()),
                )
            });
            let exact = searchable
                .clone()
                .filter_map(|(index, display_name, short_code)| {
                    (display_name == query || short_code == query).then_some(index)
                })
                .collect::<Vec<_>>();
            if exact.is_empty() {
                searchable
                    .filter_map(|(index, display_name, short_code)| {
                        (display_name.contains(&query) || short_code.contains(&query))
                            .then_some(index)
                    })
                    .collect()
            } else {
                exact
            }
        };
    }
}

fn normalise_platform_search(value: &str) -> String {
    value
        .to_lowercase()
        .replace("play station", "ps")
        .replace("playstation", "ps")
        .replace("x box", "xbox")
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect()
}

fn extract_archive_with_password_prompt(
    archive_path: &Path,
    extract_dir: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<PathBuf, Error> {
    let initial_error = match ArchiveEngine::extract_archive(
        archive_path,
        extract_dir,
        None,
        Arc::clone(&cancelled),
    ) {
        Ok(path) => return Ok(path),
        Err(error) => error,
    };
    let initial_message = format!("{initial_error:#}");
    if initial_message.contains("was not found") || initial_message.contains("failed to launch") {
        return Err(initial_error);
    }

    loop {
        println!("\nThe archive may be password protected.");
        print!("Archive password (visible; right-click to paste, Enter to cancel): ");
        io::stdout().flush()?;
        let mut password = String::new();
        io::stdin()
            .read_line(&mut password)
            .context("could not read the archive password")?;
        let password = password.trim().to_owned();
        if password.is_empty() {
            bail!("Archive extraction was cancelled. Original error: {initial_message}");
        }

        if extract_dir.exists() {
            fs::remove_dir_all(extract_dir)
                .context("could not clear the temporary extraction folder before retrying")?;
        }
        match ArchiveEngine::extract_archive(
            archive_path,
            extract_dir,
            Some(password.as_str()),
            Arc::clone(&cancelled),
        ) {
            Ok(path) => return Ok(path),
            Err(_) => {
                println!("The password was not accepted. Try again or press Enter to cancel.")
            }
        }
    }
}

fn detect_source_or_nested_payload(
    source: &Path,
    selected_platform: Option<Platform>,
) -> Result<DetectionResult, Error> {
    let direct = FormatDetector::detect(source, selected_platform)?;
    if direct.source_format != SourceFormat::Unknown || !source.is_dir() {
        return Ok(direct);
    }

    let mut candidates = Vec::new();
    collect_nested_payloads(source, selected_platform, &mut candidates)?;
    if candidates.len() == 1 {
        let (path, detection) = candidates.remove(0);
        println!("\nDetected extracted payload: {}", path.display());
        return Ok(detection);
    }
    if candidates.len() > 1 {
        let first_format = candidates[0].1.source_format;
        let first_platform = candidates[0].1.platform;
        let is_ps4_pkg =
            first_format == SourceFormat::Ps4FakePkg || first_format == SourceFormat::Ps4RetailPkg;
        if is_ps4_pkg
            && candidates.iter().all(|(_, detection)| {
                (detection.source_format == SourceFormat::Ps4FakePkg
                    || detection.source_format == SourceFormat::Ps4RetailPkg)
                    && detection.platform == first_platform
            })
        {
            let mut combined = candidates[0].1.clone();
            combined.related_files = candidates.iter().map(|(path, _)| path.clone()).collect();
            combined.is_multipart = false;
            combined.package_classification = None;
            println!(
                "\nDetected {} extracted PS4 packages.",
                combined.related_files.len()
            );
            return Ok(combined);
        }
        bail!(
            "The extracted archive contains multiple supported payloads; place Base Game and Update in their matching folders"
        );
    }
    Ok(direct)
}

fn collect_nested_payloads(
    directory: &Path,
    selected_platform: Option<Platform>,
    candidates: &mut Vec<(PathBuf, DetectionResult)>,
) -> Result<(), Error> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("could not inspect extracted folder {}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let detection = FormatDetector::detect(&path, selected_platform)?;
        if detection.source_format != SourceFormat::Unknown
            && !matches!(
                detection.source_format,
                SourceFormat::Archive | SourceFormat::MultipartArchive
            )
        {
            candidates.push((path, detection));
        } else if path.is_dir() {
            collect_nested_payloads(&path, selected_platform, candidates)?;
        }
    }
    Ok(())
}

pub struct ForgeUi;

impl ForgeUi {
    /// Console-centric source for frontend tiles and platform selection.
    pub fn platform_catalog() -> &'static [PlatformProfile] {
        PLATFORM_CATALOG
    }

    pub fn manual_import_platform_count() -> usize {
        PLATFORM_CATALOG.len()
    }

    pub fn source_formats(platform: Platform) -> &'static [super::format::SourceFormat] {
        catalog::profile(platform).source_formats
    }

    pub fn run_interactive_flow(
        source: &Path,
        selected_platform: Option<Platform>,
    ) -> Result<(), Error> {
        Self::run_interactive_flow_with_preset(source, selected_platform, None).map(|_| ())
    }

    pub fn run_interactive_flow_with_preset(
        source: &Path,
        selected_platform: Option<Platform>,
        preset: Option<&ForgeFlowPreset>,
    ) -> Result<Option<ForgeFlowPreset>, Error> {
        Self::run_interactive_flow_configured(source, selected_platform, preset, None)
    }

    pub fn run_interactive_flow_with_target(
        source: &Path,
        selected_platform: Platform,
        target_workflow: TargetWorkflow,
    ) -> Result<Option<ForgeFlowPreset>, Error> {
        Self::run_interactive_flow_configured(
            source,
            Some(selected_platform),
            None,
            Some(target_workflow),
        )
    }

    fn run_interactive_flow_configured(
        source: &Path,
        selected_platform: Option<Platform>,
        preset: Option<&ForgeFlowPreset>,
        requested_workflow: Option<TargetWorkflow>,
    ) -> Result<Option<ForgeFlowPreset>, Error> {
        let temp_ws = TempWorkspace::new("forge_flow")?;

        // 1. Initial inspection & Archive layer
        let mut active_source = source.to_path_buf();
        let archive_kind = ArchiveEngine::classify(&active_source);
        let structured_rom_set = selected_platform.is_some_and(|platform| {
            catalog::profile(platform).category == PlatformCategory::ArcadeDerived
                || matches!(
                    platform,
                    Platform::EpochCassetteVision
                        | Platform::EpochSuperCassetteVision
                        | Platform::SunplusNative32
                )
        });
        let ps5_dump_tar = selected_platform == Some(Platform::PlayStation5)
            && FormatDetector::detect(&active_source, selected_platform)
                .is_ok_and(|detection| detection.source_format == SourceFormat::Ps5DumpTar);

        match archive_kind {
            ArchiveKind::SingleVolume(_) | ArchiveKind::MultiVolume { .. }
                if structured_rom_set || ps5_dump_tar => {}
            ArchiveKind::MultiVolume {
                ref first_volume,
                ref all_volumes,
                ref missing_volumes,
            } => {
                if !missing_volumes.is_empty() {
                    println!("\n\x1b[31;1mError: Missing multi-volume archive parts:\x1b[0m");
                    for m in missing_volumes {
                        println!("  - {m}");
                    }
                    bail!("Cannot continue with incomplete archive set");
                }
                println!(
                    "\nDetected multi-volume archive ({} parts). Extracting...",
                    all_volumes.len()
                );
                let cancelled = Arc::new(AtomicBool::new(false));
                let extract_dir = temp_ws.root.join("extracted");
                active_source =
                    extract_archive_with_password_prompt(first_volume, &extract_dir, cancelled)?;
            }
            ArchiveKind::SingleVolume(ref archive_path) => {
                println!("\nDetected compressed archive. Extracting to temporary workspace...");
                let cancelled = Arc::new(AtomicBool::new(false));
                let extract_dir = temp_ws.root.join("extracted");
                active_source =
                    extract_archive_with_password_prompt(archive_path, &extract_dir, cancelled)?;
            }
            _ => {}
        }

        // 2. Deep container & format detection
        let detection = detect_source_or_nested_payload(&active_source, selected_platform)?;
        let platform = selected_platform.unwrap_or(detection.platform);
        let platform_profile = catalog::profile(platform);
        ensure!(
            platform_profile
                .source_formats
                .contains(&detection.source_format),
            "{} is not a valid {} source format; accepted formats: {}",
            detection.source_format.display_name(),
            platform_profile.display_name,
            platform_profile
                .source_formats
                .iter()
                .map(|format| format.display_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
        // 3. Destination intent and target selection.  Formats are selected by
        // the target contract rather than being presented as the first choice.
        let target_workflow = if let Some(preset) = preset {
            preset.target_workflow
        } else if let Some(workflow) = requested_workflow {
            workflow
        } else {
            let Some(usage) = Self::prompt_usage_mode(platform)? else {
                println!("\nOperation cancelled.");
                return Ok(None);
            };
            if usage == UsageMode::PhysicalConsole && platform == Platform::PlayStation4 {
                let Some(storage) = Self::prompt_console_storage(platform)? else {
                    println!("\nOperation cancelled.");
                    return Ok(None);
                };
                target_workflow_for_console_storage(platform, storage)
                    .context("no console storage workflow is configured")?
            } else if let Some(workflow) = target_workflow_for_usage(platform, usage) {
                workflow
            } else {
                let Some(workflow) = Self::prompt_target_workflow(platform, usage)? else {
                    println!("\nOperation cancelled.");
                    return Ok(None);
                };
                workflow
            }
        };
        let profile = target_workflow.target_profile(platform);
        let choices = Self::build_format_choices(&detection, platform, profile);
        if choices.is_empty() {
            bail!("No supported conversion or transfer paths found for this game format");
        }

        let selected_choice = if let Some(preset) = preset {
            choices
                .iter()
                .find(|choice| choice.target_format == preset.target_format)
        } else {
            Self::choice_for_target(&detection, platform, target_workflow, &choices)
        };
        let Some(choice) = selected_choice else {
            let accepted = choices
                .iter()
                .map(|choice| choice.target_format.display_name())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "{} cannot safely produce {} from {}; available backend paths: {}",
                platform.display_name(),
                target_workflow.display_name(),
                detection.source_format.display_name(),
                accepted
            );
        };
        println!("\nTarget: {}", target_workflow.display_name());
        println!("Workflow: {}", choice.target_format.display_name());

        // 4. Select the output folder.
        let transfer_dest = if let Some(preset) = preset {
            preset.transfer_destination.clone()
        } else {
            Self::prompt_transfer_destination(platform)?
        };

        // 5. Pre-conversion Validation
        println!("\n\x1b[1mPreparing {}\x1b[0m...", platform.display_name());
        let backend = get_backend_for_id(choice.rule.backend_id).context("Backend not found")?;

        let validate_report = backend.validate_input(&detection)?;
        if !validate_report.is_valid {
            println!("\n\x1b[31;1mValidation Failed:\x1b[0m");
            for issue in &validate_report.issues {
                println!("  - {issue}");
            }
            bail!("Input validation failed");
        }

        for warning in &validate_report.warnings {
            println!("  \x1b[33mNote:\x1b[0m {warning}");
        }

        // 6. Conversion in isolated temporary workspace
        let staging_dir = temp_ws.root.join("staged");
        fs::create_dir_all(&staging_dir)?;

        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_from_keyboard = cancelled.clone();
        let progress_cb = Arc::new(move |p: ProgressUpdate| {
            if alt_c_pressed() {
                cancel_from_keyboard.store(true, Ordering::Relaxed);
            }
            let bar = professional_progress_bar(p.percent, 24);
            let detail_str = p.details.as_deref().unwrap_or("").trim();
            // Truncate detail so the progress line fits strictly on a single 80-column line and never wraps
            let truncated_detail = if detail_str.len() > 22 {
                let clean_name = detail_str.rsplit(['/', '\\']).next().unwrap_or(detail_str);
                if clean_name.len() > 22 {
                    format!("...{}", &clean_name[clean_name.len() - 19..])
                } else {
                    clean_name.to_string()
                }
            } else if !detail_str.is_empty() {
                detail_str.to_string()
            } else {
                String::new()
            };
            let info = if truncated_detail.is_empty() {
                p.stage.clone()
            } else {
                format!("{} ({})", p.stage, truncated_detail)
            };
            let stage_label = if p.stage.to_lowercase().contains("saving")
                || p.stage.to_lowercase().contains("transfer")
            {
                "Saving     "
            } else {
                "Converting "
            };
            print!(
                "\r\x1b[2K{} {bar}  {:>3}%  {:<32}  \x1b[90mAlt+C Cancel\x1b[0m",
                stage_label, p.percent, info
            );
            let _ = io::stdout().flush();
        });

        let conversion_result = backend.convert(
            &detection,
            choice.target_format,
            &staging_dir,
            progress_cb.clone(),
            cancelled.clone(),
        )?;

        // 7. Post-conversion Output Validation
        let out_report = backend.validate_output(&conversion_result)?;
        if !out_report.is_valid {
            print!("\r\x1b[2K");
            let _ = io::stdout().flush();
            println!("\n\x1b[31;1mOutput Validation Failed:\x1b[0m");
            for issue in &out_report.issues {
                println!("  - {issue}");
            }
            bail!("Output validation failed");
        }
        Self::validate_target_output(&conversion_result.output_path, target_workflow)?;

        // 8. Deliver finished result to the selected folder.
        let final_destination =
            TransferManager::transfer(&conversion_result, &transfer_dest, progress_cb, cancelled)?;

        Self::validate_target_output(&final_destination, target_workflow)?;
        if matches!(target_workflow, TargetWorkflow::Ps4ItemzFlow)
            && let TransferDestination::RemovableDrive { target_root, .. } = &transfer_dest
        {
            let canonical_root = target_root.canonicalize()?;
            let canonical_output = final_destination.canonicalize()?;
            let depth = canonical_output
                .strip_prefix(&canonical_root)
                .context("ItemzFlow output was written outside the selected external drive")?
                .components()
                .count();
            ensure!(
                depth <= 5,
                "ItemzFlow scans only five directory levels; selected output is at depth {depth}"
            );
        }

        print!("\r\x1b[2K");
        let _ = io::stdout().flush();
        println!(
            "\x1b[32;1mConversion complete. Output verified and delivered successfully.\x1b[0m"
        );
        println!("  {}", final_destination.display());
        Ok(Some(ForgeFlowPreset {
            target_format: choice.target_format,
            target_workflow,
            transfer_destination: transfer_dest,
        }))
    }

    pub fn choice_for_target<'a>(
        detection: &DetectionResult,
        platform: Platform,
        workflow: TargetWorkflow,
        choices: &'a [FormatChoiceOption],
    ) -> Option<&'a FormatChoiceOption> {
        if let Some(target) = workflow.forced_target() {
            return choices.iter().find(|choice| choice.target_format == target);
        }

        if workflow == TargetWorkflow::Xbox360Xenia {
            let target = match detection.source_format {
                SourceFormat::Xbox360Iso => Some(TargetFormat::Xbox360Iso),
                SourceFormat::Xbox360God => Some(TargetFormat::Xbox360God),
                SourceFormat::Xbox360Folder => Some(TargetFormat::Xbox360Folder),
                SourceFormat::Xbox360SplitIso => Some(TargetFormat::Xbox360Iso),
                _ => None,
            };
            if let Some(target) = target
                && let Some(choice) = choices.iter().find(|choice| choice.target_format == target)
            {
                return Some(choice);
            }
        }

        if workflow.usage_mode() == UsageMode::Emulator {
            if let Some(choice) = choices.iter().find(|choice| choice.is_passthrough) {
                return Some(choice);
            }
        }

        let profile = workflow.target_profile(platform);
        choices
            .iter()
            .find(|choice| choice.target_format == profile.recommended_target())
            .or_else(|| choices.iter().find(|choice| choice.is_recommended))
            .or_else(|| choices.first())
    }

    fn validate_target_output(path: &Path, workflow: TargetWorkflow) -> Result<(), Error> {
        match workflow {
            TargetWorkflow::Ps4ItemzFlow | TargetWorkflow::Ps4ShadPs4 => {
                super::ps4::validate_target_folder(path, workflow)
            }
            TargetWorkflow::Ps4InternalInstall => {
                ensure!(path.is_file(), "PS4 internal output must be a package file");
                ensure!(
                    path.extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("pkg")),
                    "PS4 internal output must be a .pkg FPKG/PKG file"
                );
                Ok(())
            }
            TargetWorkflow::Ps5ItemzFlow => {
                if path.is_dir() {
                    super::ps5::validate_target_folder(path, workflow)
                } else {
                    bail!("PS5 ItemzFlow output must be a validated loose game folder")
                }
            }
            TargetWorkflow::Ps5AnyPS5 => {
                if path.is_dir() {
                    super::ps5::validate_anyps5_output(path)?;
                    Ok(())
                } else {
                    bail!("AnyPS5 output must contain validated source/ and app0/ folders")
                }
            }
            TargetWorkflow::Ps5InternalInstall => {
                ensure!(path.is_file(), "PS5 internal output must be a package file");
                ensure!(
                    path.extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("pkg")),
                    "PS5 internal output must be a .pkg file"
                );
                Ok(())
            }
            _ => Ok(()),
        }
    }

    pub fn build_format_choices(
        detection: &DetectionResult,
        platform: Platform,
        profile: TargetProfile,
    ) -> Vec<FormatChoiceOption> {
        let mut rules = FormatMatrix::get_rules_for_source(platform, detection.source_format);
        if platform == Platform::PlayStation4 && rules.is_empty() {
            rules = FormatMatrix::get_rules_for_source(platform, SourceFormat::Ps4FakePkg);
        }
        let mut options = Vec::new();

        for rule in rules {
            let is_recommended = FormatMatrix::is_recommended(rule, profile);
            let mut label = rule.target_format.display_name().to_string();

            if is_recommended {
                label.push_str(" \x1b[32;1m[Recommended]\x1b[0m");
            }
            if rule.is_passthrough {
                label.push_str(" \x1b[36m[Keep current format]\x1b[0m");
            }
            if let Some(req) = rule.requirements {
                label.push_str(&format!(" \x1b[90m({req})\x1b[0m"));
            }

            options.push(FormatChoiceOption {
                target_format: rule.target_format,
                label,
                is_recommended,
                is_passthrough: rule.is_passthrough,
                rule,
            });
        }

        // Sort so Recommended options appear first
        options.sort_by(|a, b| b.is_recommended.cmp(&a.is_recommended));
        options
    }

    pub fn prompt_usage_mode(platform: Platform) -> Result<Option<UsageMode>, Error> {
        print!("\x1b[2J\x1b[H");
        io::stdout().flush()?;
        println!("\n\x1b[1m{}\x1b[0m", platform.display_name());
        println!("========================================================");
        println!("How will you use this game?\n");
        let labels = if platform == Platform::PlayStation5 {
            ["Physical PS5 / ItemzFlow", "PC / AnyPS5", "Back"]
        } else {
            ["Physical Console", "Emulator", "Back"]
        };
        Ok(arrow_menu(&labels)?.and_then(|index| match index {
            0 => Some(UsageMode::PhysicalConsole),
            1 => Some(UsageMode::Emulator),
            _ => None,
        }))
    }

    pub fn prompt_console_storage(platform: Platform) -> Result<Option<ConsoleStorage>, Error> {
        print!("\x1b[2J\x1b[H");
        io::stdout().flush()?;
        println!(
            "\n\x1b[1m{} — Physical Console\x1b[0m",
            platform.display_name()
        );
        println!("========================================================");
        println!("Where will you use this game?\n");
        let labels = ["Internal storage", "External USB storage", "Back"];
        Ok(arrow_menu(&labels)?.and_then(|index| match index {
            0 => Some(ConsoleStorage::Internal),
            1 => Some(ConsoleStorage::External),
            _ => None,
        }))
    }

    pub fn prompt_target_workflow(
        platform: Platform,
        usage: UsageMode,
    ) -> Result<Option<TargetWorkflow>, Error> {
        print!("\x1b[2J\x1b[H");
        io::stdout().flush()?;
        let workflows = target_workflows(platform, usage);
        println!(
            "\n\x1b[1m{} — {}\x1b[0m",
            platform.display_name(),
            if platform == Platform::PlayStation5 && usage == UsageMode::Emulator {
                "PC / AnyPS5"
            } else {
                usage.display_name()
            }
        );
        println!("========================================================");
        println!(
            "{}\n",
            if platform == Platform::PlayStation5 {
                "Select the PS5 output workflow:"
            } else {
                "Select the target loader or emulator:"
            }
        );
        let mut labels = workflows
            .iter()
            .map(|workflow| workflow.display_name().to_string())
            .collect::<Vec<_>>();
        labels.push("Back".to_string());
        Ok(arrow_menu(&labels)?.and_then(|index| {
            if index < workflows.len() {
                Some(workflows[index])
            } else {
                None
            }
        }))
    }

    pub fn prompt_transfer_destination(_platform: Platform) -> Result<TransferDestination, Error> {
        let target_dest_dir = choose_output_folder()?;
        Ok(TransferDestination::LocalFolder(target_dest_dir))
    }
}

fn choose_output_folder() -> Result<PathBuf, Error> {
    if let Some(path) = std::env::var_os("DIGITAL_FORGE_OUTPUT_DIR") {
        let path = PathBuf::from(path);
        ensure!(
            path.is_dir(),
            "DIGITAL_FORGE_OUTPUT_DIR is not an existing directory: {}",
            path.display()
        );
        return Ok(path);
    }

    #[cfg(windows)]
    {
        return rfd::FileDialog::new()
            .set_title("Select where to place the game")
            .pick_folder()
            .ok_or_else(|| anyhow::anyhow!("destination selection cancelled"));
    }

    print!("Output folder: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let selected = input.trim().trim_matches('"');
    ensure!(!selected.is_empty(), "destination selection cancelled");
    Ok(PathBuf::from(selected))
}

fn default_downloads_directory() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join("Downloads");
    }
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Downloads");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Downloads")
}

fn unique_new_folder(parent: &Path) -> PathBuf {
    let first = parent.join("New Folder");
    if !first.exists() {
        return first;
    }
    for suffix in 2.. {
        let candidate = parent.join(format!("New Folder ({suffix})"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn professional_progress_bar(percent: u8, width: usize) -> String {
    let filled = (usize::from(percent.min(100)) * width / 100).min(width);
    format!(
        "\x1b[90m[\x1b[38;5;208m{}\x1b[0m{}\x1b[90m]\x1b[0m",
        "█".repeat(filled),
        " ".repeat(width.saturating_sub(filled))
    )
}

fn alt_c_pressed() -> bool {
    if !io::stdin().is_terminal() || !event::poll(std::time::Duration::ZERO).unwrap_or(false) {
        return false;
    }
    matches!(
        event::read(),
        Ok(Event::Key(key))
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::ALT)
    )
}

struct DetectedDriveOption {
    drive_letter: String,
    root: PathBuf,
    label: String,
    free_bytes: u64,
}

#[cfg(windows)]
fn detect_connected_drives() -> Vec<DetectedDriveOption> {
    let mut drives = Vec::new();
    for letter in b'D'..=b'Z' {
        let root_str = format!("{}:\\", letter as char);
        let path = Path::new(&root_str);
        if path.exists() {
            if let Ok(free) = fs2::available_space(path) {
                let vol_label = crate::forge::transfer::volume_label(path);
                let label = match vol_label {
                    Some(ref v) if !v.is_empty() => format!("{}: ({v})", letter as char),
                    _ => format!("{}:", letter as char),
                };
                drives.push(DetectedDriveOption {
                    drive_letter: format!("{}:", letter as char),
                    root: path.to_path_buf(),
                    label,
                    free_bytes: free,
                });
            }
        }
    }
    drives
}

#[cfg(not(windows))]
fn detect_connected_drives() -> Vec<DetectedDriveOption> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{detect_source_or_nested_payload, professional_progress_bar, unique_new_folder};
    use crate::forge::archive::TempWorkspace;
    use crate::forge::format::SourceFormat;
    use crate::forge::platform::Platform;

    #[test]
    fn conversion_progress_bar_has_a_solid_orange_fill_and_clean_empty_track() {
        let bar = professional_progress_bar(50, 20);
        assert_eq!(bar.matches('█').count(), 10);
        assert!(!bar.contains('░'));
        assert!(!bar.contains('▒'));
        assert!(!bar.contains('▓'));
    }

    #[test]
    fn downloads_output_uses_a_unique_new_folder_without_asking_for_a_name() {
        let workspace = TempWorkspace::new("downloads_new_folder").unwrap();
        assert_eq!(
            unique_new_folder(&workspace.root),
            workspace.root.join("New Folder")
        );
        fs::create_dir(workspace.root.join("New Folder")).unwrap();
        assert_eq!(
            unique_new_folder(&workspace.root),
            workspace.root.join("New Folder (2)")
        );
    }

    #[test]
    fn extracted_release_wrapper_selects_the_single_console_payload() {
        let workspace = TempWorkspace::new("nested_pkg_detection").unwrap();
        let wrapper = workspace.root.join("release-name");
        fs::create_dir_all(&wrapper).unwrap();
        fs::write(wrapper.join("release.nfo"), b"release notes").unwrap();
        fs::write(wrapper.join("password.txt"), b"password").unwrap();
        let package = wrapper.join("game.pkg");
        let mut header = vec![0u8; 4096];
        header[..4].copy_from_slice(b"\x7fPKG");
        header[0x70..0x74].copy_from_slice(&0x0Fu32.to_be_bytes());
        fs::write(&package, header).unwrap();

        let detection =
            detect_source_or_nested_payload(&workspace.root, Some(Platform::PlayStation4)).unwrap();
        assert_eq!(detection.platform, Platform::PlayStation4);
        assert_eq!(detection.source_format, SourceFormat::Ps4FakePkg);
        assert_eq!(detection.related_files, vec![package]);
    }

    #[test]
    fn extracted_dlc_archive_collects_every_ps4_package() {
        let workspace = TempWorkspace::new("nested_dlc_detection").unwrap();
        let wrapper = workspace.root.join("dlc-release");
        fs::create_dir_all(&wrapper).unwrap();
        for name in ["dlc-one.pkg", "dlc-two.pkg", "dlc-three.pkg"] {
            let mut header = vec![0u8; 4096];
            header[..4].copy_from_slice(b"\x7fPKG");
            header[0x70..0x74].copy_from_slice(&0x0Fu32.to_be_bytes());
            fs::write(wrapper.join(name), header).unwrap();
        }

        let detection =
            detect_source_or_nested_payload(&workspace.root, Some(Platform::PlayStation4)).unwrap();
        assert_eq!(detection.platform, Platform::PlayStation4);
        assert_eq!(detection.source_format, SourceFormat::Ps4FakePkg);
        assert_eq!(detection.related_files.len(), 3);
    }
}
