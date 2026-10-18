use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Error, bail, ensure};

use crate::forge::backend::{
    ConversionBackend, ConversionResult, ProgressUpdate, ToolReport, ValidationReport,
};
use crate::forge::detector::DetectionResult;
use crate::forge::format::{SourceFormat, TargetFormat};
use crate::forge::matrix::{FormatMatrix, SupportStatus};
use crate::forge::platform::Platform;

const SPLIT_SIZE: u64 = 3_900_000_000;
const USBEXTREME_CHUNK_SIZE: u64 = 1_073_741_824;
const PS5_MULTIPART_HEADER_SIZE: usize = 4096;

#[derive(Clone, Copy)]
pub struct ToolBackend {
    id: &'static str,
    platform: Platform,
}

impl ToolBackend {
    pub const fn new(id: &'static str, platform: Platform) -> Self {
        Self { id, platform }
    }
}

impl ConversionBackend for ToolBackend {
    fn id(&self) -> &'static str {
        self.id
    }

    fn display_name(&self) -> &'static str {
        provenance(self.id).0
    }

    fn platform(&self) -> Platform {
        self.platform
    }

    fn validate_input(&self, input: &DetectionResult) -> Result<ValidationReport, Error> {
        validate_detection(input)
    }

    fn available_outputs(&self, input: &DetectionResult) -> Vec<TargetFormat> {
        FormatMatrix::get_rules_for_source(input.platform, input.source_format)
            .into_iter()
            .filter(|rule| rule.backend_id == self.id)
            .map(|rule| rule.target_format)
            .collect()
    }

    fn convert(
        &self,
        input: &DetectionResult,
        target: TargetFormat,
        output_dir: &Path,
        progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ConversionResult, Error> {
        let rule = FormatMatrix::find_rule(input.platform, input.source_format, target)
            .filter(|rule| rule.backend_id == self.id && rule.support_status.selectable())
            .context("the selected source/target direction is not implemented by this backend")?;
        ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
        fs::create_dir_all(output_dir)?;
        let (backend_name, upstream, built_in_version) = conversion_provenance(self.id, target);
        progress(ProgressUpdate {
            stage: rule.operation.to_owned(),
            percent: 1,
            details: Some(upstream.to_owned()),
            bytes_processed: 0,
            total_bytes: input_size(input),
        });

        let execution = execute(
            self.id,
            input,
            target,
            output_dir,
            cancelled,
            progress.clone(),
        )?;
        let output_path = execution.output_path;
        let validation = validate_path(target, &output_path)?;
        ensure!(
            validation.is_valid,
            "output validation failed: {}",
            validation.issues.join("; ")
        );
        let mut resulting_files = vec![output_path.clone()];
        resulting_files.extend(execution.aux_files.iter().cloned());
        let total_size = resulting_files
            .iter()
            .filter_map(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .sum();
        progress(ProgressUpdate {
            stage: "validated".to_owned(),
            percent: 100,
            details: Some(output_path.display().to_string()),
            bytes_processed: total_size,
            total_bytes: total_size,
        });
        let title_id = validation.game_id.or(input.game_id.clone());
        let title_name = validation.game_title.or(input.title_name.clone());
        Ok(ConversionResult {
            platform: input.platform,
            target_format: target,
            output_path,
            total_size,
            was_passthrough: rule.support_status == SupportStatus::PassThrough,
            title_id,
            title_name,
            aux_files: execution.aux_files,
            tool_report: Some(ToolReport {
                backend_name: backend_name.to_owned(),
                upstream_project: upstream.to_owned(),
                upstream_version: execution
                    .tool_version
                    .unwrap_or_else(|| built_in_version.to_owned()),
                operation: rule.operation.to_owned(),
                command: execution.command,
                stdout: execution.stdout,
                stderr: execution.stderr,
                exit_code: execution.exit_code,
                resulting_files,
                validation_result: "valid".to_owned(),
            }),
        })
    }

    fn validate_output(&self, result: &ConversionResult) -> Result<ValidationReport, Error> {
        validate_path(result.target_format, &result.output_path)
    }
}

struct Execution {
    output_path: PathBuf,
    aux_files: Vec<PathBuf>,
    command: Vec<String>,
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    tool_version: Option<String>,
}

fn provenance(id: &str) -> (&'static str, &'static str, &'static str) {
    match id {
        "ps1_psxpackager" => (
            "PSXPackager adapter",
            "RupertAvery/PSXPackager",
            "system tool",
        ),
        "ps1_chdman" | "ps2_chdman" | "optical_chdman" => {
            ("chdman adapter", "mamedev/mame", "system tool")
        }
        "nintendo_gc_wii" => (
            "DolphinTool DiscIO adapter",
            "dolphin-emu/dolphin",
            "system tool",
        ),
        "n64_normalize" => (
            "Nintendo 64 byte-order normalizer",
            "Digital Forge",
            env!("CARGO_PKG_VERSION"),
        ),
        "ps2_maxcso" | "psp_maxcso" => ("MaxCSO adapter", "unknownbrackets/maxcso", "system tool"),
        "ps2_usbextreme" => (
            "USBExtreme writer",
            "imadali120/PS2IsoManager",
            "source-backed port",
        ),
        "ps3_pkg" => (
            "PS3 PKG integration adapter",
            "Aldostools PS3 Tools Collection integration",
            "external tools; see setup diagnostics",
        ),
        "ps3_split_reassembly" => (
            "Sequential split reassembler",
            "Digital Forge",
            env!("CARGO_PKG_VERSION"),
        ),
        "ps4_pkg_merge" => ("pkg-merge adapter", "Tustin/pkg-merge", "system tool"),
        "ps4_liborbispkg" => (
            "PS4 package adapter",
            "OrbisPkgTool GP4 generator and CyB1K publisher",
            "CyB1K PS4 Fake PKG Tools 3.87 V7",
        ),
        "ps5_ufs2tool" => ("UFS2Tool adapter", "SvenGDK/UFS2Tool", "system tool"),
        "ps5_mkpfs" => ("MkPFS adapter", "PSBrew/MkPFS", "system Python module"),
        "ps5_anyps5" => (
            "pkg-to-anyps5 adapter",
            "awake-devel/pkg-to-anyps5",
            "external executable",
        ),
        "ps5_multipart" => (
            "PS5MPKG1 parser",
            "itsPLK/ps5-pkg-manager",
            "4096-byte header v1",
        ),
        "ps5_raw_split" => (
            "Raw split reassembler",
            "Digital Forge",
            env!("CARGO_PKG_VERSION"),
        ),
        "vita_pkg2zip" => ("pkg2zip adapter", "mmozeiko/pkg2zip", "system tool"),
        "vita_vpk" => ("VPK ZIP adapter", "7-Zip", "system tool"),
        "xbox_extract_xiso" | "xbox360_extract_xiso" => (
            "extract-xiso adapter",
            "XboxDev/extract-xiso",
            "system tool",
        ),
        "xbox360_iso2god" => ("Iso2God adapter", "r4dius/Iso2God", "system tool"),
        "xbox360_god2iso" => ("God2Iso adapter", "raburton/god2iso", "system tool"),
        "xbox360_split" => (
            "Raw split engine",
            "Digital Forge",
            env!("CARGO_PKG_VERSION"),
        ),
        "native_passthrough" => ("Validated copy", "Digital Forge", env!("CARGO_PKG_VERSION")),
        id if id.starts_with("passthrough_") => {
            ("Validated copy", "Digital Forge", env!("CARGO_PKG_VERSION"))
        }
        _ => (
            "Validation adapter",
            "Digital Forge",
            env!("CARGO_PKG_VERSION"),
        ),
    }
}

fn conversion_provenance(
    id: &str,
    target: TargetFormat,
) -> (&'static str, &'static str, &'static str) {
    if id == "ps4_liborbispkg"
        && (target == TargetFormat::Ps4FakePkg || target == TargetFormat::Ps4RetailPkg)
    {
        return (
            "Digital Forge PS4 PKG validator",
            "Digital Forge built-in validator",
            env!("CARGO_PKG_VERSION"),
        );
    }
    provenance(id)
}

fn execute(
    id: &str,
    input: &DetectionResult,
    target: TargetFormat,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
    progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
) -> Result<Execution, Error> {
    let source = input.related_files.first().context("no source path")?;
    match id {
        "native_passthrough"
            if input.platform == Platform::PlayStation5
                && target == TargetFormat::Ps5LooseFolder =>
        {
            run_ps5_itemzflow_folder(input, output_dir, cancelled, progress)
        }
        "native_passthrough" => reference_input(input),
        id if id.starts_with("passthrough_") => reference_input(input),
        "n64_normalize" => normalize_n64(source, input.source_format, output_dir, cancelled),
        "optical_chdman" => run_optical_chdman(source, input.source_format, target, output_dir),
        "nintendo_gc_wii" => run_dolphin_tool(source, target, output_dir),
        "ps1_psxpackager" => {
            let output = if target == TargetFormat::Pbp {
                output_dir.join("EBOOT.PBP")
            } else {
                output_dir.join("disc.cue")
            };
            run_tool(
                "PSXPACKAGER",
                &["psxpackager", "PSXPackager"],
                [
                    os("-i"),
                    source.as_os_str().to_owned(),
                    os("-o"),
                    output.as_os_str().to_owned(),
                ],
                output,
                Vec::new(),
            )
        }
        "ps1_chdman" => {
            if target == TargetFormat::Chd {
                let output = output_dir.join("disc.chd");
                run_tool(
                    "CHDMAN",
                    &["chdman"],
                    [
                        os("createcd"),
                        os("-i"),
                        source.as_os_str().to_owned(),
                        os("-o"),
                        output.as_os_str().to_owned(),
                    ],
                    output,
                    Vec::new(),
                )
            } else {
                let cue = output_dir.join("disc.cue");
                let bin = output_dir.join("disc.bin");
                run_tool(
                    "CHDMAN",
                    &["chdman"],
                    [
                        os("extractcd"),
                        os("-i"),
                        source.as_os_str().to_owned(),
                        os("-o"),
                        cue.as_os_str().to_owned(),
                        os("-ob"),
                        bin.as_os_str().to_owned(),
                    ],
                    cue,
                    vec![bin],
                )
            }
        }
        "ps2_maxcso" | "psp_maxcso" => run_maxcso(source, target, output_dir),
        "ps2_chdman" => run_ps2_chdman(source, target, output_dir),
        "ps2_usbextreme" => write_usbextreme(input, output_dir, cancelled),
        "ps3_pkg" => run_ps3_pkg(input, target, output_dir, cancelled, progress),
        "ps3_split_reassembly" | "ps5_raw_split" | "xbox360_split" if is_merge_target(target) => {
            merge_raw_parts(&input.related_files, output_dir, target, cancelled)
        }
        "xbox360_split" => split_raw(source, output_dir, target, cancelled),
        "ps4_pkg_merge" => {
            let output = output_dir.join("merged.pkg");
            let mut merged = run_tool(
                "PKG_MERGE",
                &["pkg-merge", "pkg_merge"],
                [source.as_os_str().to_owned(), output.as_os_str().to_owned()],
                output.clone(),
                Vec::new(),
            )?;
            let verified = run_tool(
                "PKGTOOL",
                &["PkgTool", "pkgtool"],
                [os("pkg_validate"), output.as_os_str().to_owned()],
                output,
                Vec::new(),
            )?;
            merged.command.extend(verified.command);
            merged.stdout.push_str(&verified.stdout);
            merged.stderr.push_str(&verified.stderr);
            Ok(merged)
        }
        "ps4_liborbispkg" if target == TargetFormat::Ps4RetailPkg => {
            ensure!(
                input.source_format == SourceFormat::Ps4RetailPkg,
                "Digital Forge cannot create Sony-signed retail PKGs; retail output only supports validated unchanged retail-package transfer"
            );
            reference_input(input)
        }
        "ps4_liborbispkg" if target == TargetFormat::Ps4FakePkg => {
            if input.source_format == SourceFormat::Ps4Folder {
                run_pkgtool_build_fpkg(
                    source,
                    output_dir,
                    cancelled,
                    progress,
                    input.game_id.as_deref(),
                )
            } else if input.source_format == SourceFormat::Ps4FakePkg {
                reference_input(input)
            } else {
                bail!(
                    "retail-to-FPKG repacking requires decrypted content and a valid package passcode; unchanged reference is not a conversion"
                )
            }
        }
        "ps4_liborbispkg" => run_pkgtool(
            source,
            target,
            output_dir,
            cancelled,
            progress,
            input.game_id.as_deref(),
        ),
        "ps5_ufs2tool" => run_ufs2tool(source, output_dir),
        "ps5_anyps5" => run_anyps5(source, input.source_format, output_dir, cancelled),
        "ps5_mkpfs" => run_mkpfs(source, target, output_dir),
        "ps5_multipart" => merge_ps5_multipart(&input.related_files, output_dir, cancelled),
        "vita_pkg2zip" => run_pkg2zip(source, target, output_dir),
        "vita_vpk" => run_vpk_extract(source, output_dir),
        "xbox_extract_xiso" | "xbox360_extract_xiso" => {
            run_extract_xiso(source, target, output_dir)
        }
        "xbox360_iso2god" => run_tool(
            "ISO2GOD",
            &["Iso2God", "iso2god-cli"],
            [
                source.as_os_str().to_owned(),
                output_dir.as_os_str().to_owned(),
            ],
            output_dir.to_path_buf(),
            Vec::new(),
        ),
        "xbox360_god2iso" => {
            let output = output_dir.join("game.iso");
            run_tool(
                "GOD2ISO",
                &["god2iso"],
                [source.as_os_str().to_owned(), output.as_os_str().to_owned()],
                output,
                Vec::new(),
            )
        }
        _ => bail!("backend {id} has no conversion implementation"),
    }
}

fn normalize_n64(
    source: &Path,
    source_format: SourceFormat,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<Execution, Error> {
    let mut bytes = fs::read(source).with_context(|| format!("reading {}", source.display()))?;
    ensure!(
        bytes.len() >= 4 && bytes.len() % 4 == 0,
        "N64 ROM length must be a multiple of four bytes"
    );
    ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
    match source_format {
        SourceFormat::N64V64 => {
            for pair in bytes.chunks_exact_mut(2) {
                pair.swap(0, 1);
            }
        }
        SourceFormat::N64N64 => {
            for word in bytes.chunks_exact_mut(4) {
                word.reverse();
            }
        }
        SourceFormat::N64Z64 => {}
        _ => bail!("unsupported Nintendo 64 source format"),
    }
    ensure!(
        bytes.starts_with(&[0x80, 0x37, 0x12, 0x40]),
        "byte-order conversion did not produce Z64 magic"
    );
    let output = output_dir.join("game.z64");
    fs::write(&output, bytes)?;
    Ok(local_execution(
        output,
        Vec::new(),
        "Digital Forge N64 byte-order normalization",
    ))
}

fn run_optical_chdman(
    source: &Path,
    source_format: SourceFormat,
    target: TargetFormat,
    output_dir: &Path,
) -> Result<Execution, Error> {
    if target == TargetFormat::Chd {
        let output = output_dir.join("disc.chd");
        return run_tool(
            "CHDMAN",
            &["chdman"],
            [
                os("createcd"),
                os("-i"),
                source.as_os_str().to_owned(),
                os("-o"),
                output.as_os_str().to_owned(),
            ],
            output,
            Vec::new(),
        );
    }
    ensure!(
        source_format == SourceFormat::Chd,
        "extractcd requires a CD CHD source"
    );
    if target == TargetFormat::Gdi {
        let output = output_dir.join("disc.gdi");
        run_tool(
            "CHDMAN",
            &["chdman"],
            [
                os("extractcd"),
                os("-i"),
                source.as_os_str().to_owned(),
                os("-o"),
                output.as_os_str().to_owned(),
            ],
            output,
            Vec::new(),
        )
    } else {
        let cue = output_dir.join("disc.cue");
        let bin = output_dir.join("disc.bin");
        run_tool(
            "CHDMAN",
            &["chdman"],
            [
                os("extractcd"),
                os("-i"),
                source.as_os_str().to_owned(),
                os("-o"),
                cue.as_os_str().to_owned(),
                os("-ob"),
                bin.as_os_str().to_owned(),
            ],
            cue,
            vec![bin],
        )
    }
}

fn run_dolphin_tool(
    source: &Path,
    target: TargetFormat,
    output_dir: &Path,
) -> Result<Execution, Error> {
    let (format, extension) = match target {
        TargetFormat::NintendoDiscIso => ("iso", "iso"),
        TargetFormat::NintendoGcz => ("gcz", "gcz"),
        TargetFormat::NintendoWia => ("wia", "wia"),
        TargetFormat::NintendoRvz => ("rvz", "rvz"),
        _ => bail!("unsupported DolphinTool output target"),
    };
    let output = output_dir.join(format!("disc.{extension}"));
    let mut args = vec![
        os("convert"),
        os("--input"),
        source.as_os_str().to_owned(),
        os("--output"),
        output.as_os_str().to_owned(),
        os("--format"),
        os(format),
    ];
    if format != "iso" {
        args.extend([os("--block_size"), os("131072")]);
    }
    if matches!(format, "wia" | "rvz") {
        args.extend([
            os("--compression"),
            os(if format == "rvz" { "zstd" } else { "lzma2" }),
            os("--compression_level"),
            os("5"),
        ]);
    }
    run_tool(
        "DOLPHIN_TOOL",
        &["DolphinTool", "dolphin-tool"],
        args,
        output,
        Vec::new(),
    )
}

fn run_maxcso(source: &Path, target: TargetFormat, output_dir: &Path) -> Result<Execution, Error> {
    let (format, extension) = match target {
        TargetFormat::Ps2Cso | TargetFormat::PspCso => (Some("cso1"), "cso"),
        TargetFormat::Ps2Cso2 | TargetFormat::PspCso2 => (Some("cso2"), "cso"),
        TargetFormat::Ps2Zso | TargetFormat::PspZso => (Some("zso"), "zso"),
        TargetFormat::Ps2Dax | TargetFormat::PspDax => (Some("dax"), "dax"),
        TargetFormat::Ps2Iso | TargetFormat::PspIso => (None, "iso"),
        _ => bail!("invalid MaxCSO target"),
    };
    let output = output_dir.join(format!("game.{extension}"));
    let mut args = Vec::new();
    if let Some(format) = format {
        args.push(os(format!("--format={format}")));
    } else {
        args.push(os("--decompress"));
    }
    args.extend([
        source.as_os_str().to_owned(),
        os("-o"),
        output.as_os_str().to_owned(),
    ]);
    run_tool("MAXCSO", &["maxcso"], args, output, Vec::new())
}

fn run_ps2_chdman(
    source: &Path,
    target: TargetFormat,
    output_dir: &Path,
) -> Result<Execution, Error> {
    if target == TargetFormat::Ps2Chd {
        let output = output_dir.join("game.chd");
        run_tool(
            "CHDMAN",
            &["chdman"],
            [
                os("createdvd"),
                os("-i"),
                source.as_os_str().to_owned(),
                os("-o"),
                output.as_os_str().to_owned(),
            ],
            output,
            Vec::new(),
        )
    } else {
        let output = output_dir.join("game.iso");
        run_tool(
            "CHDMAN",
            &["chdman"],
            [
                os("extractdvd"),
                os("-i"),
                source.as_os_str().to_owned(),
                os("-o"),
                output.as_os_str().to_owned(),
            ],
            output,
            Vec::new(),
        )
    }
}

fn run_ps3_pkg(
    input: &DetectionResult,
    target: TargetFormat,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
    _progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
) -> Result<Execution, Error> {
    let _source = input.related_files.first().context("no PS3 source path")?;
    if matches!(
        (input.source_format, target),
        (SourceFormat::Ps3Iso, TargetFormat::Ps3JbFolder)
            | (SourceFormat::Ps3SplitIso, TargetFormat::Ps3JbFolder)
            | (SourceFormat::Ps3JbFolder, TargetFormat::Ps3Iso)
    ) {
        return run_ps3_iso_workflow(input, target, output_dir, cancelled);
    }
    if input.source_format == SourceFormat::Ps3Pkg && target == TargetFormat::Ps3Pkg {
        return reference_input(input);
    }
    bail!(
        "PS3 PKG creation is unavailable until a verified Aldostools package builder is configured; the retired PS3DK path is no longer used"
    )
}
fn run_ps3_iso_workflow(
    input: &DetectionResult,
    target: TargetFormat,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<Execution, Error> {
    ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
    let source = input.related_files.first().context("no PS3 source path")?;
    match (input.source_format, target) {
        (SourceFormat::Ps3Iso, TargetFormat::Ps3JbFolder) => {
            let candidates = ["extractps3iso"];
            let output = output_dir.join("game");
            run_tool(
                "PS3_EXTRACT_ISO",
                &candidates,
                [source.as_os_str().to_owned(), output.as_os_str().to_owned()],
                output,
                Vec::new(),
            )
        }
        (SourceFormat::Ps3SplitIso, TargetFormat::Ps3JbFolder) => {
            let iso = output_dir.join("reassembled.iso");
            let mut out = File::create(&iso)?;
            for part in validate_contiguous_parts(&input.related_files)? {
                ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
                std::io::copy(&mut File::open(part)?, &mut out)?;
            }
            out.flush()?;
            let candidates = ["extractps3iso"];
            let output = output_dir.join("game");
            run_tool(
                "PS3_EXTRACT_ISO",
                &candidates,
                [iso.as_os_str().to_owned(), output.as_os_str().to_owned()],
                output,
                Vec::new(),
            )
        }
        (SourceFormat::Ps3JbFolder, TargetFormat::Ps3Iso) => {
            let iso = output_dir.join("game.iso");
            let candidates = ["genps3iso"];
            run_tool(
                "PS3_GENERATE_ISO",
                &candidates,
                [source.as_os_str().to_owned(), iso.as_os_str().to_owned()],
                iso,
                Vec::new(),
            )
        }
        _ => bail!("unsupported PS3 ISO workflow"),
    }
}

fn valid_ps3_content_id(value: &str) -> bool {
    value.len() == 36
        // PS3 Content IDs use a six-character publisher prefix, e.g.
        // `UP0001-BCUS98181_00-...`.
        && value.as_bytes().get(6) == Some(&b'-')
        && value.contains('_')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Extract a finalized PS3 PKG with the bundled established extractor. This
/// is also used by the folder workflow before a bounded base/update merge.
pub fn extract_ps3_pkg_for_workflow(package: &Path, output_dir: &Path) -> Result<(), Error> {
    fs::create_dir_all(output_dir)?;
    let (python, extractor) = bundled_ps3_extractor()
        .context("PS3-PKG-Module and its bundled Python runtime are unavailable")?;
    let result = Command::new(&python)
        .arg(&extractor)
        .arg(package)
        .arg("-o")
        .arg(output_dir)
        .output()
        .with_context(|| format!("failed to launch {}", extractor.display()))?;
    ensure!(
        result.status.success(),
        "PS3-PKG-Module extraction failed: {}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

fn bundled_ps3_extractor() -> Option<(PathBuf, PathBuf)> {
    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut candidates = Vec::new();
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("runtime/ps3"));
    }
    candidates.push(source_root.join("vendor"));

    let configured_python = env::var_os("DIGITAL_FORGE_PS3_PYTHON").map(PathBuf::from);
    let configured_extractor = env::var_os("DIGITAL_FORGE_PS3_EXTRACTOR").map(PathBuf::from);
    for root in candidates {
        let python = configured_python
            .clone()
            .unwrap_or_else(|| root.join("ps3-update-dl/python311/python.exe"));
        let extractor = configured_extractor
            .clone()
            .unwrap_or_else(|| root.join("ps3-pkg-module/main.py"));
        if python.is_file() && extractor.is_file() {
            return Some((python, extractor));
        }
    }
    None
}

fn run_pkgtool(
    source: &Path,
    target: TargetFormat,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
    progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
    title_id: Option<&str>,
) -> Result<Execution, Error> {
    if target == TargetFormat::Ps4FakePkg || target == TargetFormat::Ps4RetailPkg {
        Ok(local_execution(
            source.to_path_buf(),
            Vec::new(),
            "built-in PS4 PKG validation",
        ))
    } else {
        let folder_name = if let Some(id) = title_id {
            id.to_string()
        } else {
            let mut name = "game".to_string();
            if let Ok(mut f) = File::open(source) {
                let mut hdr = [0u8; 4096];
                if let Ok(n) = f.read(&mut hdr) {
                    if let Some(cid) = crate::forge::detector::parse_pkg_content_id(&hdr[..n]) {
                        if let Some(tid) = crate::forge::detector::title_id_from_content_id(&cid) {
                            name = tid;
                        }
                    }
                }
            }
            if name == "game" {
                if let Some(stem) = source.file_stem().and_then(OsStr::to_str) {
                    name = stem.to_string();
                }
            }
            name
        };

        let folder = output_dir.join(&folder_name);
        fs::create_dir_all(&folder)?;

        let tool = find_tool(
            "PKGTOOL",
            &["OrbisPkgTool", "orbis-pkg-tool", "PkgTool", "pkgtool"],
        )
        .context("neither OrbisPkgTool nor PkgTool was found")?;

        let is_orbis_pkg_tool = tool
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.to_ascii_lowercase().contains("orbispkgtool"));

        let exec = if is_orbis_pkg_tool {
            let mut cmd = Command::new(&tool);
            cmd.arg("extract");
            cmd.arg("--verbose");
            cmd.arg(source);
            cmd.arg(&folder);
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            let mut child = cmd
                .spawn()
                .with_context(|| format!("failed to launch {}", tool.display()))?;
            let stdout = child.stdout.take().context("missing stdout")?;
            let stderr = child.stderr.take().context("missing stderr")?;

            let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
            let stderr_handle = std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut err_text = String::new();
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line) {
                    if n == 0 {
                        break;
                    }
                    err_text.push_str(&line);
                    line.clear();
                }
                let _ = stderr_tx.send(err_text);
            });

            let mut reader = BufReader::new(stdout);
            let mut stdout_text = String::new();
            let mut record = Vec::new();

            loop {
                if cancelled.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    bail!("conversion cancelled");
                }
                record.clear();
                let count = match reader.read_until(b'\r', &mut record) {
                    Ok(c) => c,
                    Err(_) => break,
                };
                if count == 0 {
                    break;
                }
                let line_text = String::from_utf8_lossy(&record);
                let trimmed = line_text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                stdout_text.push_str(trimmed);
                stdout_text.push('\n');

                let pct = if let Some(open) = trimmed.find('[') {
                    if let Some(pct_pos) = trimmed[open..].find('%') {
                        let num_str = trimmed[open + 1..open + pct_pos].trim();
                        num_str.parse::<u8>().ok()
                    } else {
                        None
                    }
                } else {
                    None
                };

                let display_detail = if let Some(close) = trimmed.find(']') {
                    let after = trimmed[close + 1..].trim();
                    after.strip_prefix("Extracting ").unwrap_or(after).trim()
                } else {
                    trimmed
                };

                let reported_pct = if let Some(p) = pct {
                    (2 + (p as f32 * 0.93) as u8).min(95)
                } else {
                    5
                };

                progress(ProgressUpdate {
                    stage: "Extracting PKG files".to_string(),
                    percent: reported_pct,
                    details: Some(display_detail.to_string()),
                    bytes_processed: 0,
                    total_bytes: 0,
                });
            }

            let status = child.wait()?;
            let _ = stderr_handle.join();
            let stderr_text = stderr_rx.recv().unwrap_or_default();

            ensure!(
                status.success(),
                "OrbisPkgTool extract failed with exit code {:?}:\n{}",
                status.code(),
                stderr_text.trim()
            );

            progress(ProgressUpdate {
                stage: "Restructuring PS4 game layout".to_string(),
                percent: 96,
                details: None,
                bytes_processed: 0,
                total_bytes: 0,
            });

            let _ = Command::new(&tool).arg("restructure").arg(&folder).output();

            // Flatten Image0 if present so game assets (eboot.bin, sce_sys, data, etc.) are at folder root
            let image0 = folder.join("Image0");
            if image0.is_dir() {
                if let Ok(entries) = fs::read_dir(&image0) {
                    for entry in entries.flatten() {
                        let target_path = folder.join(entry.file_name());
                        if !target_path.exists() {
                            let _ = fs::rename(entry.path(), &target_path);
                        } else if entry.path().is_dir() && target_path.is_dir() {
                            if let Ok(sub_entries) = fs::read_dir(entry.path()) {
                                for sub_entry in sub_entries.flatten() {
                                    let sub_target = target_path.join(sub_entry.file_name());
                                    if !sub_target.exists() {
                                        let _ = fs::rename(sub_entry.path(), &sub_target);
                                    }
                                }
                            }
                            let _ = fs::remove_dir_all(entry.path());
                        }
                    }
                }
                let _ = fs::remove_dir_all(&image0);
            }
            let sc0 = folder.join("Sc0");
            if sc0.is_dir() {
                let sce_sys = folder.join("sce_sys");
                let _ = fs::create_dir_all(&sce_sys);
                if let Ok(entries) = fs::read_dir(&sc0) {
                    for entry in entries.flatten() {
                        let target_path = sce_sys.join(entry.file_name());
                        if !target_path.exists() {
                            let _ = fs::rename(entry.path(), &target_path);
                        } else if entry.path().is_dir() && target_path.is_dir() {
                            if let Ok(sub_entries) = fs::read_dir(entry.path()) {
                                for sub_entry in sub_entries.flatten() {
                                    let sub_target = target_path.join(sub_entry.file_name());
                                    if !sub_target.exists() {
                                        let _ = fs::rename(sub_entry.path(), &sub_target);
                                    }
                                }
                            }
                            let _ = fs::remove_dir_all(entry.path());
                        }
                    }
                }
                let _ = fs::remove_dir_all(sc0);
            }

            // If real Title ID was detected in SFO, ensure the final directory is named after it
            let final_folder = {
                let sfo_path = folder.join("sce_sys").join("param.sfo");
                if sfo_path.is_file() {
                    let (sfo_title_id, _) =
                        crate::forge::detector::FormatDetector::read_sfo(&sfo_path);
                    if let Some(id) = sfo_title_id {
                        if !id.is_empty() && id != folder_name {
                            let new_path = output_dir.join(&id);
                            if !new_path.exists() && fs::rename(&folder, &new_path).is_ok() {
                                new_path
                            } else {
                                folder
                            }
                        } else {
                            folder
                        }
                    } else {
                        folder
                    }
                } else {
                    folder
                }
            };

            Execution {
                output_path: final_folder.clone(),
                aux_files: Vec::new(),
                command: vec![
                    tool.display().to_string(),
                    "extract".to_string(),
                    source.display().to_string(),
                    final_folder.display().to_string(),
                ],
                stdout: stdout_text,
                stderr: stderr_text,
                exit_code: status.code(),
                tool_version: Some("OrbisPkgTool".to_string()),
            }
        } else {
            let gp4 = folder.join("package.gp4");
            let outer = folder.join("outer.pfs.dat");
            let inner = folder.join("inner.pfs.dat");
            let first = run_tool(
                "PKGTOOL",
                &["PkgTool", "pkgtool"],
                [
                    os("pkg_makegp4"),
                    source.as_os_str().to_owned(),
                    folder.as_os_str().to_owned(),
                ],
                gp4.clone(),
                vec![outer.clone(), inner.clone()],
            )?;
            run_tool(
                "PKGTOOL",
                &["PkgTool", "pkgtool"],
                [
                    os("pkg_extractouterpfs"),
                    source.as_os_str().to_owned(),
                    outer.as_os_str().to_owned(),
                ],
                outer.clone(),
                Vec::new(),
            )?;
            run_tool(
                "PKGTOOL",
                &["PkgTool", "pkgtool"],
                [
                    os("pkg_extractinnerpfs"),
                    source.as_os_str().to_owned(),
                    inner.as_os_str().to_owned(),
                ],
                inner,
                Vec::new(),
            )
            .with_context(
                || "LibOrbisPkg could not extract inner PFS; a package passcode may be required",
            )?;
            Execution {
                output_path: folder,
                aux_files: vec![gp4, outer],
                ..first
            }
        };
        Ok(exec)
    }
}

fn run_pkgtool_build_fpkg(
    source_dir: &Path,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
    progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
    title_id: Option<&str>,
) -> Result<Execution, Error> {
    let native_tool = find_native_orbis_pkg_tool();
    let tool = native_tool
        .clone()
        .or_else(find_orbis_pkg_tool)
        .or_else(|| find_tool("PKGTOOL", &["OrbisPkgTool", "orbis-pkg-tool"]))
        .context("Digital Forge GP4 generator was not found")?;
    let publisher = if native_tool.is_none() {
        Some(find_orbis_pub_cmd().context(
            "PS4 publishing tools are not configured and OrbisPkgTool is unavailable. Install the verified OrbisPkgTool runtime or set DIGITAL_FORGE_PS4_TOOLS / DIGITAL_FORGE_TOOL_ORBIS_PUB_CMD",
        )?)
    } else {
        None
    };

    // Packaging must never rewrite the caller's source tree in place.
    let sfo_path = source_dir.join("sce_sys").join("param.sfo");
    let eboot_path = source_dir.join("eboot.bin");
    ensure!(source_dir.is_dir(), "PS4 game folder does not exist");
    ensure!(
        sfo_path.is_file(),
        "PS4 game folder is missing sce_sys/param.sfo"
    );
    ensure!(eboot_path.is_file(), "PS4 game folder is missing eboot.bin");
    let category = crate::forge::detector::FormatDetector::read_sfo_category(&sfo_path)
        .context("PS4 param.sfo has no readable CATEGORY value")?;
    ensure!(
        !category.eq_ignore_ascii_case("ac"),
        "add-on content (CATEGORY=ac) cannot be packaged as a base game or update"
    );
    let is_patch = category.eq_ignore_ascii_case("gp");

    let sfo_id = crate::forge::detector::FormatDetector::read_sfo_key(&sfo_path, "TITLE_ID");
    let resolved_title_id = title_id.map(String::from).or(sfo_id);
    if let (Some(requested), Some(found)) = (title_id, resolved_title_id.as_deref()) {
        ensure!(
            requested.eq_ignore_ascii_case(found),
            "selected PS4 Title ID {requested} does not match param.sfo Title ID {found}"
        );
    }
    let pkg_name = format!("{}.pkg", resolved_title_id.as_deref().unwrap_or("game"));
    let output_pkg = output_dir.join(&pkg_name);
    let gp4_path = output_dir.join("project.gp4");

    progress(ProgressUpdate {
        stage: if is_patch {
            "Generating update GP4 project"
        } else {
            "Generating base-game GP4 project"
        }
        .to_string(),
        percent: 10,
        details: Some(
            if is_patch {
                "Using patch project settings"
            } else {
                "Using application project settings"
            }
            .to_string(),
        ),
        bytes_processed: 0,
        total_bytes: 0,
    });

    let mut gp4_command = Command::new(&tool);
    gp4_command.arg("gp4gen");
    if is_patch {
        gp4_command.arg("--patch");
    }
    let gp4_output = gp4_command
        .arg(source_dir)
        .arg("--out")
        .arg(&gp4_path)
        .output()
        .with_context(|| format!("failed to launch gp4gen with {}", tool.display()))?;
    ensure!(
        gp4_output.status.success(),
        "{} GP4 generation failed: {}{}",
        if is_patch { "Update" } else { "Base-game" },
        String::from_utf8_lossy(&gp4_output.stdout),
        String::from_utf8_lossy(&gp4_output.stderr)
    );
    ensure!(
        gp4_path.is_file(),
        "GP4 generator reported success without creating a project"
    );

    // Clean addcont references from Base Game application GP4
    if let Ok(gp4_text) = fs::read_to_string(&gp4_path) {
        let filtered = if gp4_text.contains("pkg_ps4_app") && gp4_text.contains("addcont") {
            gp4_text
                .lines()
                .filter(|line| {
                    !line.contains("targ_path=\"addcont") && !line.contains("targ_dir=\"addcont")
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            gp4_text
        };
        fs::write(&gp4_path, absolutize_gp4_orig_paths(&filtered, source_dir))?;
    }

    progress(ProgressUpdate {
        stage: if is_patch {
            "Building PS4 update FPKG"
        } else {
            "Building PS4 base-game FPKG"
        }
        .to_string(),
        percent: 30,
        details: Some(pkg_name.clone()),
        bytes_processed: 0,
        total_bytes: 0,
    });

    ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
    let (build, builder_name) = if let Some(publisher) = publisher {
        (
            Command::new(&publisher)
                .arg("img_create")
                .arg("--oformat")
                .arg("pkg")
                .arg("--passcode")
                .arg("00000000000000000000000000000000")
                .arg("--no_progress_bar")
                .arg(&gp4_path)
                .arg(output_dir)
                .output()
                .with_context(|| {
                    format!("failed to launch PS4 publisher {}", publisher.display())
                })?,
            publisher.display().to_string(),
        )
    } else {
        (
            Command::new(&tool)
                .arg("build")
                .arg(&gp4_path)
                .arg(source_dir)
                .arg("--out")
                .arg(&output_pkg)
                .arg("--validate")
                .output()
                .with_context(|| format!("failed to launch OrbisPkgTool {}", tool.display()))?,
            tool.display().to_string(),
        )
    };
    let stdout_text = String::from_utf8_lossy(&build.stdout).into_owned();
    let stderr_text = String::from_utf8_lossy(&build.stderr).into_owned();
    ensure!(
        build.status.success(),
        "PS4 FPKG builder failed: {stdout_text}{stderr_text}"
    );
    ensure!(!cancelled.load(Ordering::Relaxed), "conversion cancelled");
    let generated = fs::read_dir(output_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("pkg"))
                && path == &output_pkg
        })
        .or_else(|| {
            fs::read_dir(output_dir)
                .ok()?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    path.extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("pkg"))
                })
        })
        .context("PS4 FPKG builder finished but output PKG is missing")?;
    if generated != output_pkg {
        fs::rename(&generated, &output_pkg)?;
    }
    let verify_tool = find_native_orbis_pkg_tool()
        .or_else(find_orbis_pkg_tool)
        .or_else(|| find_tool("PKGTOOL", &["OrbisPkgTool", "orbis-pkg-tool"]))
        .context("Digital Forge package validator was not found")?;
    let mut verify_command = Command::new(&verify_tool);
    if verify_tool
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("OrbisPkgTool.exe"))
    {
        verify_command.arg("validate").arg("--fake-tolerant");
    } else {
        verify_command.arg("pkg_validate").arg("--verbose");
    }
    let verify = verify_command.arg(&output_pkg).output()?;
    let verify_text = format!(
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
    ensure!(
        verify.status.success() && !verify_text.contains("[ERROR]"),
        "Digital Forge package validation failed: {verify_text}"
    );
    let _ = fs::remove_file(&gp4_path);

    Ok(Execution {
        output_path: output_pkg,
        aux_files: Vec::new(),
        command: vec![
            builder_name,
            if native_tool.is_some() {
                "build"
            } else {
                "img_create"
            }
            .to_string(),
        ],
        stdout: stdout_text,
        stderr: stderr_text,
        exit_code: build.status.code(),
        tool_version: Some(
            if native_tool.is_some() {
                "OrbisPkgTool v1.0.0 (PC-validated; not hardware-tested)"
            } else {
                "CyB1K PS4 Fake PKG Tools 3.87 publisher"
            }
            .to_string(),
        ),
    })
}

fn absolutize_gp4_orig_paths(gp4: &str, source_dir: &Path) -> String {
    let mut out = String::with_capacity(gp4.len() + 1024);
    let mut rest = gp4;
    const MARKER: &str = "orig_path=\"";
    while let Some(start) = rest.find(MARKER) {
        let value_start = start + MARKER.len();
        out.push_str(&rest[..value_start]);
        let after = &rest[value_start..];
        let Some(end) = after.find('"') else {
            out.push_str(after);
            return out;
        };
        let value = &after[..end];
        let source = Path::new(value);
        let absolute = if source.is_absolute() {
            source.to_path_buf()
        } else {
            source_dir.join(source)
        };
        let escaped = absolute
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        out.push_str(&escaped);
        out.push('"');
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn run_ufs2tool(source: &Path, output_dir: &Path) -> Result<Execution, Error> {
    ensure!(
        source.join("eboot.bin").is_file(),
        "PS5 game root is missing eboot.bin"
    );
    let output = output_dir.join("game.ffpkg");
    run_tool(
        "UFS2TOOL",
        &["UFS2Tool", "ufs2tool"],
        [
            os("newfs"),
            os("-O"),
            os("2"),
            os("-b"),
            os("65536"),
            os("-f"),
            os("65536"),
            os("-m"),
            os("0"),
            os("-S"),
            os("4096"),
            os("-i"),
            os("262144"),
            os("-D"),
            source.as_os_str().to_owned(),
            output.as_os_str().to_owned(),
        ],
        output,
        Vec::new(),
    )
}

fn run_anyps5(
    source: &Path,
    source_format: SourceFormat,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<Execution, Error> {
    ensure!(
        matches!(
            source_format,
            SourceFormat::Ps5Pkg | SourceFormat::Ps5LooseFolder
        ),
        "AnyPS5 preparation accepts a PS5 debug FIH PKG or an extracted PS5 game folder"
    );
    if source_format == SourceFormat::Ps5Pkg {
        let mut file = File::open(source)
            .with_context(|| format!("could not read PS5 package {}", source.display()))?;
        let mut header = [0u8; 4];
        file.read_exact(&mut header)?;
        ensure!(
            &header == b"\x7FFIH",
            "AnyPS5 preparation supports debug FIH packages; encrypted retail PKGs are not supported"
        );
    }
    let repository_tool = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("consoles/PlayStation/AnyPS5/pkg-to-anyps5.exe");
    let tool = env::var_os("DIGITAL_FORGE_PKG_TO_ANYPS5")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| repository_tool.is_file().then_some(repository_tool))
        .or_else(|| {
            find_tool(
                "PS5_ANYPS5",
                &[
                    "runtime/ps5/AnyPS5/pkg-to-anyps5",
                    "../../consoles/PlayStation/AnyPS5/pkg-to-anyps5",
                    "../../../consoles/PlayStation/AnyPS5/pkg-to-anyps5",
                    "pkg-to-anyps5",
                ],
            )
        })
        .context("pkg-to-anyps5 is not installed; set DIGITAL_FORGE_PKG_TO_ANYPS5, place it in consoles/PlayStation/AnyPS5 for a development checkout, or put it under runtime/ps5/AnyPS5 beside the release executable")?;

    let output = output_dir.join("AnyPS5");
    ensure!(
        !output.exists(),
        "AnyPS5 destination already exists: {}",
        output.display()
    );
    let stage = output_dir.join(".anyps5-partial");
    ensure!(
        !stage.exists(),
        "a previous AnyPS5 preparation is still present"
    );
    fs::create_dir_all(&stage)?;
    let result = Command::new(&tool)
        .arg(source)
        .arg(&stage)
        .arg("--verbose")
        .output()
        .with_context(|| format!("failed to launch {}", tool.display()));
    let process = match result {
        Ok(process) => process,
        Err(error) => {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
    };
    if cancelled.load(Ordering::Relaxed) {
        let _ = fs::remove_dir_all(&stage);
        bail!("conversion cancelled");
    }
    let stdout = String::from_utf8_lossy(&process.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&process.stderr).into_owned();
    if !process.status.success() {
        let _ = fs::remove_dir_all(&stage);
        bail!(
            "pkg-to-anyps5 failed with exit code {:?}: {}{}",
            process.status.code(),
            stdout,
            stderr
        );
    }
    if let Err(error) = crate::forge::ps5::validate_anyps5_output(&stage) {
        let _ = fs::remove_dir_all(&stage);
        return Err(error.context("pkg-to-anyps5 output validation failed"));
    }
    fs::rename(&stage, &output)
        .with_context(|| format!("could not finalize AnyPS5 output {}", output.display()))?;
    Ok(Execution {
        output_path: output,
        aux_files: Vec::new(),
        command: vec![
            tool.display().to_string(),
            source.display().to_string(),
            "<output>/AnyPS5".to_owned(),
            "--verbose".to_owned(),
        ],
        stdout,
        stderr,
        exit_code: process.status.code(),
        tool_version: None,
    })
}

fn run_mkpfs(source: &Path, target: TargetFormat, output_dir: &Path) -> Result<Execution, Error> {
    let (args, output) = match target {
        TargetFormat::Ps5ExfatImage => (
            vec![
                os("-m"),
                os("mkpfs"),
                os("pack"),
                os("exfat"),
                source.as_os_str().to_owned(),
                output_dir.join("game.exfat").into_os_string(),
            ],
            output_dir.join("game.exfat"),
        ),
        TargetFormat::Ps5Ffpfsc if source.is_dir() => (
            vec![
                os("-m"),
                os("mkpfs"),
                os("pack"),
                os("folder"),
                os("--verify"),
                source.as_os_str().to_owned(),
                output_dir.join("game.ffpfsc").into_os_string(),
            ],
            output_dir.join("game.ffpfsc"),
        ),
        TargetFormat::Ps5Ffpfsc => (
            vec![
                os("-m"),
                os("mkpfs"),
                os("pack"),
                os("file"),
                source.as_os_str().to_owned(),
                output_dir.join("game.ffpfsc").into_os_string(),
            ],
            output_dir.join("game.ffpfsc"),
        ),
        TargetFormat::Ps5Ffpfs => (
            vec![
                os("-m"),
                os("mkpfs"),
                os("pack"),
                os("folder"),
                os("--raw"),
                source.as_os_str().to_owned(),
                output_dir.join("game.ffpfs").into_os_string(),
            ],
            output_dir.join("game.ffpfs"),
        ),
        TargetFormat::Ps5LooseFolder
            if source
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exfat")) =>
        {
            (
                vec![
                    os("-m"),
                    os("mkpfs"),
                    os("unpack"),
                    source.as_os_str().to_owned(),
                    output_dir.join("game").into_os_string(),
                    os("--format"),
                    os("exfat"),
                ],
                output_dir.join("game"),
            )
        }
        TargetFormat::Ps5LooseFolder => (
            vec![
                os("-m"),
                os("mkpfs"),
                os("unpack"),
                os("--deep"),
                source.as_os_str().to_owned(),
                output_dir.join("game").into_os_string(),
            ],
            output_dir.join("game"),
        ),
        _ => bail!("invalid MkPFS target"),
    };
    let mut result = run_tool(
        "PYTHON",
        &["python", "python3"],
        args,
        output.clone(),
        Vec::new(),
    )?;
    if matches!(target, TargetFormat::Ps5Ffpfsc | TargetFormat::Ps5Ffpfs) {
        let verified = run_tool(
            "PYTHON",
            &["python", "python3"],
            [
                os("-m"),
                os("mkpfs"),
                os("verify"),
                output.as_os_str().to_owned(),
            ],
            output,
            Vec::new(),
        )?;
        result.command.extend(verified.command);
        result.stdout.push_str(&verified.stdout);
        result.stderr.push_str(&verified.stderr);
    }
    Ok(result)
}

fn run_pkg2zip(source: &Path, target: TargetFormat, output_dir: &Path) -> Result<Execution, Error> {
    let mut args = Vec::new();
    if target == TargetFormat::VitaNoNpDrmFolder {
        args.push(os("-x"));
    }
    args.push(source.as_os_str().to_owned());
    if let Some(zrif) = env::var_os("DIGITAL_FORGE_ZRIF") {
        args.push(zrif);
    }
    let output = if target == TargetFormat::VitaZip {
        output_dir.join("package.zip")
    } else {
        output_dir.to_path_buf()
    };
    let mut result = run_tool_in(
        "PKG2ZIP",
        &["pkg2zip"],
        args,
        output.clone(),
        Vec::new(),
        output_dir,
    )?;
    if target == TargetFormat::VitaZip && !output.exists() {
        let generated = fs::read_dir(output_dir)?
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
            })
            .context("pkg2zip did not create a ZIP")?;
        result.output_path = generated;
    }
    Ok(result)
}

fn run_vpk_extract(source: &Path, output_dir: &Path) -> Result<Execution, Error> {
    let output = output_dir.join("vpk");
    fs::create_dir_all(&output)?;
    run_tool(
        "SEVEN_ZIP",
        &["7z", "7za"],
        [
            os("x"),
            os("-y"),
            os(format!("-o{}", output.display())),
            source.as_os_str().to_owned(),
        ],
        output,
        Vec::new(),
    )
}

fn run_extract_xiso(
    source: &Path,
    target: TargetFormat,
    output_dir: &Path,
) -> Result<Execution, Error> {
    match target {
        TargetFormat::XboxOriginalXiso => {
            let output = output_dir.join("game.iso");
            run_tool(
                "EXTRACT_XISO",
                &["extract-xiso"],
                [
                    os("-c"),
                    source.as_os_str().to_owned(),
                    output.as_os_str().to_owned(),
                ],
                output,
                Vec::new(),
            )
        }
        TargetFormat::XboxOriginalFolder | TargetFormat::Xbox360Folder => {
            let output = output_dir.join("game");
            run_tool(
                "EXTRACT_XISO",
                &["extract-xiso"],
                [
                    os("-x"),
                    os("-d"),
                    output.as_os_str().to_owned(),
                    source.as_os_str().to_owned(),
                ],
                output,
                Vec::new(),
            )
        }
        TargetFormat::XboxOriginalTrimmedXiso => {
            let output = output_dir.join("game-rewritten.iso");
            fs::copy(source, &output)?;
            run_tool(
                "EXTRACT_XISO",
                &["extract-xiso"],
                [os("-r"), output.as_os_str().to_owned()],
                output,
                Vec::new(),
            )
        }
        _ => bail!("invalid extract-xiso target"),
    }
}

fn run_tool<I>(
    env_name: &str,
    candidates: &[&str],
    args: I,
    output: PathBuf,
    aux: Vec<PathBuf>,
) -> Result<Execution, Error>
where
    I: IntoIterator<Item = OsString>,
{
    run_tool_in(env_name, candidates, args, output, aux, Path::new("."))
}

fn run_tool_in<I>(
    env_name: &str,
    candidates: &[&str],
    args: I,
    output: PathBuf,
    aux: Vec<PathBuf>,
    current_dir: &Path,
) -> Result<Execution, Error>
where
    I: IntoIterator<Item = OsString>,
{
    let tool = find_tool(env_name, candidates).with_context(|| {
        format!("required tool {env_name} was not found; set DIGITAL_FORGE_TOOL_{env_name}")
    })?;
    let args: Vec<OsString> = args.into_iter().collect();
    let command_text = std::iter::once(tool.display().to_string())
        .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
        .collect::<Vec<_>>();
    let process = Command::new(&tool)
        .args(&args)
        .current_dir(current_dir)
        .output()
        .with_context(|| format!("failed to launch {}", tool.display()))?;
    let stdout = String::from_utf8_lossy(&process.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&process.stderr).into_owned();
    ensure!(
        process.status.success(),
        "{} failed with exit code {:?}: {}",
        tool.display(),
        process.status.code(),
        stderr.trim()
    );
    ensure!(
        output.exists(),
        "tool succeeded but expected output is missing: {}",
        output.display()
    );
    let tool_version = Command::new(&tool)
        .arg("--version")
        .output()
        .ok()
        .and_then(|version| {
            let text = if version.stdout.is_empty() {
                String::from_utf8_lossy(&version.stderr).into_owned()
            } else {
                String::from_utf8_lossy(&version.stdout).into_owned()
            };
            text.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(str::to_owned)
        });
    Ok(Execution {
        output_path: output,
        aux_files: aux.into_iter().filter(|path| path.exists()).collect(),
        command: command_text,
        stdout,
        stderr,
        exit_code: process.status.code(),
        tool_version,
    })
}

/// Resolve locally supplied PS4 publishing tools without baking third-party
/// executables into Digital Forge or making a clean source checkout depend on
/// files outside the repository.
fn ps4_tools_directory() -> Option<PathBuf> {
    if let Some(configured) = env::var_os("DIGITAL_FORGE_PS4_TOOLS") {
        let configured = PathBuf::from(configured);
        if configured.is_dir() {
            return Some(configured);
        }
    }

    let mut candidates = Vec::new();
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("runtime/ps4/FPKG/tools/DigitalForge"));
        candidates.push(directory.join("..\\..\\consoles/PlayStation/FPKG/tools/DigitalForge"));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..\\consoles/PlayStation/FPKG/tools/DigitalForge"),
    );
    candidates.into_iter().find(|path| path.is_dir())
}

pub fn find_orbis_pkg_tool() -> Option<PathBuf> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        let tools = ps4_tools_directory()?;
        let configured = env::var_os("DIGITAL_FORGE_TOOL_ORBIS_PKG_TOOL").map(PathBuf::from);
        let tool = configured.unwrap_or_else(|| tools.join("OrbisPkgTool-fixed.exe"));
        return tool.is_file().then_some(tool);
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        None
    }
}

/// Resolves the maintained OrbisPkgTool CLI used for native GP4 and FPKG
/// creation. Kept separate from the legacy LibOrbisPkg adapter because its
/// command-line verbs differ.
pub fn find_native_orbis_pkg_tool() -> Option<PathBuf> {
    let configured = env::var_os("DIGITAL_FORGE_NATIVE_ORBIS_PKG_TOOL").map(PathBuf::from);
    let mut candidates = configured.into_iter().collect::<Vec<_>>();
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("runtime/ps4/OrbisPkgTool/OrbisPkgTool.exe"));
        candidates.push(directory.join("OrbisPkgTool/OrbisPkgTool.exe"));
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../consoles/PlayStation/FPKG/tools/OrbisPkgTool/OrbisPkgTool.exe"),
    );
    candidates.into_iter().find(|path| path.is_file())
}

/// Resolves the configured LibOrbisPkg tool when supplied by the user.
pub fn find_liborbis_pkg_tool() -> Option<PathBuf> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        let tools = ps4_tools_directory()?;
        let configured = env::var_os("DIGITAL_FORGE_TOOL_LIBORBIS_PKG_TOOL").map(PathBuf::from);
        let tool = configured.unwrap_or_else(|| tools.join("PkgTool.exe"));
        return tool.is_file().then_some(tool);
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        None
    }
}

/// Resolves the selected PS4 command-line publisher and its runtime files.
pub fn find_orbis_pub_cmd() -> Option<PathBuf> {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        let configured = env::var_os("DIGITAL_FORGE_TOOL_ORBIS_PUB_CMD").map(PathBuf::from);
        let (publisher, tools) = if let Some(publisher) = configured {
            let tools = publisher.parent()?.to_path_buf();
            (publisher, tools)
        } else {
            let tools = ps4_tools_directory()?;
            (tools.join("orbis-pub-cmd.exe"), tools)
        };
        let complete = publisher.is_file()
            && tools.join("orbis-pub-prx.dll").is_file()
            && tools.join("ext/sc.exe").is_file()
            && tools.join("ext/di.exe").is_file()
            && tools.join("ext/libatrac9.dll").is_file();
        return complete.then_some(publisher);
    }
    #[cfg(not(all(windows, target_arch = "x86_64")))]
    {
        None
    }
}

fn find_tool(env_name: &str, candidates: &[&str]) -> Option<PathBuf> {
    let variable = format!("DIGITAL_FORGE_TOOL_{env_name}");
    if let Some(path) = env::var_os(variable) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(executable) = env::current_exe()
        && let Some(directory) = executable.parent()
    {
        for candidate in candidates {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
            #[cfg(windows)]
            {
                let exe = directory.join(format!("{candidate}.exe"));
                if exe.is_file() {
                    return Some(exe);
                }
            }
        }
    }
    for base in [
        Path::new("Releases\\WIN-X64"),
        Path::new("target\\release"),
        Path::new("..\\Releases\\WIN-X64"),
        Path::new("research\\OrbisPkgTool\\publish-single"),
    ] {
        for candidate in candidates {
            let path = base.join(candidate);
            if path.is_file() {
                return Some(path);
            }
            #[cfg(windows)]
            {
                let exe = base.join(format!("{candidate}.exe"));
                if exe.is_file() {
                    return Some(exe);
                }
            }
        }
    }
    let found = env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths).find_map(|directory| {
            candidates.iter().find_map(|candidate| {
                let path = directory.join(candidate);
                if path.is_file() {
                    return Some(path);
                }
                #[cfg(windows)]
                {
                    let exe = directory.join(format!("{candidate}.exe"));
                    if exe.is_file() {
                        return Some(exe);
                    }
                }
                None
            })
        })
    });
    if found.is_some() {
        return found;
    }
    if env_name == "PKGTOOL"
        || candidates
            .iter()
            .any(|c| c.eq_ignore_ascii_case("OrbisPkgTool") || c.eq_ignore_ascii_case("PkgTool"))
    {
        return find_orbis_pkg_tool();
    }
    None
}

fn reference_input(input: &DetectionResult) -> Result<Execution, Error> {
    let first = input.related_files.first().context("no source path")?;
    let output = first.to_path_buf();
    let aux_files = input
        .related_files
        .iter()
        .filter(|path| *path != first)
        .cloned()
        .collect();
    Ok(local_execution(
        output,
        aux_files,
        "validated direct transfer",
    ))
}

fn run_ps5_itemzflow_folder(
    input: &DetectionResult,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
    progress: Arc<dyn Fn(ProgressUpdate) + Send + Sync>,
) -> Result<Execution, Error> {
    let source = input.related_files.first().context("no PS5 game folder")?;
    let output = crate::forge::ps5::copy_prepared_itemzflow_folder(
        source,
        output_dir,
        &cancelled,
        |bytes_processed, total_bytes| {
            let percent = if total_bytes == 0 {
                100
            } else {
                ((bytes_processed.saturating_mul(100) / total_bytes).min(100)) as u8
            };
            progress(ProgressUpdate {
                stage: "Copying prepared PS5 Itemzflow folder".to_string(),
                percent,
                details: Some(format!("{bytes_processed} / {total_bytes} bytes")),
                bytes_processed,
                total_bytes,
            });
        },
    )?;
    Ok(local_execution(
        output,
        Vec::new(),
        "Digital Forge validated and copied the prepared PS5 Itemzflow folder",
    ))
}

fn split_raw(
    source: &Path,
    output_dir: &Path,
    target: TargetFormat,
    cancelled: Arc<AtomicBool>,
) -> Result<Execution, Error> {
    let stem = source
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("game.iso");
    let mut input = File::open(source)?;
    let mut parts = Vec::new();
    let mut index = 0;
    loop {
        ensure!(!cancelled.load(Ordering::Relaxed), "split cancelled");
        let path = output_dir.join(format!("{stem}.{index}"));
        let mut part = File::create(&path)?;
        let written = std::io::copy(
            &mut std::io::Read::by_ref(&mut input).take(SPLIT_SIZE),
            &mut part,
        )?;
        if written == 0 {
            fs::remove_file(path)?;
            break;
        }
        parts.push(path);
        index += 1;
    }
    let output = parts.first().cloned().context("empty source")?;
    Ok(local_execution(
        output,
        parts.into_iter().skip(1).collect(),
        match target {
            TargetFormat::Xbox360SplitIso => "Digital Forge raw split",
            _ => "raw split",
        },
    ))
}

fn merge_raw_parts(
    parts: &[PathBuf],
    output_dir: &Path,
    target: TargetFormat,
    cancelled: Arc<AtomicBool>,
) -> Result<Execution, Error> {
    let parts = validate_contiguous_parts(parts)?;
    let output = output_dir.join(match target {
        TargetFormat::Ps5Pkg => "merged.pkg",
        _ => "merged.iso",
    });
    let mut destination = File::create(&output)?;
    for part in parts {
        ensure!(!cancelled.load(Ordering::Relaxed), "reassembly cancelled");
        std::io::copy(&mut File::open(part)?, &mut destination)?;
    }
    destination.flush()?;
    Ok(local_execution(
        output,
        Vec::new(),
        "Digital Forge sequential reassembly",
    ))
}

fn validate_contiguous_parts(parts: &[PathBuf]) -> Result<Vec<&PathBuf>, Error> {
    ensure!(!parts.is_empty(), "no split parts supplied");
    let mut sorted = parts.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|path| numeric_suffix(path).unwrap_or(u32::MAX));
    for (index, path) in sorted.iter().enumerate() {
        ensure!(
            numeric_suffix(path) == Some(index as u32),
            "missing or duplicate split part at index {index}"
        );
    }
    if sorted.len() > 1 {
        let expected = fs::metadata(sorted[0])?.len();
        for part in &sorted[..sorted.len() - 1] {
            ensure!(
                fs::metadata(part)?.len() == expected,
                "non-final split parts have inconsistent sizes"
            );
        }
    }
    Ok(sorted)
}

fn numeric_suffix(path: &Path) -> Option<u32> {
    let name = path.file_name()?.to_str()?;
    let suffix = name.rsplit('.').next()?;
    if suffix.len() == 5 && suffix.starts_with("666") {
        return suffix[3..].parse().ok();
    }
    suffix.parse().ok()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Ps5MultipartHeader {
    pub part_index: u32,
    pub total_parts: u32,
    pub compression_type: u32,
    pub part_data_size: u64,
    pub total_pkg_size: u64,
    pub package_uuid: [u8; 16],
    pub part_offset: u64,
    pub data_offset: u32,
}

pub(crate) fn parse_ps5_multipart_header(bytes: &[u8]) -> Result<Ps5MultipartHeader, Error> {
    ensure!(
        bytes.len() >= PS5_MULTIPART_HEADER_SIZE,
        "PS5MPKG1 header is shorter than 4096 bytes"
    );
    ensure!(&bytes[0..8] == b"PS5MPKG1", "missing PS5MPKG1 magic");
    ensure!(
        u32_at(bytes, 8)? == 1,
        "unsupported PS5MPKG1 header version"
    );
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&bytes[664..680]);
    let header = Ps5MultipartHeader {
        part_index: u32_at(bytes, 12)?,
        total_parts: u32_at(bytes, 16)?,
        compression_type: u32_at(bytes, 20)?,
        part_data_size: u64_at(bytes, 32)?,
        total_pkg_size: u64_at(bytes, 40)?,
        package_uuid: uuid,
        part_offset: u64_at(bytes, 736)?,
        data_offset: u32_at(bytes, 744)?,
    };
    ensure!(
        header.part_index > 0 && header.part_index <= header.total_parts,
        "invalid PS5MPKG1 part index"
    );
    ensure!(
        header.compression_type == 0,
        "compressed PS5MPKG1 parts are not supported by header v1"
    );
    ensure!(
        header.data_offset as usize >= PS5_MULTIPART_HEADER_SIZE,
        "PS5MPKG1 data offset overlaps the header"
    );
    Ok(header)
}

fn merge_ps5_multipart(
    parts: &[PathBuf],
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<Execution, Error> {
    ensure!(!parts.is_empty(), "no PS5MPKG1 parts supplied");
    let mut parsed = Vec::new();
    for path in parts {
        let mut file = File::open(path)?;
        let mut bytes = [0u8; PS5_MULTIPART_HEADER_SIZE];
        file.read_exact(&mut bytes)?;
        parsed.push((parse_ps5_multipart_header(&bytes)?, path));
    }
    parsed.sort_by_key(|(header, _)| header.part_index);
    let first = parsed[0].0;
    ensure!(
        parsed.len() == first.total_parts as usize,
        "PS5MPKG1 total_parts does not match the supplied set"
    );
    for (index, (header, path)) in parsed.iter().enumerate() {
        ensure!(!cancelled.load(Ordering::Relaxed), "reassembly cancelled");
        ensure!(
            header.part_index == index as u32 + 1,
            "PS5MPKG1 part indexes are not contiguous"
        );
        ensure!(
            header.total_parts == first.total_parts
                && header.total_pkg_size == first.total_pkg_size
                && header.package_uuid == first.package_uuid,
            "PS5MPKG1 shared metadata differs between parts"
        );
        ensure!(
            fs::metadata(path)?.len() >= header.data_offset as u64 + header.part_data_size,
            "PS5MPKG1 part data extends beyond the file"
        );
    }
    let output = output_dir.join("merged.pkg");
    let mut destination = File::create(&output)?;
    destination.set_len(first.total_pkg_size)?;
    for (header, path) in parsed {
        ensure!(
            header.part_offset + header.part_data_size <= first.total_pkg_size,
            "PS5MPKG1 part range exceeds total package size"
        );
        let mut source = File::open(path)?;
        source.seek(SeekFrom::Start(header.data_offset as u64))?;
        destination.seek(SeekFrom::Start(header.part_offset))?;
        std::io::copy(&mut source.take(header.part_data_size), &mut destination)?;
    }
    destination.flush()?;
    ensure!(
        fs::metadata(&output)?.len() == first.total_pkg_size,
        "reassembled package length mismatch"
    );
    Ok(local_execution(
        output,
        Vec::new(),
        "PS5MPKG1 4096-byte header reassembly",
    ))
}

fn write_usbextreme(
    input: &DetectionResult,
    output_dir: &Path,
    cancelled: Arc<AtomicBool>,
) -> Result<Execution, Error> {
    let source = input.related_files.first().context("no ISO source")?;
    let game_id = input
        .game_id
        .as_deref()
        .context("USBExtreme requires a detected PS2 game ID")?;
    let game_name = input.title_name.as_deref().unwrap_or(game_id);
    let crc = opl_crc32(game_name);
    let prefix = format!("ul.{crc:08X}.{game_id}");
    let mut source_file = File::open(source)?;
    let total = source_file.metadata()?.len();
    let mut parts = Vec::new();
    let mut copied = 0;
    let mut index = 0;
    while copied < total {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "USBExtreme split cancelled"
        );
        let path = output_dir.join(format!("{prefix}.{index:02}"));
        let mut part = File::create(&path)?;
        let count = std::io::copy(
            &mut std::io::Read::by_ref(&mut source_file).take(USBEXTREME_CHUNK_SIZE),
            &mut part,
        )?;
        copied += count;
        parts.push(path);
        index += 1;
    }
    let cfg = output_dir.join("ul.cfg");
    write_ul_cfg(
        &cfg,
        game_name,
        game_id,
        index as u8,
        if total < 800_000_000 { 0x12 } else { 0x14 },
    )?;
    Ok(local_execution(
        cfg,
        parts,
        "PS2IsoManager-compatible USBExtreme writer",
    ))
}

fn write_ul_cfg(
    path: &Path,
    name: &str,
    game_id: &str,
    parts: u8,
    media_type: u8,
) -> Result<(), Error> {
    let mut record = [0u8; 64];
    copy_ascii(&mut record[0..32], name);
    copy_ascii(&mut record[0x20..0x2f], &format!("ul.{game_id}"));
    record[0x2f] = parts;
    record[0x30] = media_type;
    record[0x35] = 0x08;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&record)?;
    Ok(())
}

fn opl_crc32(name: &str) -> u32 {
    let mut padded = [0u8; 32];
    copy_ascii(&mut padded, name);
    let length = name.len().min(31) + 1;
    let mut table = [0u32; 256];
    for i in 0..256 {
        let mut value = (i as u32) << 24;
        for _ in 0..8 {
            value = if value & 0x8000_0000 == 0 {
                (value << 1) ^ 0x04c1_1db7
            } else {
                value << 1
            };
        }
        table[255 - i] = value;
    }
    let mut crc = 0u32;
    for byte in &padded[..length] {
        let index = ((crc >> 24) as u8 ^ byte) as usize;
        crc = (crc << 8) ^ table[index];
    }
    crc
}

fn copy_ascii(destination: &mut [u8], text: &str) {
    let bytes = text.as_bytes();
    let length = bytes.len().min(destination.len().saturating_sub(1));
    destination[..length].copy_from_slice(&bytes[..length]);
}

fn validate_detection(input: &DetectionResult) -> Result<ValidationReport, Error> {
    if input.related_files.is_empty() {
        return Ok(ValidationReport::failed("no source files"));
    }
    if !input.missing_parts.is_empty() {
        return Ok(ValidationReport::failed(format!(
            "missing parts: {}",
            input.missing_parts.join(", ")
        )));
    }
    for path in &input.related_files {
        if !path.exists() {
            return Ok(ValidationReport::failed(format!(
                "source is missing: {}",
                path.display()
            )));
        }
    }
    match input.source_format {
        SourceFormat::BinCue => validate_cue(input),
        SourceFormat::Pbp => validate_pbp(
            input.related_files.first().unwrap(),
            input.platform == Platform::PlayStation1,
        ),
        SourceFormat::Ps5PkgManagerMultipart => {
            for path in &input.related_files {
                let mut file = File::open(path)?;
                let mut header = [0u8; PS5_MULTIPART_HEADER_SIZE];
                file.read_exact(&mut header)?;
                parse_ps5_multipart_header(&header)?;
            }
            Ok(ValidationReport::ok())
        }
        SourceFormat::ShadowMountExfatImage => validate_exfat(input.related_files.first().unwrap()),
        SourceFormat::ShadowMountFfPkg => validate_ufs2(input.related_files.first().unwrap()),
        SourceFormat::Ps3JbFolder => {
            let root = input.related_files.first().unwrap();
            if !root.join("PS3_GAME").is_dir() {
                return Ok(ValidationReport::failed("PS3 JB folder lacks PS3_GAME"));
            }
            Ok(ValidationReport::ok())
        }
        SourceFormat::Ps5LooseFolder => {
            let root = input.related_files.first().unwrap();
            if !root.join("eboot.bin").is_file()
                || !root.join("sce_sys").join("param.json").is_file()
            {
                return Ok(ValidationReport::failed(
                    "PS5 game root requires eboot.bin and sce_sys/param.json",
                ));
            }
            Ok(ValidationReport::ok())
        }
        SourceFormat::XboxOriginalFolder => {
            let root = input.related_files.first().unwrap();
            if !root.join("default.xbe").is_file() {
                return Ok(ValidationReport::failed(
                    "Original Xbox folder lacks default.xbe",
                ));
            }
            Ok(ValidationReport::ok())
        }
        SourceFormat::Ps4RetailPkg => validate_ps4_pkg(input.related_files.first().unwrap(), false),
        SourceFormat::Ps4FakePkg => validate_ps4_pkg(input.related_files.first().unwrap(), true),
        SourceFormat::Ps5Pkg | SourceFormat::VitaPkg | SourceFormat::Ps3Pkg => {
            validate_pkg(input.related_files.first().unwrap())
        }
        SourceFormat::Xbox360LiveContainer => validate_stfs(input.related_files.first().unwrap()),
        SourceFormat::Xbox360God => validate_god(&input.related_files),
        _ => Ok(ValidationReport::ok()),
    }
}

fn validate_path(target: TargetFormat, path: &Path) -> Result<ValidationReport, Error> {
    if !path.exists() {
        return Ok(ValidationReport::failed("output path does not exist"));
    }
    match target {
        TargetFormat::N64Z64 => validate_magic(path, &[0x80, 0x37, 0x12, 0x40], "Z64"),
        TargetFormat::NintendoRvz => validate_magic(path, b"RVZ\x01", "RVZ"),
        TargetFormat::NintendoWia => validate_magic(path, b"WIA\x01", "WIA"),
        TargetFormat::NintendoGcz => validate_magic(path, &[0x01, 0xc0, 0x0b, 0xb1], "GCZ"),
        TargetFormat::NintendoDiscIso => validate_nintendo_disc(path),
        TargetFormat::Gdi => validate_gdi_descriptor(path),
        TargetFormat::Pbp => validate_pbp(path, true),
        TargetFormat::BinCue => {
            let detection = DetectionResult {
                platform: Platform::PlayStation1,
                source_format: SourceFormat::BinCue,
                package_classification: None,
                confidence: crate::forge::format::ConfidenceLevel::Definitive,
                game_id: None,
                title_name: None,
                is_multipart: false,
                is_archive: false,
                is_multidisc: false,
                disc_number: None,
                total_discs: None,
                related_files: vec![path.to_path_buf()],
                missing_parts: Vec::new(),
                license_status: crate::forge::format::LicenseStatus::NotApplicable,
            };
            validate_cue(&detection)
        }
        TargetFormat::Ps5ExfatImage => validate_exfat(path),
        TargetFormat::Ps5Ffpkg => validate_ufs2(path),
        TargetFormat::Chd | TargetFormat::Ps2Chd => validate_magic(path, b"MComprHD", "CHD"),
        TargetFormat::Ps2Cso
        | TargetFormat::Ps2Cso2
        | TargetFormat::PspCso
        | TargetFormat::PspCso2 => validate_magic(path, b"CISO", "CSO"),
        TargetFormat::Ps2Zso | TargetFormat::PspZso => validate_magic(path, b"ZISO", "ZSO"),
        TargetFormat::Ps2Dax | TargetFormat::PspDax => validate_magic(path, b"DAX\0", "DAX"),
        TargetFormat::Ps2Iso => validate_iso9660(path, None),
        TargetFormat::Ps3Iso => validate_iso9660(path, None),
        TargetFormat::PspIso => validate_iso9660(path, Some(b"PSP_GAME")),
        TargetFormat::Ps3JbFolder => validate_ps3_jb_folder(path),
        TargetFormat::XboxOriginalXiso | TargetFormat::XboxOriginalTrimmedXiso => {
            validate_xdvdfs(path, &[0x10000])
        }
        TargetFormat::Xbox360Iso => validate_xdvdfs(path, &[0x80000, 0x2080000, 0x10000]),
        TargetFormat::Xbox360God => validate_god_tree(path),
        TargetFormat::Ps4FakePkg => validate_ps4_pkg(path, true),
        TargetFormat::Ps4RetailPkg => validate_ps4_pkg(path, false),
        TargetFormat::Ps5Pkg => validate_pkg(path),
        TargetFormat::Ps5LooseFolder => crate::forge::ps5::validate_target_folder(
            path,
            crate::forge::profile::TargetWorkflow::Ps5ItemzFlow,
        )
        .map(|()| ValidationReport::ok()),
        TargetFormat::Ps5AnyPS5Output => crate::forge::ps5::validate_anyps5_output(path),
        TargetFormat::Ps3Pkg => validate_ps3_pkg(path),
        TargetFormat::Ps2UsbExtreme => validate_ul_cfg(path),
        TargetFormat::VitaZip => validate_zip(path),
        TargetFormat::VitaNoNpDrmFolder => validate_vita_tree(path),
        TargetFormat::Ps4Folder => {
            let has_payload = path.join("eboot.bin").is_file()
                || path.join("Image0").join("eboot.bin").is_file()
                || path.join("param.sfo").is_file()
                || path.join("sce_sys").join("param.sfo").is_file()
                || path
                    .join("Image0")
                    .join("sce_sys")
                    .join("param.sfo")
                    .is_file()
                || path.join("Sc0").join("param.sfo").is_file()
                || path.join("package.gp4").is_file();
            if !has_payload {
                Ok(ValidationReport::failed(
                    "extracted PS4 folder lacks game payload or metadata",
                ))
            } else {
                let mut rep = ValidationReport::ok();
                let sfo1 = path.join("sce_sys").join("param.sfo");
                let sfo2 = path.join("param.sfo");
                for sfo in &[sfo1, sfo2] {
                    if sfo.is_file() {
                        let (id, title) = crate::forge::detector::FormatDetector::read_sfo(sfo);
                        rep.game_id = id;
                        rep.game_title = title;
                        break;
                    }
                }
                Ok(rep)
            }
        }
        TargetFormat::XboxOriginalFolder if !path.join("default.xbe").is_file() => Ok(
            ValidationReport::failed("extracted Xbox folder lacks default.xbe"),
        ),
        TargetFormat::Xbox360Folder if !path.join("default.xex").is_file() => Ok(
            ValidationReport::failed("extracted Xbox 360 disc folder lacks default.xex"),
        ),
        _ if path.is_file() && fs::metadata(path)?.len() == 0 => {
            Ok(ValidationReport::failed("output file is empty"))
        }
        _ => Ok(ValidationReport::ok()),
    }
}

fn validate_ps3_jb_folder(path: &Path) -> Result<ValidationReport, Error> {
    let game = path.join("PS3_GAME");
    if !game.is_dir() || !game.join("PARAM.SFO").is_file() || !game.join("USRDIR").is_dir() {
        return Ok(ValidationReport::failed(
            "PS3 JB output requires PS3_GAME/PARAM.SFO and PS3_GAME/USRDIR",
        ));
    }
    Ok(ValidationReport::ok())
}

fn validate_magic(path: &Path, magic: &[u8], format: &str) -> Result<ValidationReport, Error> {
    let mut bytes = vec![0u8; magic.len()];
    File::open(path)?.read_exact(&mut bytes)?;
    if bytes != magic {
        return Ok(ValidationReport::failed(format!("invalid {format} header")));
    }
    Ok(ValidationReport::ok())
}

fn validate_nintendo_disc(path: &Path) -> Result<ValidationReport, Error> {
    let mut header = [0u8; 0x20];
    File::open(path)?.read_exact(&mut header)?;
    if header[0x1c..0x20] != [0xc2, 0x33, 0x9f, 0x3d]
        && header[0x18..0x1c] != [0x5d, 0x1c, 0x9e, 0xa3]
    {
        return Ok(ValidationReport::failed(
            "not a GameCube or Wii disc header",
        ));
    }
    Ok(ValidationReport::ok())
}

fn validate_gdi_descriptor(path: &Path) -> Result<ValidationReport, Error> {
    let text = fs::read_to_string(path)?;
    let count = text
        .lines()
        .next()
        .and_then(|line| line.trim().parse::<usize>().ok());
    if count.is_none()
        || text
            .lines()
            .skip(1)
            .filter(|line| !line.trim().is_empty())
            .count()
            != count.unwrap()
    {
        return Ok(ValidationReport::failed("invalid GDI descriptor"));
    }
    Ok(ValidationReport::ok())
}

fn validate_iso9660(path: &Path, required_name: Option<&[u8]>) -> Result<ValidationReport, Error> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(0x8000))?;
    let mut descriptor = [0u8; 2048];
    file.read_exact(&mut descriptor)?;
    if &descriptor[1..6] != b"CD001" {
        return Ok(ValidationReport::failed(
            "ISO9660 primary volume descriptor is missing",
        ));
    }
    if let Some(name) = required_name {
        file.seek(SeekFrom::Start(0))?;
        let mut sample = vec![0u8; fs::metadata(path)?.len().min(8 * 1024 * 1024) as usize];
        file.read_exact(&mut sample)?;
        if !sample.windows(name.len()).any(|window| window == name) {
            return Ok(ValidationReport::failed(format!(
                "ISO filesystem does not contain {}",
                String::from_utf8_lossy(name)
            )));
        }
    }
    Ok(ValidationReport::ok())
}

fn validate_ul_cfg(path: &Path) -> Result<ValidationReport, Error> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() || bytes.len() % 64 != 0 {
        return Ok(ValidationReport::failed(
            "ul.cfg is not a sequence of 64-byte records",
        ));
    }
    for record in bytes.chunks_exact(64) {
        if &record[0x20..0x23] != b"ul."
            || record[0x2f] == 0
            || !matches!(record[0x30], 0x12 | 0x14)
            || record[0x35] != 0x08
        {
            return Ok(ValidationReport::failed("ul.cfg record fields are invalid"));
        }
    }
    Ok(ValidationReport::ok())
}

fn validate_zip(path: &Path) -> Result<ValidationReport, Error> {
    let mut magic = [0u8; 4];
    File::open(path)?.read_exact(&mut magic)?;
    if !matches!(&magic, b"PK\x03\x04" | b"PK\x05\x06" | b"PK\x07\x08") {
        return Ok(ValidationReport::failed("invalid ZIP/VPK header"));
    }
    Ok(ValidationReport::ok())
}

fn validate_vita_tree(path: &Path) -> Result<ValidationReport, Error> {
    fn contains_vita_root(path: &Path) -> bool {
        if path.join("eboot.bin").is_file() && path.join("sce_sys").join("param.sfo").is_file() {
            return true;
        }
        fs::read_dir(path).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.path().is_dir() && contains_vita_root(&entry.path()))
        })
    }
    if !contains_vita_root(path) {
        return Ok(ValidationReport::failed(
            "extracted Vita tree lacks eboot.bin and sce_sys/param.sfo",
        ));
    }
    Ok(ValidationReport::ok())
}

fn validate_god(paths: &[PathBuf]) -> Result<ValidationReport, Error> {
    let has_data = paths.iter().any(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                name.starts_with("Data") && name[4..].chars().all(|ch| ch.is_ascii_digit())
            })
    });
    let has_header = paths.iter().any(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| !name.starts_with("Data") && path.is_file())
    });
    if !has_data || !has_header {
        return Ok(ValidationReport::failed(
            "GOD header/data-file relationship is incomplete",
        ));
    }
    Ok(ValidationReport::ok())
}

fn validate_god_tree(root: &Path) -> Result<ValidationReport, Error> {
    fn collect(path: &Path, files: &mut Vec<PathBuf>) {
        if path.is_file() {
            files.push(path.to_path_buf());
        } else if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                collect(&entry.path(), files);
            }
        }
    }
    let mut files = Vec::new();
    collect(root, &mut files);
    validate_god(&files)
}

fn validate_xdvdfs(path: &Path, offsets: &[u64]) -> Result<ValidationReport, Error> {
    let mut file = File::open(path)?;
    for offset in offsets {
        file.seek(SeekFrom::Start(*offset))?;
        let mut magic = [0u8; 20];
        if file.read_exact(&mut magic).is_ok() && &magic == b"MICROSOFT*XBOX*MEDIA" {
            return Ok(ValidationReport::ok());
        }
    }
    Ok(ValidationReport::failed(
        "XDVDFS volume descriptor was not found",
    ))
}

fn validate_cue(input: &DetectionResult) -> Result<ValidationReport, Error> {
    let cue = input
        .related_files
        .iter()
        .find(|path| {
            path.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("cue"))
        })
        .context("BIN/CUE input has no CUE sheet")?;
    let text = fs::read_to_string(cue)?;
    let mut files = 0;
    let mut tracks = 0;
    let mut saw_supported_track = false;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("FILE ") {
            let name = rest
                .strip_prefix('"')
                .and_then(|value| value.split('"').next())
                .context("CUE FILE entry must use a quoted filename")?;
            ensure!(
                cue.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(name)
                    .is_file(),
                "CUE references missing file {name}"
            );
            files += 1;
        } else if let Some(rest) = line.strip_prefix("TRACK ") {
            tracks += 1;
            saw_supported_track |= rest.contains("MODE2/2352")
                || rest.contains("AUDIO")
                || rest.contains("MODE1/2352");
        }
    }
    if files == 0 || tracks == 0 || !saw_supported_track {
        return Ok(ValidationReport::failed(
            "CUE has no supported FILE/TRACK layout",
        ));
    }
    Ok(ValidationReport::ok())
}

fn validate_pbp(path: &Path, require_ps1: bool) -> Result<ValidationReport, Error> {
    let bytes = fs::read(path)?;
    if bytes.len() < 40 || &bytes[0..4] != b"\0PBP" {
        return Ok(ValidationReport::failed("invalid PBP header"));
    }
    let mut previous = 40u32;
    let mut offsets = Vec::new();
    for position in (8..40).step_by(4) {
        let offset = u32_at(&bytes, position)?;
        if offset < previous || offset as usize > bytes.len() {
            return Ok(ValidationReport::failed(
                "PBP section offsets are not monotonic and in bounds",
            ));
        }
        previous = offset;
        offsets.push(offset as usize);
    }
    if require_ps1 {
        let psar = offsets[7];
        if psar >= bytes.len()
            || !bytes[psar..bytes.len().min(psar + 64)]
                .windows(8)
                .any(|window| window == b"PSISOIMG")
        {
            return Ok(ValidationReport::failed(
                "PBP is not a PS1 Classics/Popstation image",
            ));
        }
    }
    Ok(ValidationReport::ok())
}

fn validate_exfat(path: &Path) -> Result<ValidationReport, Error> {
    let mut boot = [0u8; 512];
    File::open(path)?.read_exact(&mut boot)?;
    if &boot[3..11] != b"EXFAT   " || boot[510..512] != [0x55, 0xaa] {
        return Ok(ValidationReport::failed(
            "not an exFAT filesystem boot sector",
        ));
    }
    Ok(ValidationReport::ok())
}

fn validate_ufs2(path: &Path) -> Result<ValidationReport, Error> {
    let mut file = File::open(path)?;
    for offset in [65_536u64 + 1_372, 8_192 + 1_372, 0 + 1_372] {
        if file.seek(SeekFrom::Start(offset)).is_ok() {
            let mut magic = [0u8; 4];
            if file.read_exact(&mut magic).is_ok() && u32::from_le_bytes(magic) == 0x1954_0119 {
                return Ok(ValidationReport::ok());
            }
        }
    }
    Ok(ValidationReport::failed(
        "UFS2 superblock magic was not found",
    ))
}

fn validate_pkg(path: &Path) -> Result<ValidationReport, Error> {
    let mut magic = [0u8; 4];
    File::open(path)?.read_exact(&mut magic)?;
    if magic != [0x7f, b'P', b'K', b'G']
        && magic != [0x7f, b'C', b'N', b'T']
        && magic != [0x7f, b'F', b'I', b'H']
    {
        return Ok(ValidationReport::failed(
            "unrecognised PlayStation PKG header",
        ));
    }
    Ok(ValidationReport::ok())
}

fn validate_ps3_pkg(path: &Path) -> Result<ValidationReport, Error> {
    let mut header = [0u8; 0x60];
    File::open(path)?.read_exact(&mut header)?;
    if &header[..4] != b"\x7fPKG" {
        return Ok(ValidationReport::failed("unrecognised PS3 PKG header"));
    }
    let content_id = String::from_utf8_lossy(&header[0x30..0x60])
        .trim_end_matches('\0')
        .trim()
        .to_owned();
    if !valid_ps3_content_id(&content_id) {
        return Ok(ValidationReport::failed(
            "PS3 PKG has no valid 36-character Content ID",
        ));
    }
    if fs::metadata(path)?.len() <= 0x1a0 {
        return Ok(ValidationReport::failed("PS3 PKG has no package data area"));
    }
    let mut report = ValidationReport::ok();
    report.game_id = content_id
        .split('-')
        .nth(1)
        .and_then(|part| part.split('_').next())
        .map(str::to_owned);
    Ok(report)
}

fn validate_ps4_pkg(path: &Path, fake_tolerant: bool) -> Result<ValidationReport, Error> {
    let magic_report = validate_pkg(path)?;
    if !magic_report.is_valid {
        return Ok(magic_report);
    }
    if fs::metadata(path)?.len() < 1024 * 1024 {
        return Ok(ValidationReport::failed(
            "PS4 PKG is smaller than the format's 1 MiB minimum",
        ));
    }
    let native_tool = find_native_orbis_pkg_tool();
    let output = if let Some(tool) = &native_tool {
        let mut command = Command::new(tool);
        command.arg("validate");
        if fake_tolerant {
            command.arg("--fake-tolerant");
        }
        command.arg(path).output()
    } else {
        let lib_tool = find_liborbis_pkg_tool()
            .context("PS4 deep validation requires OrbisPkgTool or LibOrbisPkg PkgTool")?;
        Command::new(&lib_tool)
            .arg("pkg_validate")
            .arg("--verbose")
            .arg(path)
            .output()
    }
    .with_context(|| format!("could not deep-validate {}", path.display()))?;
    let validation_text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() || validation_text.contains("[ERROR]") {
        let legacy_digest_only = validation_text
            .lines()
            .filter(|line| line.contains("[ERROR]"))
            .all(|line| {
                line.contains("Content Digest")
                    || line.contains("Major Param Digest")
                    || line.contains("NPBIND_DAT digest")
            });
        if !legacy_digest_only {
            return Ok(ValidationReport::failed(format!(
                "PS4 deep validation failed: {validation_text}"
            )));
        }
    }
    let tool = native_tool
        .or_else(find_orbis_pkg_tool)
        .context("PS4 semantic validation requires OrbisPkgTool")?;
    let info = Command::new(&tool).arg("info").arg(path).output()?;
    if !info.status.success() {
        return Ok(ValidationReport::failed(
            "PS4 metadata could not be reopened",
        ));
    }
    let text = String::from_utf8_lossy(&info.stdout);
    let category = text.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(key, _)| key.trim().eq_ignore_ascii_case("Category"))
            .map(|(_, value)| value.trim())
    });
    if !matches!(category, Some("gd" | "gp" | "ac" | "th" | "av" | "wa")) {
        return Ok(ValidationReport::failed(
            "PS4 package has a missing or unsupported CATEGORY",
        ));
    }
    let mut report = ValidationReport::ok();
    if fake_tolerant {
        report
            .warnings
            .push("validated as a fake-signed package".to_string());
    }
    if validation_text.contains("[ERROR]") {
        report.warnings.push(
            "accepted legacy source digest quirks; output will be rebuilt and strictly revalidated"
                .to_string(),
        );
    }
    Ok(report)
}

fn validate_stfs(path: &Path) -> Result<ValidationReport, Error> {
    let mut magic = [0u8; 4];
    File::open(path)?.read_exact(&mut magic)?;
    if !matches!(&magic, b"CON " | b"LIVE" | b"PIRS") {
        return Ok(ValidationReport::failed(
            "invalid STFS CON/LIVE/PIRS signature",
        ));
    }
    Ok(ValidationReport::ok())
}

fn local_execution(output_path: PathBuf, aux_files: Vec<PathBuf>, operation: &str) -> Execution {
    Execution {
        output_path,
        aux_files,
        command: vec![operation.to_owned()],
        stdout: String::new(),
        stderr: String::new(),
        exit_code: Some(0),
        tool_version: None,
    }
}

fn input_size(input: &DetectionResult) -> u64 {
    input
        .related_files
        .iter()
        .filter_map(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len())
        .sum()
}

fn is_merge_target(target: TargetFormat) -> bool {
    matches!(target, TargetFormat::Ps5Pkg | TargetFormat::Xbox360Iso)
}

fn os(value: impl AsRef<OsStr>) -> OsString {
    value.as_ref().to_owned()
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .context("truncated header")?
            .try_into()?,
    ))
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..offset + 8)
            .context("truncated header")?
            .try_into()?,
    ))
}
