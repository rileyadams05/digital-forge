mod app_log;
mod app_settings;
mod automation;
mod download_queue;
mod pc_host;
mod pipeline;
mod store;
mod store_worker;
mod workflow;
mod workspace;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, IsTerminal, Read, Seek, SeekFrom, Write};

use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Error};

use clap::{Parser, ValueEnum};
use crossterm::cursor::MoveTo;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};

use rayon::prelude::*;

use iso2god::executable::TitleInfo;
use iso2god::god::ContentType;
use iso2god::{game_list, god, iso};
use workflow::{
    parse_dropped_paths_input, receive_ps5_dump, run_drag_and_drop_pipeline, select_ftp_platform,
};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(color = clap::ColorChoice::Never)]
struct Cli {
    /// ISO or ZIP/7Z/RAR archive to convert
    source_iso: PathBuf,

    /// A folder to write resulting GOD files to
    dest_dir: PathBuf,

    /// Do not convert anything, just print the title info
    #[arg(long)]
    dry_run: bool,

    /// Set game title
    #[arg(long, value_name = "TITLE")]
    game_title: Option<String>,

    /// Whether to trim off unused space from the ISO image;
    /// passing no --trim flag at all is equivalent to "from-end"
    #[arg(
        verbatim_doc_comment,
        long,
        value_enum,
        require_equals = true,
        num_args = 0..=1,
        default_missing_value = "from-end"
    )]
    trim: Option<TrimMode>,

    /// Number of worker threads to use
    #[arg(long, short = 'j', value_name = "N", default_value_t = 1)]
    num_threads: usize,

    /// Prompt for Xbox FTP details and upload after conversion
    #[arg(long, conflicts_with = "copy_usb")]
    upload_ftp: bool,

    /// Copy the converted game to a removable USB drive
    #[arg(long, conflicts_with = "upload_ftp")]
    copy_usb: bool,

    /// Use the securely saved FTP connection without prompting
    #[arg(long, hide = true, conflicts_with_all = ["upload_ftp", "copy_usb"])]
    use_saved_ftp: bool,

    #[arg(skip)]
    prompt_delivery: bool,

    #[arg(skip)]
    selected_usb_drive: Option<RemovableDrive>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Default, ValueEnum)]
enum TrimMode {
    /// (default) Trim unallocated space from the end
    #[default]
    FromEnd,

    /// Trim nothing
    None,
    // TODO
    // FullRebuild,
}

fn main() {
    let raw_args: Vec<OsString> = env::args_os().collect();
    if raw_args.len() == 2 && raw_args[1] == OsStr::new("--pair-digital360") {
        if let Err(error) = pc_host::start_and_pair() {
            eprintln!("Digital 360 setup failed: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if raw_args.len() == 3 && raw_args[1] == OsStr::new("--automation-worker") {
        std::process::exit(automation::run_worker(Path::new(&raw_args[2])));
    }
    if raw_args.len() == 2 && raw_args[1] == OsStr::new("--download-queue-service") {
        std::process::exit(download_queue::run_service());
    }
    if raw_args.len() == 2 && raw_args[1] == OsStr::new("--store-worker-service") {
        std::process::exit(store_worker::run());
    }
    if raw_args.len() == 3 && raw_args[1] == OsStr::new("--store-worker-job") {
        std::process::exit(store_worker::run_job(Path::new(&raw_args[2])));
    }
    if raw_args.len() == 3 && raw_args[1] == OsStr::new("--download-queue-job") {
        std::process::exit(download_queue::run_job(&raw_args[2].to_string_lossy()));
    }
    if raw_args.len() == 2 && raw_args[1] == OsStr::new("--third-party-notices") {
        print!("{THIRD_PARTY_NOTICES}");
        return;
    }
    if raw_args.len() == 2 && raw_args[1] == OsStr::new("--mcp-server") {
        if let Err(error) = run_mcp_stdio_server() {
            eprintln!("AI bridge error: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if raw_args.len() == 2 && raw_args[1] == OsStr::new("--ai-status") {
        match ai_converter_status() {
            Ok(status) => println!("{}", serde_json::to_string_pretty(&status).unwrap()),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
        return;
    }
    if raw_args.len() == 3 && raw_args[1] == OsStr::new("--scan-folder-json") {
        let folder = PathBuf::from(&raw_args[2]);
        match scan_batch_game_inputs(&folder) {
            Ok(items) => println!("{}", serde_json::to_string_pretty(&items).unwrap()),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
        return;
    }
    let mut dropped_sources = Vec::new();
    let mut is_cli_flag_invocation = false;
    for arg in raw_args.iter().skip(1) {
        let s = arg.to_string_lossy();
        if s.starts_with('-') {
            is_cli_flag_invocation = true;
            break;
        } else {
            dropped_sources.push(PathBuf::from(arg));
        }
    }
    let interactive = !is_cli_flag_invocation;
    let result = if interactive {
        terminal_application(dropped_sources)
    } else {
        convert_and_maybe_upload(Cli::parse_from(raw_args))
    };
    let failed = result.is_err();

    if let Err(ref error) = result {
        eprintln!("\nError: {error:#}");
    }

    if interactive && failed {
        pause_before_exit();
    }

    if failed {
        std::process::exit(1);
    }
}

const THIRD_PARTY_NOTICES: &str = include_str!("../../THIRD-PARTY-NOTICES.txt");

#[derive(Clone, Copy)]
enum TerminalDelivery {
    Ftp,
    Usb,
    Local,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainAction {
    Exit,
    UpdateNow,
    ReceivePs5Dump,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManualDestinationAction {
    Local,
    Usb,
    Back,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UpdateAvailability {
    version: String,
}

struct MenuOption<T> {
    value: T,
    label: String,
}

fn terminal_application(dropped_sources: Vec<PathBuf>) -> Result<(), Error> {
    // Sweep abandoned job workspaces left by previous crashed/killed instances
    {
        let pref = workspace::load_workspace_preference();
        workspace::sweep_stale_jobs(&pref);
    }

    // Draw immediately before doing any network work.  A blocked GitHub
    // request must never look like a dead/black terminal window.
    render_terminal_header()?;
    println!("Checking for updates (press Ctrl+C to cancel)...");
    io::stdout().flush()?;
    let mut deferred_update = check_for_automatic_update().ok().flatten();
    if let Some(update) = deferred_update.as_ref() {
        render_update_prompt(update)?;
        let Some(update_now) = prompt_yes_no_key("Would you like to update now?", true)? else {
            return Ok(());
        };
        if update_now {
            install_automatic_update(update)?;
            return Ok(());
        }
    }

    if !dropped_sources.is_empty() {
        return run_drag_and_drop_pipeline(&dropped_sources);
    }

    loop {
        render_terminal_header()?;
        println!();
        println!("\x1b[1mDrag and drop your game folder here.\x1b[0m");
        println!();
        print!("Folder path: ");
        io::stdout().flush()?;

        let input = read_folder_path_input()?;
        let input_trimmed = input.trim();

        if !input_trimmed.is_empty() {
            let parsed_paths = parse_dropped_paths_input(input_trimmed);
            if !parsed_paths.is_empty() {
                run_drag_and_drop_pipeline(&parsed_paths)?;
                continue;
            } else {
                println!(
                    "\n\x1b[31;1mError: Path does not exist: {}\x1b[0m",
                    input_trimmed
                );
                pause_to_continue()?;
                continue;
            }
        }

        println!("\n\x1b[1mOptions:\x1b[0m\n");
        let options = main_menu_options(deferred_update.is_some());
        let Some(action) = select_menu(&options)? else {
            return Ok(());
        };

        match action {
            MainAction::Exit => return Ok(()),
            MainAction::ReceivePs5Dump => {
                receive_ps5_dump()?;
            }
            MainAction::UpdateNow => {
                let update = deferred_update
                    .take()
                    .context("the available update could not be loaded")?;
                install_automatic_update(&update)?;
                return Ok(());
            }
        }
    }
}

fn read_folder_path_input() -> Result<String, Error> {
    let _raw_input = LineInputGuard::enter()?;
    let mut input = String::new();

    loop {
        // Check before blocking for the next event. Windows Terminal can leave
        // key-release events after the final dropped-path character; checking
        // only inside the character branch would miss the idle transition.
        if parse_dropped_paths_input(&input).first().is_some() && !poll(Duration::from_millis(250))?
        {
            println!();
            return Ok(input);
        }

        match read().context("could not read folder path input")? {
            Event::Paste(pasted) => {
                println!();
                return Ok(pasted);
            }
            Event::Key(event) if event.kind == KeyEventKind::Press => match event.code {
                KeyCode::Char(character) => {
                    input.push(character);
                    print!("{character}");
                    io::stdout().flush()?;
                }
                KeyCode::Backspace => {
                    if input.pop().is_some() {
                        print!("\x08 \x08");
                        io::stdout().flush()?;
                    }
                }
                KeyCode::Enter => {
                    println!();
                    return Ok(input);
                }
                KeyCode::Esc => {
                    println!();
                    return Ok(String::new());
                }
                _ => {}
            },
            _ => {}
        }
    }
}

const SEVEN_ZIP_DOWNLOAD_URL: &str = "https://www.7-zip.org/download.html";

fn offer_archive_tool_setup() -> Result<(), Error> {
    if find_archive_tool().is_some()
        || env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1"))
    {
        return Ok(());
    }

    render_terminal_header()?;
    render_status_summary()?;
    println!("\n7-Zip or WinRAR is required when you drop an archive.");
    println!("ISO files can still be converted without either program.\n");
    if let Some(installer_name) = archive_install_method() {
        let Some(install_now) =
            prompt_yes_no_key("Would you like to install 7-Zip automatically now?", true)?
        else {
            return Ok(());
        };

        if install_now {
            println!("\nPlease wait while {installer_name} installs 7-Zip.");
            println!("Your operating system may ask you to approve the installation.\n");
            match install_7zip_automatically() {
                Ok(path) => {
                    println!("\n\x1b[32;1m7-Zip installed successfully.\x1b[0m");
                    println!("  {}", path.display());
                    pause_to_continue()?;
                    return Ok(());
                }
                Err(error) => {
                    println!("\n\x1b[33;1mAutomatic installation was not completed.\x1b[0m");
                    println!("  {error:#}\n");
                }
            }
        }
    } else {
        println!("No supported package manager was found, so automatic installation cannot run.\n");
    }

    let Some(open_page) = prompt_yes_no_key("Open the official 7-Zip download page?", true)? else {
        return Ok(());
    };
    if open_page {
        open_url_in_default_browser(SEVEN_ZIP_DOWNLOAD_URL)?;
        println!("\nThe official 7-Zip download page was opened in your default browser.");
        pause_to_continue()?;
    }
    Ok(())
}

#[cfg(windows)]
fn archive_install_method() -> Option<&'static str> {
    find_executable_on_path(&["winget.exe", "winget"]).map(|_| "Windows Package Manager")
}

#[cfg(target_os = "linux")]
fn archive_install_method() -> Option<&'static str> {
    if find_executable_on_path(&["apt-get"]).is_some() {
        Some("APT")
    } else if find_executable_on_path(&["dnf"]).is_some() {
        Some("DNF")
    } else if find_executable_on_path(&["pacman"]).is_some() {
        Some("Pacman")
    } else if find_executable_on_path(&["zypper"]).is_some() {
        Some("Zypper")
    } else {
        None
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn archive_install_method() -> Option<&'static str> {
    None
}

#[cfg(windows)]
fn install_7zip_automatically() -> Result<PathBuf, Error> {
    let status = Command::new("winget.exe")
        .args([
            "install",
            "--id",
            "7zip.7zip",
            "--exact",
            "--source",
            "winget",
            "--silent",
            "--accept-source-agreements",
            "--accept-package-agreements",
        ])
        .status()
        .context("Windows Package Manager (winget) is unavailable")?;

    if !status.success() {
        anyhow::bail!("Windows Package Manager returned {status}");
    }

    find_7zip().context("Windows reported success, but the 7-Zip executable was not found")
}

#[cfg(target_os = "linux")]
fn install_7zip_automatically() -> Result<PathBuf, Error> {
    let (manager, arguments): (&str, &[&str]) = if find_executable_on_path(&["apt-get"]).is_some() {
        ("apt-get", &["install", "-y", "p7zip-full"])
    } else if find_executable_on_path(&["dnf"]).is_some() {
        ("dnf", &["install", "-y", "7zip"])
    } else if find_executable_on_path(&["pacman"]).is_some() {
        ("pacman", &["-S", "--needed", "--noconfirm", "7zip"])
    } else if find_executable_on_path(&["zypper"]).is_some() {
        ("zypper", &["--non-interactive", "install", "7zip"])
    } else {
        anyhow::bail!("no supported Linux package manager was found");
    };

    let status = if let Some(sudo) = find_executable_on_path(&["sudo"]) {
        Command::new(sudo).arg(manager).args(arguments).status()
    } else {
        Command::new(manager).args(arguments).status()
    }
    .with_context(|| format!("could not start {manager}"))?;

    if !status.success() {
        anyhow::bail!("{manager} returned {status}");
    }
    find_7zip().context("Linux reported success, but the 7-Zip executable was not found")
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn install_7zip_automatically() -> Result<PathBuf, Error> {
    anyhow::bail!("automatic 7-Zip installation is unsupported on this operating system")
}

#[cfg(windows)]
fn open_url_in_default_browser(url: &str) -> Result<(), Error> {
    Command::new("explorer.exe")
        .arg(url)
        .spawn()
        .context("could not open the default web browser")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_url_in_default_browser(url: &str) -> Result<(), Error> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("could not open the default web browser")?;
    Ok(())
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn open_url_in_default_browser(_url: &str) -> Result<(), Error> {
    anyhow::bail!("opening a web browser is unsupported on this operating system")
}

#[cfg(windows)]
fn open_folder_in_file_explorer(path: &Path) -> Result<(), Error> {
    Command::new("explorer.exe")
        .arg(path)
        .spawn()
        .with_context(|| format!("could not open {} in File Explorer", path.display()))?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_folder_in_file_explorer(path: &Path) -> Result<(), Error> {
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .with_context(|| format!("could not open {} in the file manager", path.display()))?;
    Ok(())
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn open_folder_in_file_explorer(_path: &Path) -> Result<(), Error> {
    anyhow::bail!("opening folders is unsupported on this operating system")
}

fn main_menu_options(update_was_deferred: bool) -> Vec<MenuOption<MainAction>> {
    let mut options = vec![MenuOption {
        value: MainAction::ReceivePs5Dump,
        label: "1. Receive PS5 game dump".to_owned(),
    }];
    if update_was_deferred {
        options.push(MenuOption {
            value: MainAction::UpdateNow,
            label: "2. Update now".to_owned(),
        });
    }
    options.push(MenuOption {
        value: MainAction::Exit,
        label: if update_was_deferred {
            "3. Exit"
        } else {
            "2. Exit"
        }
        .to_owned(),
    });
    options
}

fn run_ftp_settings_menu() -> Result<(), Error> {
    loop {
        let Some(platform) = select_ftp_platform()? else {
            return Ok(());
        };
        let result = match platform {
            iso2god::forge::Platform::Xbox360 => connect_saved_or_configure_xbox_ftp(),
            platform if platform.supports_ftp_configuration() => {
                connect_saved_or_configure_platform_ftp(platform)
            }
            _ => anyhow::bail!("{} is not FTP compatible", platform.display_name()),
        };
        if let Err(error) = result {
            print_terminal_error(&error);
        }
        pause_to_continue()?;
    }
}

fn connect_saved_or_configure_platform_ftp(
    platform: iso2god::forge::Platform,
) -> Result<(), Error> {
    render_terminal_header()?;
    println!("{} FTP", platform.display_name());
    println!("========================================================\n");

    if let Some(mut settings) = load_platform_ftp_settings(platform)? {
        settings.password = load_platform_ftp_password(platform)?.unwrap_or_default();
        println!("Testing the saved FTP connection...");
        match test_platform_ftp_connection(&settings) {
            Ok(()) => {
                println!("\x1b[32mConnection successful. Saved FTP details are ready.\x1b[0m");
                return Ok(());
            }
            Err(error) => {
                print_terminal_error(&error);
                println!("\nThe saved connection failed. Enter the current FTP details again.\n");
            }
        }
    }

    configure_platform_ftp_terminal(platform)
}

fn configure_platform_ftp_terminal(platform: iso2god::forge::Platform) -> Result<(), Error> {
    let existing = load_platform_ftp_settings(platform)?;
    let host_default = existing.as_ref().map(|settings| settings.host.as_str());
    let username_default = existing
        .as_ref()
        .map(|settings| settings.username.as_str())
        .unwrap_or("anonymous");

    let Some(host_text) = prompt_terminal_text("IP address", host_default, false)? else {
        return Ok(());
    };
    let host = normalise_ftp_host(&host_text)?;
    let Some(username) = prompt_terminal_text(
        "Username (optional; press Enter if none)",
        Some(username_default),
        false,
    )?
    else {
        return Ok(());
    };
    let username = if username.trim().is_empty() {
        "anonymous".to_owned()
    } else {
        username.trim().to_owned()
    };
    let Some(password) =
        prompt_optional_terminal_text("Password (optional; press Enter if none)", None, true)?
    else {
        return Ok(());
    };

    let settings = FtpSettings {
        host,
        port: platform.default_ftp_port(),
        background_port: None,
        username,
        password,
        destination_path: platform.default_console_ftp_dir().to_owned(),
    };

    println!("\nPlease wait... testing the FTP connection.");
    test_platform_ftp_connection(&settings).with_context(|| {
        format!(
            "could not connect to {}. Make sure its FTP server is running and both devices are on the same network",
            platform.display_name()
        )
    })?;
    save_platform_ftp_settings(platform, &settings)?;
    save_platform_ftp_password(platform, &settings.username, &settings.password)?;
    println!("\x1b[32mConnection successful. FTP settings saved.\x1b[0m");
    Ok(())
}

fn test_platform_ftp_connection(settings: &FtpSettings) -> Result<(), Error> {
    run_curl_ftp(
        &settings.host,
        settings.port,
        &settings.username,
        &settings.password,
        "/",
        false,
    )?;
    Ok(())
}

fn connect_saved_or_configure_xbox_ftp() -> Result<(), Error> {
    render_terminal_header()?;
    println!("Xbox 360 FTP");
    println!("========================================================\n");
    if saved_ftp_is_ready()? {
        println!("Testing the saved FTP connection...");
        match saved_ftp_settings_with_password()
            .and_then(|settings| RcloneFtp::new(&settings))
            .and_then(|ftp| ftp.connect_available().map(|_| ()))
        {
            Ok(()) => {
                println!("\x1b[32mConnection successful. Saved FTP details are ready.\x1b[0m");
                return Ok(());
            }
            Err(error) => {
                print_terminal_error(&error);
                println!("\nThe saved connection failed. Enter the current FTP details again.\n");
            }
        }
    }
    configure_ftp_terminal()?;
    Ok(())
}

fn update_repository() -> Option<(&'static str, &'static str)> {
    let repository = option_env!("GITHUB_REPOSITORY")?;
    let (owner, name) = repository.split_once('/')?;
    (!owner.is_empty() && !name.is_empty()).then_some((owner, name))
}

fn check_for_automatic_update() -> Result<Option<UpdateAvailability>, Error> {
    if env::var_os("ISO2GOD_DISABLE_UPDATE_CHECK").is_some() {
        return Ok(None);
    }
    let Some((owner, repository)) = update_repository() else {
        return Ok(None);
    };
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(owner)
        .repo_name(repository)
        // Startup must remain bounded when GitHub, DNS, or a proxy is
        // unavailable.  Update discovery is optional and should never hold
        // the interactive app in a blank terminal.
        .timeout(Duration::from_secs(4))
        .retries(0)
        .build()
        .context("could not configure the GitHub update service")?
        .fetch()
        .context("could not check GitHub for updates")?;
    let Some(latest) = releases.latest() else {
        return Ok(None);
    };
    if self_update::version::bump_is_greater(env!("CARGO_PKG_VERSION"), latest.version())
        .context("the latest release has an invalid version number")?
    {
        Ok(Some(UpdateAvailability {
            version: latest.version().to_owned(),
        }))
    } else {
        Ok(None)
    }
}

fn render_update_prompt(update: &UpdateAvailability) -> Result<(), Error> {
    print!("\x1b]0;Digital Forge Update\x07\x1b[2J\x1b[H");
    println!("\x1b[38;5;208m============================================================\x1b[0m");
    println!("\x1b[1m                    Update available\x1b[0m");
    println!("\x1b[38;5;208m============================================================\x1b[0m\n");
    println!("Digital Forge {} is available.", update.version);
    println!("Current version: {}\n", env!("CARGO_PKG_VERSION"));
    io::stdout()
        .flush()
        .context("error drawing the update screen")
}

fn install_automatic_update(update: &UpdateAvailability) -> Result<(), Error> {
    let (owner, repository) = update_repository()
        .context("automatic updates are not configured for this development build")?;
    render_update_prompt(update)?;
    println!("Please wait while the verified update is downloaded and installed.\n");
    let status = self_update::backends::github::Update::configure()
        .repo_owner(owner)
        .repo_name(repository)
        .bin_name(platform_update_binary_name())
        .bin_path_in_archive(platform_update_binary_filename())
        .asset_identifier(platform_update_asset_identifier())
        .release_tag(format!("v{}", update.version))
        .show_download_progress(true)
        .show_output(false)
        .no_confirm(true)
        .current_version(env!("CARGO_PKG_VERSION"))
        .build()
        .context("could not configure the automatic updater")?
        .update()
        .context("the automatic update could not be installed")?;
    println!(
        "\n\x1b[32;1mUpdate {} installed successfully.\x1b[0m",
        status.version()
    );
    println!("Restart the converter to use the new version.");
    Ok(())
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn platform_update_asset_identifier() -> &'static str {
    "win-x64.exe"
}
#[cfg(all(windows, target_arch = "x86"))]
fn platform_update_asset_identifier() -> &'static str {
    "win-x86.exe"
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn platform_update_asset_identifier() -> &'static str {
    "linux-x64"
}
#[cfg(all(target_os = "linux", target_arch = "x86"))]
fn platform_update_asset_identifier() -> &'static str {
    "linux-x86"
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn platform_update_binary_name() -> &'static str {
    "win-x64"
}
#[cfg(all(windows, target_arch = "x86"))]
fn platform_update_binary_name() -> &'static str {
    "win-x86"
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn platform_update_binary_name() -> &'static str {
    "linux-x64"
}
#[cfg(all(target_os = "linux", target_arch = "x86"))]
fn platform_update_binary_name() -> &'static str {
    "linux-x86"
}

#[cfg(windows)]
fn platform_update_binary_filename() -> &'static str {
    match platform_update_binary_name() {
        "win-x64" => "win-x64.exe",
        "win-x86" => "win-x86.exe",
        _ => unreachable!(),
    }
}

#[cfg(target_os = "linux")]
fn platform_update_binary_filename() -> &'static str {
    platform_update_binary_name()
}

fn render_terminal_header() -> Result<(), Error> {
    let mut stdout = io::stdout();
    execute!(
        stdout,
        Clear(ClearType::All),
        Clear(ClearType::Purge),
        MoveTo(0, 0)
    )
    .context("could not clear the terminal")?;
    print!("\x1b]0;Digital Forge\x07");
    println!("\x1b[38;5;208m========================================================\x1b[0m");
    println!("\x1b[1m                     Digital Forge\x1b[0m");
    println!("\x1b[1m                         2.0\x1b[0m");
    println!("\x1b[38;5;208m========================================================\x1b[0m");
    io::stdout().flush().context("error drawing the terminal")
}

pub(crate) fn render_progress_screen(
    context: &str,
    stage: &str,
    percent: u8,
    detail: Option<&str>,
) {
    store_worker::progress(stage, percent, detail);
    if gui_backend_enabled() {
        report_gui_progress(stage, percent);
        return;
    }
    if !io::stdout().is_terminal() {
        println!("{stage}: {}%", percent.min(100));
        return;
    }
    let _ = render_terminal_header();
    let terminal_width = crossterm::terminal::size()
        .map(|(width, _)| usize::from(width))
        .unwrap_or(80);
    let bar_width = terminal_width.saturating_sub(14).clamp(32, 72);
    let bar = smooth_progress_bar(percent, bar_width);
    println!("{context}\n");
    println!("\x1b[1m{stage}\x1b[0m");
    println!("{bar}  {:>3}%", percent.min(100));
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        println!("\x1b[90m{detail}\x1b[0m");
    }
    let _ = io::stdout().flush();
}

fn render_status_summary() -> Result<(), Error> {
    println!("  Import: Packages, disc images, archives, and game folders");
    println!("  Mode: Manual import only (no Store or console connection required)");
    Ok(())
}

#[derive(Debug, serde::Serialize)]
struct BatchGameInput {
    path: String,
    input_type: String,
    recommended_entry: bool,
}

fn scan_batch_game_inputs(folder: &Path) -> Result<Vec<BatchGameInput>, Error> {
    let folder = fs::canonicalize(folder)
        .with_context(|| format!("could not open game folder {}", folder.display()))?;
    if !folder.is_dir() {
        anyhow::bail!("the supplied batch path is not a folder");
    }
    let mut directories = vec![folder];
    let mut items = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                directories.push(path);
                continue;
            }
            if !entry.file_type()?.is_file() {
                continue;
            }
            let extension = path
                .extension()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if extension == "iso" {
                items.push(BatchGameInput {
                    path: portable_path_text(&path),
                    input_type: "Xbox 360 ISO".to_owned(),
                    recommended_entry: true,
                });
                continue;
            }
            let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if let Some(part) = parse_multipart_part_name(file_name, path.clone()) {
                if part.number == 1 {
                    items.push(BatchGameInput {
                        path: portable_path_text(&path),
                        input_type: "Multipart archive (Part 1)".to_owned(),
                        recommended_entry: true,
                    });
                }
                continue;
            }
            if matches!(extension.as_str(), "zip" | "7z" | "rar") {
                items.push(BatchGameInput {
                    path: portable_path_text(&path),
                    input_type: format!("{} archive", extension.to_ascii_uppercase()),
                    recommended_entry: true,
                });
            }
        }
    }
    items.sort_by(|left, right| {
        left.path
            .to_ascii_lowercase()
            .cmp(&right.path.to_ascii_lowercase())
    });
    Ok(items)
}

fn portable_path_text(path: &Path) -> String {
    let value = path.to_string_lossy();
    value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
}

fn ai_converter_status() -> Result<serde_json::Value, Error> {
    let extractor = find_archive_tool().map(|tool| match tool.kind {
        ArchiveKind::SevenZip => "7-Zip",
        ArchiveKind::WinRar => "WinRAR",
    });
    Ok(serde_json::json!({
        "application": "ISO 2 GOD Converter for XB 360 Games",
        "version": env!("CARGO_PKG_VERSION"),
        "platform": env::consts::OS,
        "architecture": env::consts::ARCH,
        "archiveExtractor": extractor,
        "ftpConfigured": saved_ftp_is_ready()?,
        "passwordExposed": false,
        "supportedInputs": ["iso", "zip", "7z", "rar", "multipart archives", "nested archives"],
        "supportedDestinations": ["removable USB", "Xbox 360 FTP"]
    }))
}

fn ai_inspect_game_input(path: &Path) -> Result<serde_json::Value, Error> {
    let path = fs::canonicalize(path)
        .with_context(|| format!("could not open supplied path {}", path.display()))?;
    if path.is_dir() {
        let items = scan_batch_game_inputs(&path)?;
        return Ok(serde_json::json!({
            "path": portable_path_text(&path),
            "kind": "folder",
            "gameInputs": items,
            "count": items.len()
        }));
    }
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if extension == "iso" {
        return Ok(serde_json::json!({
            "path": portable_path_text(&path),
            "kind": "Xbox 360 ISO candidate",
            "sizeBytes": fs::metadata(&path)?.len()
        }));
    }
    let archive = detect_archive_set(&path)?;
    Ok(serde_json::json!({
        "path": portable_path_text(&path),
        "kind": if archive.multipart { "multipart archive" } else { "archive" },
        "entryPath": archive.entry_path,
        "partCount": archive.part_count,
        "complete": true
    }))
}

fn run_mcp_stdio_server() -> Result<(), Error> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.context("could not read AI bridge request")?;
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": format!("Invalid JSON: {error}")}
                });
                serde_json::to_writer(&mut stdout, &response)?;
                writeln!(stdout)?;
                stdout.flush()?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let method = request
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let response = match method {
            "initialize" => {
                let protocol = request
                    .pointer("/params/protocolVersion")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("2025-06-18");
                mcp_success(
                    id,
                    serde_json::json!({
                        "protocolVersion": protocol,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {
                            "name": "iso2god-xbox360",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "instructions": "Control supported converter operations using background jobs. Act only on paths and destinations authorized by the user. Preserve source archives unless deletion was explicitly requested. Passwords are never returned. Poll job_status until completion; starting a job is not proof of success."
                    }),
                )
            }
            "server/discover" => mcp_success(
                id,
                serde_json::json!({
                    "resultType": "complete",
                    "supportedVersions": ["2025-06-18", "2025-03-26"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "iso2god-xbox360", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Local background converter control for AI clients and developer tools; no terminal menu is required."
                }),
            ),
            "ping" => mcp_success(id, serde_json::json!({})),
            "tools/list" => mcp_success(id, serde_json::json!({"tools": mcp_tool_definitions()})),
            "tools/call" => {
                let name = request
                    .pointer("/params/name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let arguments = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                match call_ai_tool(name, &arguments) {
                    Ok(result) => mcp_success(id, mcp_tool_result(result, false)),
                    Err(error) => mcp_success(
                        id,
                        mcp_tool_result(serde_json::json!({"error": format!("{error:#}")}), true),
                    ),
                }
            }
            _ => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {method}")}
            }),
        };
        serde_json::to_writer(&mut stdout, &response)?;
        writeln!(stdout)?;
        stdout.flush()?;
    }
    Ok(())
}

fn mcp_success(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn mcp_tool_result(value: serde_json::Value, is_error: bool) -> serde_json::Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": is_error
    })
}

fn mcp_tool_definitions() -> Vec<serde_json::Value> {
    let mut tools = vec![
        serde_json::json!({
            "name": "converter_status",
            "title": "Converter Status",
            "description": "Read platform, archive-tool, FTP-readiness and supported-format status without exposing credentials.",
            "inputSchema": {"type": "object", "additionalProperties": false},
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false}
        }),
        serde_json::json!({
            "name": "scan_game_folder",
            "title": "Scan Game Folder",
            "description": "Recursively find possible ISO, archive and Part 1 inputs for one or many Xbox 360 games and return a batch plan for further inspection.",
            "inputSchema": {
                "type": "object",
                "properties": {"folder": {"type": "string", "description": "User-approved local folder path"}},
                "required": ["folder"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false}
        }),
        serde_json::json!({
            "name": "inspect_game_input",
            "title": "Inspect Game Input",
            "description": "Diagnose a supplied ISO, ZIP, 7Z, RAR, multipart archive part or folder and identify its correct entry point.",
            "inputSchema": {
                "type": "object",
                "properties": {"path": {"type": "string", "description": "User-approved file or folder path"}},
                "required": ["path"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false}
        }),
    ];
    tools.extend(automation::tool_definitions());
    tools
}

fn call_ai_tool(name: &str, arguments: &serde_json::Value) -> Result<serde_json::Value, Error> {
    match name {
        "converter_status" => ai_converter_status(),
        "scan_game_folder" => {
            let folder = arguments
                .get("folder")
                .and_then(serde_json::Value::as_str)
                .context("folder must be supplied as a string")?;
            let items = scan_batch_game_inputs(Path::new(folder))?;
            Ok(serde_json::json!({
                "folder": portable_path_text(&fs::canonicalize(folder)?),
                "count": items.len(),
                "gameInputs": items
            }))
        }
        "inspect_game_input" => {
            let path = arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                .context("path must be supplied as a string")?;
            ai_inspect_game_input(Path::new(path))
        }
        _ => automation::call_tool(name, arguments),
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self, Error> {
        enable_raw_mode().context("could not enable arrow-key terminal input")?;
        print!("\x1b[?25l");
        io::stdout()
            .flush()
            .context("error updating the terminal")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        print!("\x1b[?25h");
        let _ = io::stdout().flush();
    }
}

fn select_menu<T: Copy>(options: &[MenuOption<T>]) -> Result<Option<T>, Error> {
    select_menu_with_initial(options, 0)
}

fn select_menu_with_initial<T: Copy>(
    options: &[MenuOption<T>],
    initial: usize,
) -> Result<Option<T>, Error> {
    if env::var_os("ISO2GOD_QUEUE_JOB").is_some() {
        anyhow::bail!("The unattended job requires input; correct its settings and retry.");
    }
    if options.is_empty() {
        anyhow::bail!("the menu has no available options");
    }
    let mut selected = initial.min(options.len() - 1);
    draw_menu(options, selected, false)?;
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
                draw_menu(options, selected, true)?;
            }
            KeyCode::Down => {
                selected = (selected + 1) % options.len();
                draw_menu(options, selected, true)?;
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

fn draw_menu<T>(
    options: &[MenuOption<T>],
    selected: usize,
    move_to_start: bool,
) -> Result<(), Error> {
    if move_to_start {
        print!("\x1b[{}A", options.len());
    }
    for (index, option) in options.iter().enumerate() {
        print!("\r\x1b[2K");
        let label = unnumbered_menu_label(&option.label);
        if index == selected {
            println!("\x1b[38;5;208;1m  > {}\x1b[0m", label);
        } else {
            println!("    {}", label);
        }
    }
    io::stdout().flush().context("error drawing the menu")
}

fn unnumbered_menu_label(label: &str) -> &str {
    if let Some((prefix, remainder)) = label.split_once(". ")
        && !prefix.is_empty()
        && prefix.chars().all(|character| character.is_ascii_digit())
    {
        return remainder;
    }
    label
}

fn saved_ftp_is_ready() -> Result<bool, Error> {
    Ok(load_ftp_settings()?.is_some() && load_saved_password()?.is_some())
}

const DEFAULT_FTP_USERNAME: &str = "xbox";
const DEFAULT_FTP_PASSWORD: &str = "xbox";
const DEFAULT_FTP_PORT: u16 = 21;
const DEFAULT_BACKGROUND_FTP_PORT: u16 = 7564;
const DEFAULT_FTP_DESTINATION: &str = "/Hdd1/Content/0000000000000000/";
const XBOX_GUIDE_REMOTE: &str = "/Hdd1/Plugins/ISO2GOD/ISO2GODGuide.xex";
const XBOX_FTP_REMOTE: &str = "/Hdd1/Plugins/ISO2GOD/ftpdll.xex";
const DASHLAUNCH_GUIDE_PATH: &str = "Hdd:\\Plugins\\ISO2GOD\\ISO2GODGuide.xex";
const DASHLAUNCH_FTP_PATH: &str = "Hdd:\\Plugins\\ISO2GOD\\ftpdll.xex";
const LEGACY_DASHLAUNCH_GUIDE_PATH: &str = "Hdd:\\ISO2GOD\\ISO2GODGuide.xex";
const XBOX_PLUGIN_INSTALL_VERSION: &str = "iso2god-background-ftp-only-2026-09-03";

fn saved_ftp_settings_with_password() -> Result<FtpSettings, Error> {
    if let Some(settings) = store_worker::configured_ftp()? {
        return Ok(settings);
    }
    let mut settings = load_ftp_settings()?.context("Xbox FTP is not configured")?;
    settings.password = load_saved_password()?.context("saved FTP password is unavailable")?;
    Ok(settings)
}

fn configure_ftp_terminal() -> Result<bool, Error> {
    println!("\x1b[1mXbox 360 FTP setup\x1b[0m");
    println!("------------------------------------------------------------");
    println!("Enter the FTP details shown by your Xbox dashboard.\n");

    let existing = load_ftp_settings()?;
    let host_default = existing.as_ref().map(|settings| settings.host.as_str());
    let username_default = existing
        .as_ref()
        .map(|settings| settings.username.as_str())
        .unwrap_or("anonymous");

    let Some(host_text) = prompt_terminal_text("Host/IP address", host_default, false)? else {
        return Ok(false);
    };
    let host = normalise_ftp_host(&host_text)?;
    let Some(username) = prompt_terminal_text(
        "Username (optional; press Enter if none)",
        Some(username_default),
        false,
    )?
    else {
        return Ok(false);
    };
    let username = if username.trim().is_empty() {
        "anonymous".to_owned()
    } else {
        username.trim().to_owned()
    };

    let saved_password = load_saved_password()?;
    let password = if let Some(password) = saved_password {
        let Some(keep_password) = prompt_yes_no_key("Keep the current password?", true)? else {
            return Ok(false);
        };
        if keep_password {
            password
        } else {
            let Some(password) = prompt_optional_terminal_text(
                "Password (optional; press Enter if none)",
                None,
                true,
            )?
            else {
                return Ok(false);
            };
            password
        }
    } else {
        let Some(password) =
            prompt_optional_terminal_text("Password (optional; press Enter if none)", None, true)?
        else {
            return Ok(false);
        };
        password
    };

    let settings = FtpSettings {
        host,
        port: existing
            .as_ref()
            .map(|settings| settings.port)
            .unwrap_or(DEFAULT_FTP_PORT),
        background_port: Some(DEFAULT_BACKGROUND_FTP_PORT),
        username,
        password,
        destination_path: existing
            .as_ref()
            .map(|settings| settings.destination_path.clone())
            .unwrap_or_else(|| DEFAULT_FTP_DESTINATION.to_owned()),
    };
    loop {
        println!("\nPlease wait... testing the FTP connection.");
        let connection_result = RcloneFtp::new(&settings)?.connect_available().map(|_| ());
        match connection_result {
            Ok(()) => break,
            Err(error) => {
                print_terminal_error(&error);
                println!(
                    "\nMake sure the Xbox is turned on, connected to the same network, and its FTP server is running.\n"
                );
                let choices = [
                    MenuOption {
                        value: 0_u8,
                        label: "Retry connection".to_owned(),
                    },
                    MenuOption {
                        value: 1_u8,
                        label: "Edit FTP details".to_owned(),
                    },
                    MenuOption {
                        value: 2_u8,
                        label: "Cancel FTP setup".to_owned(),
                    },
                ];
                match select_menu(&choices)? {
                    Some(0) => continue,
                    Some(1) => return configure_ftp_terminal(),
                    _ => return Ok(false),
                }
            }
        }
    }
    save_ftp_settings(&settings)?;
    save_password(&settings.username, &settings.password)?;
    println!("\x1b[32mConnection successful. FTP settings saved.\x1b[0m");
    println!(
        "For transfers while playing a game, install the background FTP plugin separately (port 7564)."
    );
    Ok(true)
}

fn verify_remote_file(ftp: &RcloneFtp, remote_path: &str, expected: &[u8]) -> Result<(), Error> {
    let actual = ftp.read_file(remote_path)?;
    if actual != expected {
        anyhow::bail!("Xbox plugin verification failed for {remote_path}");
    }
    Ok(())
}

fn remote_missing_error(error: &Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "object not found",
        "file not found",
        "no such file",
        "does not exist",
    ]
    .iter()
    .any(|text| message.contains(text))
}

fn configure_dashlaunch_plugin(contents: &str) -> Result<String, Error> {
    let newline = if contents.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut lines = contents.lines().map(str::to_owned).collect::<Vec<_>>();
    for line in &mut lines {
        if let Some((name, value)) = line.split_once('=')
            && [LEGACY_DASHLAUNCH_GUIDE_PATH, DASHLAUNCH_GUIDE_PATH]
                .iter()
                .any(|path| value.trim().eq_ignore_ascii_case(path))
        {
            *line = format!("{} =", name.trim());
        }
    }
    let section = lines
        .iter()
        .position(|line| line.trim().eq_ignore_ascii_case("[Plugins]"));
    let section_start = if let Some(index) = section {
        index
    } else {
        if !lines.is_empty() && !lines.last().is_some_and(String::is_empty) {
            lines.push(String::new());
        }
        lines.push("[Plugins]".to_owned());
        lines.len() - 1
    };
    let mut section_end = lines
        .iter()
        .enumerate()
        .skip(section_start + 1)
        .find(|(_, line)| line.trim().starts_with('['))
        .map(|(index, _)| index)
        .unwrap_or(lines.len());

    for plugin_path in [DASHLAUNCH_FTP_PATH] {
        if lines.iter().any(|line| {
            line.split_once('=')
                .is_some_and(|(_, value)| value.trim().eq_ignore_ascii_case(plugin_path))
        }) {
            continue;
        }
        let mut installed = false;
        for slot in 1..=5 {
            let key = format!("plugin{slot}");
            let existing = (section_start + 1..section_end).find(|index| {
                lines[*index]
                    .split_once('=')
                    .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case(&key))
            });
            if let Some(index) = existing {
                let empty = lines[index]
                    .split_once('=')
                    .is_none_or(|(_, value)| value.trim().is_empty());
                if empty {
                    lines[index] = format!("{key} = {plugin_path}");
                    installed = true;
                    break;
                }
            } else {
                lines.insert(section_end, format!("{key} = {plugin_path}"));
                section_end += 1;
                installed = true;
                break;
            }
        }
        if !installed {
            anyhow::bail!("DashLaunch does not have a free plugin slot for background FTP");
        }
    }
    Ok(format!("{}{}", lines.join(newline), newline))
}

fn run_terminal_conversion(
    delivery: TerminalDelivery,
    dropped_source: Option<PathBuf>,
) -> Result<(), Error> {
    let selected_usb_drive = if matches!(delivery, TerminalDelivery::Usb) {
        render_terminal_header()?;
        render_status_summary()?;
        println!("\nSearching for removable USB drives...\n");
        let Some(drive) = select_removable_drive()? else {
            return Ok(());
        };
        render_terminal_header()?;
        render_status_summary()?;
        println!("\nSelected USB drive: {}\n", drive.label);
        Some(drive)
    } else {
        None
    };
    let selected_local_folder = if matches!(delivery, TerminalDelivery::Local) {
        println!("\nDrag and drop the destination folder, or type its path.\n");
        let Some(folder) = prompt_dropped_path("Waiting for destination folder", Some(true))?
        else {
            return Ok(());
        };
        Some(folder)
    } else {
        None
    };
    let source = if let Some(source) = dropped_source {
        source
    } else {
        println!("\nDrag and drop an ISO, ZIP, 7Z, RAR, multi-part archive, or archive folder");
        println!("into this terminal. Processing will start automatically.\n");
        let Some(path) = prompt_dropped_game_file()? else {
            return Ok(());
        };
        path
    };

    let archive_workflow = source.is_dir()
        || source
            .extension()
            .and_then(OsStr::to_str)
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("iso"));

    loop {
        render_terminal_header()?;
        println!("\x1b[1mPreparing game\x1b[0m");
        println!("------------------------------------------------------------");
        println!("  Input: {}", source.display());
        println!(
            "  Delivery: {}\n",
            match delivery {
                TerminalDelivery::Ftp => "Xbox 360 FTP",
                TerminalDelivery::Usb => "Removable USB drive",
                TerminalDelivery::Local => "Folder on this PC",
            }
        );

        if archive_workflow {
            let result = (|| {
                let (source_folder, sets, remove_parts_after_success) =
                    multipart_sets_for_dropped_input(&source)?;
                let destination = match delivery {
                    TerminalDelivery::Ftp => MultipartDestination::Ftp,
                    TerminalDelivery::Usb => MultipartDestination::Usb(
                        selected_usb_drive
                            .clone()
                            .context("USB transfer was cancelled")?,
                    ),
                    TerminalDelivery::Local => MultipartDestination::Local(
                        selected_local_folder
                            .clone()
                            .context("folder selection was cancelled")?,
                    ),
                };
                show_import_stage("Scanning dropped folder", 2);
                multipart_game_import_once(
                    &source_folder,
                    sets,
                    &destination,
                    remove_parts_after_success,
                    false,
                )
            })();

            match result {
                Ok(_) => {
                    println!(
                        "\n\x1b[32;1mComplete. The game was prepared and verified successfully.\x1b[0m"
                    );
                    pause_to_continue()?;
                    return Ok(());
                }
                Err(error) => {
                    if env::var_os("ISO2GOD_QUEUE_JOB").is_some() {
                        return Err(error);
                    }
                    print_terminal_error(&error);
                    println!();
                    let choices = [
                        MenuOption {
                            value: true,
                            label: "Retry".to_owned(),
                        },
                        MenuOption {
                            value: false,
                            label: "Return to main menu".to_owned(),
                        },
                    ];
                    if !select_menu(&choices)?.unwrap_or(false) {
                        return Ok(());
                    }
                    continue;
                }
            }
        }

        let mut work = TempDir::new()?;
        let destination = work.path().join("converted");
        let args = Cli {
            source_iso: source.clone(),
            dest_dir: destination,
            dry_run: false,
            game_title: None,
            trim: None,
            num_threads: 1,
            upload_ftp: false,
            copy_usb: matches!(delivery, TerminalDelivery::Usb),
            use_saved_ftp: matches!(delivery, TerminalDelivery::Ftp),
            prompt_delivery: false,
            selected_usb_drive: selected_usb_drive.clone(),
        };

        let result = if matches!(delivery, TerminalDelivery::Local) {
            (|| {
                let output = run(args)?;
                verify_converted_game(&output)?;
                let folder = selected_local_folder
                    .as_ref()
                    .context("folder selection was cancelled")?;
                let destination = folder
                    .join("Content")
                    .join("0000000000000000")
                    .join(&output.title_id);
                copy_directory_and_verify(&output.local_title_dir, &destination, "Copying game")?;
                Ok(())
            })()
        } else {
            convert_and_maybe_upload(args)
        };

        match result {
            Ok(()) => {
                work.cleanup = true;
                drop(work);
                println!(
                    "\n\x1b[32;1mComplete. The converted game was verified successfully.\x1b[0m"
                );
                pause_to_continue()?;
                return Ok(());
            }
            Err(error) => {
                if env::var_os("ISO2GOD_QUEUE_JOB").is_some() {
                    return Err(error);
                }
                print_terminal_error(&error);
                drop(work);
                println!();
                let choices = [
                    MenuOption {
                        value: true,
                        label: "Retry".to_owned(),
                    },
                    MenuOption {
                        value: false,
                        label: "Return to main menu".to_owned(),
                    },
                ];
                if !select_menu(&choices)?.unwrap_or(false) {
                    return Ok(());
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct MultipartGameSet {
    game_name: String,
    entry_path: PathBuf,
    parts: Vec<PathBuf>,
    missing_part: Option<String>,
}

#[derive(Debug)]
struct MultipartPart {
    key: String,
    game_name: String,
    number: u32,
    width: usize,
    prefix: String,
    suffix: String,
    path: PathBuf,
}

enum DetectedXboxGame {
    Iso(PathBuf),
    God {
        title_dir: PathBuf,
        title_id: String,
    },
    Jtag(PathBuf),
}

enum PreparedGameKind {
    God {
        title_dir: PathBuf,
        title_id: String,
    },
    Jtag {
        title_id: String,
    },
}

struct PreparedGame {
    root: PathBuf,
    name: String,
    kind: PreparedGameKind,
}

enum MultipartDestination {
    Usb(RemovableDrive),
    Ftp,
    Local(PathBuf),
}

fn run_multipart_game_import() -> Result<(), Error> {
    render_terminal_header()?;
    println!("\x1b[1mMultipart Game Import\x1b[0m");
    println!("------------------------------------------------------------");
    println!(
        "Import a complete game from split archive parts and prepare it automatically for your Xbox 360."
    );
    println!("\nChoose where the completed game will be saved:\n");
    let destinations = [
        MenuOption {
            value: 0_u8,
            label: "1. Save to a removable USB drive".to_owned(),
        },
        MenuOption {
            value: 1_u8,
            label: "2. Save to a folder on this PC".to_owned(),
        },
    ];
    let Some(destination) = select_menu(&destinations)? else {
        return Ok(());
    };
    let destination = if destination == 0 {
        render_terminal_header()?;
        println!("Searching for removable USB drives...\n");
        let Some(drive) = select_removable_drive()? else {
            return Ok(());
        };
        MultipartDestination::Usb(drive)
    } else if destination == 1 {
        println!("\nDrag and drop the destination folder, or type its path.\n");
        let Some(folder) = prompt_dropped_path("Waiting for destination folder", Some(true))?
        else {
            return Ok(());
        };
        MultipartDestination::Local(folder)
    } else {
        unreachable!("multipart destination menu returned an invalid option")
    };

    render_terminal_header()?;
    println!("\x1b[1mMultipart Game Import\x1b[0m");
    println!("------------------------------------------------------------");
    println!("Drag and drop the folder, Part 1, or archive file.");
    println!("Processing will start automatically as soon as it is dropped.\n");
    let Some(source_input) = prompt_dropped_archive_input()? else {
        return Ok(());
    };
    let (source_folder, initial_sets, remove_parts_after_success) =
        multipart_sets_for_dropped_input(&source_input)?;

    loop {
        render_terminal_header()?;
        println!("\x1b[1mMultipart Game Import\x1b[0m");
        println!("------------------------------------------------------------");
        println!(
            "Import a complete game from split archive parts and prepare it automatically for your Xbox 360."
        );
        println!("\nInput:");
        println!("  {}\n", source_input.display());

        show_import_stage("Scanning dropped folder", 2);
        let sets = if source_input.is_dir() {
            scan_archive_sets(&source_folder)?
        } else {
            initial_sets.clone()
        };

        match multipart_game_import_once(
            &source_folder,
            sets,
            &destination,
            remove_parts_after_success,
            true,
        ) {
            Ok(_) => {
                pause_to_continue()?;
                return Ok(());
            }
            Err(error) => {
                print_terminal_error(&error);
                println!(
                    "\nEvery archive part that was not already verified and completed has been preserved.\n"
                );
                let choices = [
                    MenuOption {
                        value: true,
                        label: "Retry Multipart Game Import".to_owned(),
                    },
                    MenuOption {
                        value: false,
                        label: "Return to main menu".to_owned(),
                    },
                ];
                if !select_menu(&choices)?.unwrap_or(false) {
                    return Ok(());
                }
            }
        }
    }
}

fn multipart_game_import_once(
    source_folder: &Path,
    sets: Vec<MultipartGameSet>,
    destination: &MultipartDestination,
    remove_parts_after_success: bool,
    keep_prepared_output: bool,
) -> Result<PreparedGame, Error> {
    if sets.is_empty() {
        anyhow::bail!(
            "no RAR, ZIP, or 7Z archives were found in the dropped folder or its subfolders: {}",
            source_folder.display()
        );
    }
    show_import_stage("Detecting archive parts", 6);
    let set = choose_multipart_game_set(sets)?;

    show_import_stage("Checking for missing parts", 10);
    if let Some(missing) = &set.missing_part {
        anyhow::bail!(
            "Archive part {} of {} is missing",
            missing
                .chars()
                .filter(|character| character.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
                .unwrap_or(0),
            set.parts.len().saturating_add(1)
        );
    }
    if set.parts.is_empty() || !set.entry_path.is_file() {
        anyhow::bail!("Cannot continue: Part 1 is missing from the selected archive set");
    }
    println!(
        "Found {} related archive file(s); no gaps in the discovered volume numbers. Archive integrity is checked during extraction.",
        set.parts.len()
    );

    let seven_zip = require_7zip_for_multipart()?;
    show_import_stage("Checking available storage", 14);
    let unpacked_size = list_archive_unpacked_size(&seven_zip, &set.entry_path)?;
    let archive_size = set.parts.iter().try_fold(0_u64, |total, path| {
        Ok::<_, Error>(total.saturating_add(fs::metadata(path)?.len()))
    })?;
    let estimated_output = unpacked_size.max(archive_size);
    let safety_margin = (estimated_output / 10).max(512 * 1024 * 1024);
    let required = estimated_output.saturating_add(safety_margin);
    let available =
        fs2::available_space(source_folder).context("could not check free storage space")?;
    println!(
        "  Estimated extraction: {}\n  Required with safety margin: {}\n  Available: {}",
        format_byte_size(estimated_output),
        format_byte_size(required),
        format_byte_size(available)
    );
    if available < required {
        anyhow::bail!(
            "Insufficient disk space ({} required, {} available)",
            format_byte_size(required),
            format_byte_size(available)
        );
    }

    let staging = unique_path(source_folder, &format!(".iso2god-import-{}", set.game_name));
    fs::create_dir(&staging)
        .with_context(|| format!("could not create extraction folder {}", staging.display()))?;
    show_import_stage("Extracting complete game", 18);
    println!("Beginning with Part 1: {}", set.entry_path.display());
    let extraction_progress = ProgressDisplay::with_total(unpacked_size);
    extraction_progress.update("Extracting complete game", 0);
    let status = extract_with_7zip(&seven_zip, &set.entry_path, &staging, &extraction_progress)?;
    if !status.success() {
        anyhow::bail!(
            "7-Zip could not extract the complete archive set (exit status {status}); all numbered parts were preserved"
        );
    }
    extraction_progress.finish("Extraction complete");

    show_import_stage("Verifying extracted files", 43);
    verify_nonempty_directory(&staging)?;
    show_import_stage("Detecting Xbox 360 format", 48);
    let detected = detect_xbox_game_with_nested_archives(&staging, &seven_zip)?;

    show_import_stage(
        match &detected {
            DetectedXboxGame::Iso(_) => "Converting Xbox 360 ISO",
            DetectedXboxGame::God { .. } => "Preparing Games on Demand package",
            DetectedXboxGame::Jtag(_) => "Preparing extracted Xbox 360 game",
        },
        53,
    );
    let prepared = prepare_detected_game(source_folder, &set.game_name, detected)?;
    verify_prepared_game(&prepared)?;
    remember_prepared_output(&prepared);

    show_import_stage("Preserving source archives until delivery succeeds", 70);
    match destination {
        MultipartDestination::Usb(drive) => copy_prepared_game_to_selected_drive(&prepared, drive)?,
        MultipartDestination::Ftp => upload_prepared_game_to_xbox(&prepared)?,
        MultipartDestination::Local(folder) => copy_prepared_game_to_folder(&prepared, folder)?,
    }

    if remove_parts_after_success && set.parts.len() > 1 {
        // Recursive discovery may select a set in a subfolder. Only that set's
        // own directory is eligible for cleanup, after successful delivery.
        let part_folder = set
            .entry_path
            .parent()
            .context("archive has no parent folder")?;
        if !fs::canonicalize(part_folder)?.starts_with(fs::canonicalize(source_folder)?) {
            anyhow::bail!("refusing archive cleanup outside the dropped folder");
        }
        remove_verified_archive_parts(part_folder, &set.parts)?;
    }

    if staging.is_dir() && staging != prepared.root {
        if let Err(error) = fs::remove_dir_all(&staging) {
            eprintln!(
                "Warning: the verified transfer is complete, but extraction leftovers could not be removed from {}: {error}",
                staging.display()
            );
        }
    }

    show_import_stage("Complete", 100);
    if keep_prepared_output {
        println!("\nGame prepared and verified successfully at:");
        println!("  {}", prepared.root.display());
    } else {
        println!("\nGame prepared, transferred and verified successfully.");
        println!("Removing temporary prepared files...");
        if let Err(error) = fs::remove_dir_all(&prepared.root) {
            eprintln!(
                "Warning: the verified transfer is complete, but temporary prepared files could not be removed from {}: {error}",
                prepared.root.display()
            );
        }
    }
    Ok(prepared)
}

fn detect_xbox_game_with_nested_archives(
    extracted_root: &Path,
    seven_zip: &Path,
) -> Result<DetectedXboxGame, Error> {
    // A real extracted game often contains ZIP files as game assets. Detect a
    // usable Xbox layout before considering any archive to be another wrapper.
    if let Some(detected) = try_detect_extracted_xbox_game(extracted_root)? {
        return Ok(detected);
    }

    // Queue every branch, not just the first archive encountered. Each newly
    // extracted directory is scanned once; source archives are never requeued.
    let mut pending = std::collections::VecDeque::from(scan_archive_sets(extracted_root)?);
    let mut detected_game = None;
    let mut extracted_count = 0_u64;
    while let Some(nested) = pending.pop_front() {
        if let Some(missing) = &nested.missing_part {
            anyhow::bail!(
                "Cannot continue: nested archive part `{missing}` is missing beside {}",
                nested.entry_path.display()
            );
        }
        println!("Nested archive detected: {}", nested.entry_path.display());
        let unpacked_size = list_archive_unpacked_size(seven_zip, &nested.entry_path)?;
        let required = unpacked_size.saturating_add(512 * 1024 * 1024);
        let available = fs2::available_space(extracted_root)?;
        if available < required {
            anyhow::bail!(
                "not enough free storage for nested archive {} ({} required, {} available)",
                nested.entry_path.display(),
                format_byte_size(required),
                format_byte_size(available)
            );
        }
        extracted_count += 1;
        // Keep destinations shallow even when the archive chain is very deep.
        let destination = unique_path(
            extracted_root,
            &format!(".iso2god-nested-extraction-{extracted_count}"),
        );
        fs::create_dir(&destination).with_context(|| {
            format!(
                "could not create nested extraction folder {}",
                destination.display()
            )
        })?;
        show_import_stage("Extracting nested archive", 46);
        let progress = ProgressDisplay::with_total(unpacked_size);
        progress.update("Extracting nested archive", 0);
        let status = extract_with_7zip(seven_zip, &nested.entry_path, &destination, &progress)?;
        if !status.success() {
            anyhow::bail!(
                "7-Zip could not extract nested archive {} (exit status {status}); originals preserved",
                nested.entry_path.display()
            );
        }
        progress.finish("Nested extraction complete");
        if let Some(detected) = try_detect_extracted_xbox_game(&destination)? {
            // Do not inspect archives inside a detected game: titles commonly
            // contain asset ZIPs using codecs that general-purpose 7-Zip does
            // not support. Keep processing archives already queued from sibling
            // branches so a damaged wrapper cannot be silently concealed.
            if detected_game.is_none() {
                detected_game = Some(detected);
            }
            continue;
        }
        pending.extend(scan_archive_sets(&destination)?);
    }
    if let Some(detected) = detected_game {
        return Ok(detected);
    }
    show_import_stage("Detecting Xbox 360 format", 48);
    detect_extracted_xbox_game(extracted_root)
}

fn scan_archive_sets(root: &Path) -> Result<Vec<MultipartGameSet>, Error> {
    let mut directories = vec![root.to_path_buf()];
    let mut sets = Vec::new();
    while let Some(directory) = directories.pop() {
        sets.extend(scan_archive_sets_in_directory(&directory)?);
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            // Do not follow symlinks/junctions into cycles or outside this tree.
            if entry.file_type()?.is_dir() && !entry.path().is_symlink() {
                directories.push(entry.path());
            }
        }
    }
    sets.sort_by(|a, b| a.entry_path.cmp(&b.entry_path));
    Ok(sets)
}

fn scan_archive_sets_in_directory(directory: &Path) -> Result<Vec<MultipartGameSet>, Error> {
    let mut sets = scan_multipart_game_sets(directory)?;
    let mut seen = sets
        .iter()
        .flat_map(|set| set.parts.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            files.push(entry.path());
        }
    }
    files.sort();
    for file in &files {
        if seen.contains(file) {
            continue;
        }
        let extension = file
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let legacy_volume = extension.len() == 3
            && matches!(extension.as_bytes()[0], b'r' | b'z')
            && extension.as_bytes()[1..].iter().all(u8::is_ascii_digit);
        if !matches!(extension.as_str(), "zip" | "7z" | "rar") && !legacy_volume {
            continue;
        }
        let archive = detect_archive_set(file)
            .with_context(|| format!("could not identify archive {}", file.display()))?;
        if seen.contains(&archive.entry_path) {
            continue;
        }
        let mut parts = vec![archive.entry_path.clone()];
        if archive.multipart {
            let base = archive
                .entry_path
                .file_stem()
                .and_then(OsStr::to_str)
                .context("archive name is not valid Unicode")?
                .to_ascii_lowercase();
            let marker = if archive
                .entry_path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rar"))
            {
                "r"
            } else {
                "z"
            };
            let prefix = format!("{base}.{marker}");
            parts.extend(
                files
                    .iter()
                    .filter(|path| {
                        path.file_name()
                            .and_then(OsStr::to_str)
                            .is_some_and(|name| {
                                let name = name.to_ascii_lowercase();
                                name.strip_prefix(&prefix).is_some_and(|digits| {
                                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                                })
                            })
                    })
                    .cloned(),
            );
        }
        seen.extend(parts.clone());
        sets.push(MultipartGameSet {
            game_name: sanitize_game_name(
                archive
                    .entry_path
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .context("archive name is not valid Unicode")?,
            ),
            entry_path: archive.entry_path,
            parts,
            missing_part: None,
        });
    }
    Ok(sets)
}

fn multipart_sets_for_dropped_input(
    source: &Path,
) -> Result<(PathBuf, Vec<MultipartGameSet>, bool), Error> {
    if source.is_dir() {
        return Ok((source.to_path_buf(), scan_archive_sets(source)?, true));
    }
    let parent = source
        .parent()
        .context("the dropped archive does not have a parent folder")?
        .to_path_buf();
    let canonical_source = fs::canonicalize(source)?;
    // Inspect only this file's set; an unrelated damaged set must not prevent
    // importing an explicitly dropped archive.
    if source
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| parse_multipart_part_name(name, source.to_path_buf()).is_some())
    {
        let mut sets = scan_multipart_game_sets(&parent)?;
        sets.retain(|set| {
            set.parts
                .iter()
                .any(|part| fs::canonicalize(part).is_ok_and(|path| path == canonical_source))
        });
        if sets.is_empty() {
            anyhow::bail!("the numbered archive set could not be identified");
        }
        return Ok((parent, sets, true));
    }
    let archive = detect_archive_set(source)?;
    if archive.multipart {
        let mut sets = scan_archive_sets_in_directory(&parent)?;
        sets.retain(|set| set.entry_path == archive.entry_path);
        return Ok((parent, sets, true));
    }
    let game_name = source
        .file_stem()
        .and_then(OsStr::to_str)
        .map(sanitize_game_name)
        .context("the dropped archive name is not valid Unicode")?;
    Ok((
        parent,
        vec![MultipartGameSet {
            game_name,
            entry_path: archive.entry_path,
            parts: vec![source.to_path_buf()],
            missing_part: None,
        }],
        false,
    ))
}

fn show_import_stage(stage: &str, percent: u8) {
    report_gui_stage(stage, percent);
    if !gui_backend_enabled() {
        render_progress_screen("Preparing game", stage, percent, None);
    }
}

fn scan_multipart_game_sets(games: &Path) -> Result<Vec<MultipartGameSet>, Error> {
    let mut grouped = BTreeMap::<String, Vec<MultipartPart>>::new();
    for entry in
        fs::read_dir(games).with_context(|| format!("could not scan {}", games.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Some(part) = parse_multipart_part_name(&file_name, entry.path()) {
            grouped.entry(part.key.clone()).or_default().push(part);
        }
    }

    let mut sets = Vec::new();
    for mut parts in grouped.into_values() {
        parts.sort_by_key(|part| part.number);
        let first = parts
            .first()
            .context("multipart group was unexpectedly empty")?;
        let width = parts.iter().map(|part| part.width).max().unwrap_or(1);
        let max_part = parts.iter().map(|part| part.number).max().unwrap_or(1);
        let mut by_number = BTreeMap::new();
        for part in &parts {
            if by_number.insert(part.number, part.path.clone()).is_some() {
                anyhow::bail!(
                    "duplicate archive volume number {} was found for {}",
                    part.number,
                    first.game_name
                );
            }
        }
        let missing_number = (1..=max_part).find(|number| !by_number.contains_key(number));
        let missing_part = missing_number.map(|number| {
            format!(
                "{}{number:0width$}{}",
                first.prefix,
                first.suffix,
                width = width
            )
        });
        let entry_path = by_number.get(&1).cloned().unwrap_or_else(|| {
            games.join(format!(
                "{}{:0width$}{}",
                first.prefix,
                1,
                first.suffix,
                width = width
            ))
        });
        sets.push(MultipartGameSet {
            game_name: sanitize_game_name(&first.game_name),
            entry_path,
            parts: parts.into_iter().map(|part| part.path).collect(),
            missing_part,
        });
    }
    Ok(sets)
}

fn parse_multipart_part_name(file_name: &str, path: PathBuf) -> Option<MultipartPart> {
    let lower = file_name.to_ascii_lowercase();
    let extension_index = lower.rfind('.')?;
    let extension = &lower[extension_index..];
    if matches!(extension, ".rar" | ".zip" | ".7z") {
        let stem = &lower[..extension_index];
        let trimmed_stem = stem.trim_start_matches('.');
        let part_info = if let Some(part_index) = stem.rfind(".part") {
            if part_index == 0 {
                Some((0, 5, "part".to_owned()))
            } else {
                Some((
                    part_index,
                    part_index + 5,
                    file_name[..part_index].to_owned(),
                ))
            }
        } else if let Some(part_index) = stem.rfind("_part") {
            Some((
                part_index,
                part_index + 5,
                file_name[..part_index].to_owned(),
            ))
        } else if let Some(part_index) = stem.rfind("-part") {
            Some((
                part_index,
                part_index + 5,
                file_name[..part_index].to_owned(),
            ))
        } else if stem.starts_with("part") {
            Some((0, 4, "part".to_owned()))
        } else if trimmed_stem.starts_with("part") {
            let offset = stem.len() - trimmed_stem.len();
            Some((offset, offset + 4, "part".to_owned()))
        } else {
            None
        };
        if let Some((_, prefix_len, game_name)) = part_info {
            let digits = &file_name[prefix_len..extension_index];
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                let number = digits.parse().ok()?;
                let raw_prefix = file_name[..prefix_len].to_owned();
                let prefix = if game_name == "part" && raw_prefix.starts_with('.') {
                    raw_prefix.trim_start_matches('.').to_owned()
                } else {
                    raw_prefix
                };
                let suffix = file_name[extension_index..].to_owned();
                return Some(MultipartPart {
                    key: format!("part:{}{}", prefix.to_ascii_lowercase(), extension),
                    game_name,
                    number,
                    width: digits.len(),
                    prefix,
                    suffix,
                    path,
                });
            }
        }
    }

    let digits = &file_name[extension_index + 1..];
    if digits.len() < 2 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let numbered_base = &file_name[..extension_index];
    let lower_base = &lower[..extension_index];
    if !lower_base.ends_with(".7z") && !lower_base.ends_with(".zip") && digits.len() != 3 {
        return None;
    }
    let game_name = if lower_base.ends_with(".7z") {
        numbered_base[..numbered_base.len() - 3].to_owned()
    } else if lower_base.ends_with(".zip") {
        numbered_base[..numbered_base.len() - 4].to_owned()
    } else {
        numbered_base.to_owned()
    };
    Some(MultipartPart {
        key: format!("numeric:{}.", lower_base),
        game_name,
        number: digits.parse().ok()?,
        width: digits.len(),
        prefix: file_name[..extension_index + 1].to_owned(),
        suffix: String::new(),
        path,
    })
}

fn choose_multipart_game_set(sets: Vec<MultipartGameSet>) -> Result<MultipartGameSet, Error> {
    if sets.len() == 1 {
        return Ok(sets.into_iter().next().unwrap());
    }
    println!("\nMultiple game archives were detected:\n");
    let options = sets
        .iter()
        .enumerate()
        .map(|(index, set)| MenuOption {
            value: index,
            label: format!(
                "{}. {} ({} files) - {}",
                index + 1,
                set.game_name,
                set.parts.len(),
                set.entry_path.display()
            ),
        })
        .collect::<Vec<_>>();
    let selected = select_menu(&options)?.context("multipart import was cancelled")?;
    Ok(sets.into_iter().nth(selected).unwrap())
}

fn sanitize_game_name(name: &str) -> String {
    let cleaned = name
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
    let cleaned = cleaned.trim().trim_matches(['.', ' ']);
    if cleaned.is_empty() {
        "Imported Game".to_owned()
    } else {
        cleaned.to_owned()
    }
}

fn require_7zip_for_multipart() -> Result<PathBuf, Error> {
    if let Some(path) = find_7zip() {
        return Ok(path);
    }
    println!("\nMultipart Game Import requires the 7-Zip command-line executable.");
    if let Some(installer) = archive_install_method() {
        if prompt_yes_no_key(
            &format!("Install 7-Zip automatically using {installer}?"),
            true,
        )?
        .unwrap_or(false)
        {
            println!("\nPlease wait while {installer} installs 7-Zip.");
            if let Ok(path) = install_7zip_automatically() {
                return Ok(path);
            }
            println!("Automatic installation did not complete.");
        }
    }
    if prompt_yes_no_key("Open the official 7-Zip download page?", true)?.unwrap_or(false) {
        open_url_in_default_browser(SEVEN_ZIP_DOWNLOAD_URL)?;
    }
    anyhow::bail!("7-Zip is required for Multipart Game Import; install it and retry")
}

fn list_archive_unpacked_size(seven_zip: &Path, part_one: &Path) -> Result<u64, Error> {
    let mut command = Command::new(seven_zip);
    command.args(["l", "-slt", "-sccUTF-8"]);
    if let Some(password) = current_archive_password() {
        command.arg(format!("-p{password}"));
    }
    let output = command
        .arg("--")
        .arg(part_one)
        .stdin(Stdio::null())
        .output()
        .context("could not inspect the multipart archive with 7-Zip")?;
    if !output.status.success() {
        let details = archive_diagnostics(&output.stdout, &output.stderr);
        if details.to_ascii_lowercase().contains("wrong password") {
            anyhow::bail!("Archive password is incorrect");
        }
        anyhow::bail!(
            "7-Zip could not inspect {} ({}). This does not by itself mean a numbered part is missing. The archive may be damaged, incomplete, encrypted, or unreadable. Originals were preserved.\n{details}",
            part_one.display(),
            output.status
        );
    }
    let size = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Size = "))
        .filter_map(|value| value.trim().parse::<u64>().ok())
        .fold(0_u64, u64::saturating_add);
    if size == 0 {
        anyhow::bail!("7-Zip reported that the multipart archive contains no extractable data");
    }
    Ok(size)
}

fn archive_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        "7-Zip returned no diagnostic text.".to_owned()
    } else if lines
        .iter()
        .any(|line| line.to_ascii_lowercase().contains("unsupported method"))
    {
        let affected = lines
            .iter()
            .filter(|line| line.to_ascii_lowercase().contains("unsupported method"))
            .count();
        let example = lines
            .iter()
            .find(|line| line.to_ascii_lowercase().contains("unsupported method"))
            .map(|line| line.trim_start_matches("ERROR: "))
            .unwrap_or("Unsupported archive method");
        format!(
            "The archive uses a compression method that this 7-Zip build cannot read ({affected} affected file(s)).\nFirst affected entry: {example}"
        )
    } else {
        let mut selected = Vec::new();
        for line in lines {
            let line = line.chars().take(300).collect::<String>();
            if !selected.contains(&line) {
                selected.push(line);
            }
            if selected.len() == 6 {
                break;
            }
        }
        selected.join("\n")
    }
}

fn verify_nonempty_directory(root: &Path) -> Result<(), Error> {
    let files = collect_upload_files(root)?;
    let total = files.iter().map(|file| file.size).sum::<u64>();
    if files.is_empty() || total == 0 {
        anyhow::bail!("the extracted game folder is empty or contains no usable data");
    }
    Ok(())
}

fn detect_extracted_xbox_game(root: &Path) -> Result<DetectedXboxGame, Error> {
    try_detect_extracted_xbox_game(root)?
        .context("No supported Xbox 360 game structure was found after extraction")
}

fn try_detect_extracted_xbox_game(root: &Path) -> Result<Option<DetectedXboxGame>, Error> {
    if let Some((title_dir, title_id)) = find_existing_god_package(root)? {
        return Ok(Some(DetectedXboxGame::God {
            title_dir,
            title_id,
        }));
    }
    if let Some(game_dir) = find_default_xex_game(root)? {
        return Ok(Some(DetectedXboxGame::Jtag(game_dir)));
    }
    if let Some(iso) = choose_extracted_iso(root)? {
        return Ok(Some(DetectedXboxGame::Iso(iso)));
    }
    Ok(None)
}

fn find_valid_xbox_isos(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut directories = vec![root.to_path_buf()];
    let mut isos = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("error reading extracted folder {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                directories.push(path);
                continue;
            }
            if !entry.file_type()?.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            if name.ends_with(".txt")
                || name.ends_with(".url")
                || name.ends_with(".lnk")
                || name.ends_with(".jpg")
                || name.ends_with(".png")
                || name.ends_with(".nfo")
                || name.ends_with(".html")
            {
                continue;
            }
            if path
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("iso"))
            {
                if File::open(&path)
                    .ok()
                    .and_then(|file| iso::IsoReader::read(file).ok())
                    .is_some()
                {
                    isos.push(path);
                }
            }
        }
    }
    isos.sort();
    Ok(isos)
}

fn choose_extracted_iso(root: &Path) -> Result<Option<PathBuf>, Error> {
    let isos = find_valid_xbox_isos(root)?;
    match isos.len() {
        0 => Ok(find_largest_iso(root)?),
        1 => Ok(isos.into_iter().next()),
        _ => {
            println!("\nMultiple Xbox 360 ISO files were found. Choose which one to process:\n");
            let options = isos
                .iter()
                .enumerate()
                .map(|(index, path)| MenuOption {
                    value: index,
                    label: format!(
                        "{}. {}",
                        index + 1,
                        path.file_name()
                            .and_then(OsStr::to_str)
                            .unwrap_or("game.iso")
                    ),
                })
                .collect::<Vec<_>>();
            let Some(index) = select_menu(&options)? else {
                return Ok(None);
            };
            Ok(isos.into_iter().nth(index))
        }
    }
}

fn find_existing_god_package(root: &Path) -> Result<Option<(PathBuf, String)>, Error> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let path = entry.path();
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("00007000"))
                && let Some(title_dir) = path.parent()
                && let Some(title_id) = title_dir.file_name().and_then(OsStr::to_str)
                && title_id.len() == 8
                && title_id.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                let output = ConversionOutput {
                    local_title_dir: title_dir.to_path_buf(),
                    title_id: title_id.to_ascii_uppercase(),
                    extracted_temp: None,
                };
                if verify_converted_game(&output).is_ok() {
                    return Ok(Some((output.local_title_dir, output.title_id)));
                }
            }
            directories.push(path);
        }
    }
    Ok(None)
}

fn find_default_xex_game(root: &Path) -> Result<Option<PathBuf>, Error> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                directories.push(path);
            } else if entry.file_type()?.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("default.xex"))
                && entry.metadata()?.len() > 0
            {
                return Ok(path.parent().map(Path::to_path_buf));
            }
        }
    }
    Ok(None)
}

fn prepare_detected_game(
    games: &Path,
    archive_name: &str,
    detected: DetectedXboxGame,
) -> Result<PreparedGame, Error> {
    match detected {
        DetectedXboxGame::Iso(iso) => {
            let iso_size = fs::metadata(&iso)?.len();
            let available = fs2::available_space(games)?;
            if available < iso_size.saturating_add(512 * 1024 * 1024) {
                anyhow::bail!("not enough free storage to convert the extracted ISO to GOD");
            }
            let root = unique_path(games, archive_name);
            let god_base = root.join("Content").join("0000000000000000");
            fs::create_dir_all(&god_base)?;
            let output = run(Cli {
                source_iso: iso,
                dest_dir: god_base,
                dry_run: false,
                game_title: Some(archive_name.to_owned()),
                trim: None,
                num_threads: 1,
                upload_ftp: false,
                copy_usb: false,
                use_saved_ftp: false,
                prompt_delivery: false,
                selected_usb_drive: None,
            })?;
            verify_converted_game(&output)?;
            Ok(PreparedGame {
                root,
                name: archive_name.to_owned(),
                kind: PreparedGameKind::God {
                    title_dir: output.local_title_dir,
                    title_id: output.title_id,
                },
            })
        }
        DetectedXboxGame::God {
            title_dir,
            title_id,
        } => {
            let root = unique_path(games, archive_name);
            let destination = root
                .join("Content")
                .join("0000000000000000")
                .join(&title_id);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::rename(&title_dir, &destination).with_context(|| {
                format!(
                    "could not move the verified GOD package into {}",
                    destination.display()
                )
            })?;
            Ok(PreparedGame {
                root,
                name: archive_name.to_owned(),
                kind: PreparedGameKind::God {
                    title_dir: destination,
                    title_id,
                },
            })
        }
        DetectedXboxGame::Jtag(game_dir) => {
            let detected_name = game_dir
                .file_name()
                .and_then(OsStr::to_str)
                .filter(|name| !name.starts_with(".iso2god-import-"))
                .map(sanitize_game_name)
                .unwrap_or_else(|| archive_name.to_owned());
            let root = unique_path(games, &detected_name);
            fs::rename(&game_dir, &root).with_context(|| {
                format!("could not move the extracted game into {}", root.display())
            })?;
            let title_id = read_prepared_jtag_title_id(&root)?;
            Ok(PreparedGame {
                root,
                name: detected_name,
                kind: PreparedGameKind::Jtag { title_id },
            })
        }
    }
}

fn read_prepared_jtag_title_id(root: &Path) -> Result<String, Error> {
    let default_xex = fs::read_dir(root)?
        .filter_map(Result::ok)
        .find(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("default.xex"))
        })
        .context("the prepared JTAG/RGH game is missing default.xex")?;
    let header = iso2god::executable::xex::XexHeader::read(File::open(default_xex.path())?)
        .context("the prepared default.xex has an invalid Xbox 360 header")?;
    let execution = header
        .fields
        .execution_info
        .context("the prepared default.xex does not contain an Xbox Title ID")?;
    Ok(format!("{:08X}", execution.title_id))
}

fn verify_prepared_game(game: &PreparedGame) -> Result<(), Error> {
    match &game.kind {
        PreparedGameKind::God {
            title_dir,
            title_id,
        } => verify_converted_game(&ConversionOutput {
            local_title_dir: title_dir.clone(),
            title_id: title_id.clone(),
            extracted_temp: None,
        }),
        PreparedGameKind::Jtag { title_id } => {
            verify_nonempty_directory(&game.root)?;
            let default_xex = fs::read_dir(&game.root)?
                .filter_map(Result::ok)
                .find(|entry| {
                    entry.file_type().is_ok_and(|kind| kind.is_file())
                        && entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.eq_ignore_ascii_case("default.xex"))
                })
                .context("the prepared JTAG/RGH game is missing default.xex")?;
            if default_xex.metadata()?.len() == 0 {
                anyhow::bail!("the prepared default.xex file is empty");
            }
            let detected_title_id = read_prepared_jtag_title_id(&game.root)?;
            if !detected_title_id.eq_ignore_ascii_case(title_id) {
                anyhow::bail!(
                    "the prepared game changed Title ID (expected {title_id}, found {detected_title_id})"
                );
            }
            Ok(())
        }
    }
}

fn unique_path(parent: &Path, name: &str) -> PathBuf {
    let initial = parent.join(name);
    if !initial.exists() {
        return initial;
    }
    for index in 2..10_000 {
        let candidate = parent.join(format!("{name} (Imported {index})"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{name} (Imported {})", std::process::id()))
}

fn remove_verified_archive_parts(source_folder: &Path, parts: &[PathBuf]) -> Result<(), Error> {
    let source_folder = fs::canonicalize(source_folder)?;
    let mut verified = Vec::new();
    for part in parts {
        let canonical = fs::canonicalize(part).with_context(|| {
            format!(
                "archive part disappeared before cleanup: {}",
                part.display()
            )
        })?;
        if canonical.parent() != Some(source_folder.as_path()) || !canonical.is_file() {
            anyhow::bail!(
                "refusing to remove an archive part outside the dropped folder: {}",
                canonical.display()
            );
        }
        verified.push(canonical);
    }
    for part in verified {
        fs::remove_file(&part).with_context(|| {
            format!(
                "the game is verified, but could not remove {}",
                part.display()
            )
        })?;
    }
    println!("Removed {} completed archive part(s).", parts.len());
    Ok(())
}

fn prepared_source_and_relative_destination(game: &PreparedGame) -> (&Path, PathBuf) {
    match &game.kind {
        PreparedGameKind::God {
            title_dir,
            title_id,
        } => (
            title_dir,
            PathBuf::from("Content")
                .join("0000000000000000")
                .join(title_id),
        ),
        PreparedGameKind::Jtag { title_id } => (
            &game.root,
            PathBuf::from("Content")
                .join("0000000000000000")
                .join(title_id),
        ),
    }
}

fn copy_prepared_game_to_selected_drive(
    game: &PreparedGame,
    drive: &RemovableDrive,
) -> Result<(), Error> {
    show_import_stage("Preparing USB destination", 76);
    let destination_root = &drive.root;
    println!("USB destination: {}", drive.label);
    let (source, relative) = prepared_source_and_relative_destination(game);
    let destination = destination_root.join(relative);
    if destination.exists() {
        anyhow::bail!(
            "the destination already contains this game: {}",
            destination.display()
        );
    }
    let files = collect_upload_files(source)?;
    let total = files.iter().map(|file| file.size).sum::<u64>();
    let available = fs2::available_space(destination_root)
        .context("could not check free space at the selected destination")?;
    if available < total {
        anyhow::bail!(
            "the selected destination needs {} but only {} is available",
            format_byte_size(total),
            format_byte_size(available)
        );
    }
    show_import_stage("Copying game to USB", 80);
    copy_directory_and_verify(source, &destination, "Copying game to USB")?;
    println!("Game copied successfully to {}.", destination.display());
    Ok(())
}

fn copy_prepared_game_to_folder(game: &PreparedGame, folder: &Path) -> Result<(), Error> {
    show_import_stage("Preparing destination folder", 76);
    let (source, relative) = prepared_source_and_relative_destination(game);
    let destination = folder.join(relative);
    fs::create_dir_all(destination.parent().unwrap_or(folder))?;
    copy_directory_and_verify(source, &destination, "Copying game")?;
    println!("Game copied successfully to {}.", destination.display());
    Ok(())
}

fn remember_prepared_output(prepared: &PreparedGame) {
    if let Ok(Some(mut job)) = pipeline::load_last_job() {
        job.prepared_root = Some(prepared.root.clone());
        job.pipeline.set(
            pipeline::StageKind::Extracting,
            pipeline::StageStatus::Complete,
        );
        job.pipeline.set(
            pipeline::StageKind::IsoDetected,
            pipeline::StageStatus::Complete,
        );
        job.pipeline.set(
            pipeline::StageKind::Converting,
            pipeline::StageStatus::Complete,
        );
        job.pipeline.set(
            pipeline::StageKind::Verification,
            pipeline::StageStatus::Complete,
        );
        let _ = job.save();
    }
}

fn copy_directory_and_verify(
    source: &Path,
    destination: &Path,
    progress_label: &str,
) -> Result<(), Error> {
    let files = collect_upload_files(source)?;
    let total = files.iter().map(|file| file.size).sum::<u64>();
    if files.is_empty() || total == 0 {
        anyhow::bail!("the prepared game contains no files to copy");
    }
    fs::create_dir_all(destination)?;
    let progress = ProgressDisplay::new();
    progress.update(progress_label, 0);
    let mut copied = 0_u64;
    for file in &files {
        let target = destination.join(Path::new(&file.relative_path));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut input = File::open(&file.local_path)?;
        let mut output = File::create(&target)?;
        let mut file_copied = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            file_copied += count as u64;
            let current = copied.saturating_add(file_copied);
            progress.update(
                progress_label,
                (current.saturating_mul(100) / total).min(100) as u8,
            );
        }
        output.flush()?;
        copied = copied.saturating_add(file.size);
    }
    progress.finish(&format!("{progress_label} complete"));
    show_import_stage(
        if progress_label.contains("USB") {
            "Verifying USB copy"
        } else {
            "Verifying local copy"
        },
        95,
    );
    for file in &files {
        let target = destination.join(Path::new(&file.relative_path));
        let size = fs::metadata(&target)
            .with_context(|| format!("copied file is missing: {}", target.display()))?
            .len();
        if size != file.size {
            anyhow::bail!(
                "copy verification failed for {}: expected {} bytes, found {size}",
                target.display(),
                file.size
            );
        }
    }
    Ok(())
}

fn upload_prepared_game_to_xbox(game: &PreparedGame) -> Result<(), Error> {
    if !saved_ftp_is_ready()? {
        render_terminal_header()?;
        println!(
            "Please configure and log in to your Xbox 360 FTP connection before continuing.\n"
        );
        if !configure_ftp_terminal()? {
            anyhow::bail!(
                "FTP setup was cancelled; the prepared game remains in the dropped folder"
            );
        }
    }
    let mut settings = load_ftp_settings()?.context("saved FTP settings are missing")?;
    settings.password = load_saved_password()?.context("saved FTP password is missing")?;
    show_import_stage("Connecting to Xbox 360 through FTP", 76);
    let ftp = store_worker::delivery_connection(&settings)?;
    let drives = detect_storage_drives(&ftp)?;
    let drive = choose_storage_drive(&drives)?;
    let (source, remote_base) = match &game.kind {
        PreparedGameKind::God {
            title_dir,
            title_id,
        } => (
            title_dir.as_path(),
            format!(
                "/{}/Content/0000000000000000/{}",
                drive.remote_name, title_id
            ),
        ),
        PreparedGameKind::Jtag { title_id } => (
            game.root.as_path(),
            format!(
                "/{}/Content/0000000000000000/{}",
                drive.remote_name, title_id
            ),
        ),
    };
    show_import_stage("Uploading to Xbox through FTP", 80);
    upload_directory_files(&ftp, source, &remote_base, "Xbox 360")
}

fn print_terminal_error(error: &Error) {
    eprintln!("\n\x1b[31;1mOperation failed\x1b[0m");
    eprintln!("{}", friendly_error_text(error));
}

fn friendly_error_text(error: &Error) -> String {
    let text = format!("{error:#}");
    let lower = text.to_ascii_lowercase();
    if lower.contains("insufficient disk") || lower.contains("not enough free") {
        "Insufficient disk space".to_owned()
    } else if lower.contains("wrong password") || lower.contains("archive password is incorrect") {
        "Archive password is incorrect".to_owned()
    } else if lower.contains("no valid xbox 360 iso") {
        "No valid Xbox 360 ISO was found".to_owned()
    } else if lower.contains("store could not be reached") {
        "Store could not be reached".to_owned()
    } else if lower.contains("search returned no results") {
        "Search returned no results".to_owned()
    } else if lower.contains("download link is unavailable") {
        "Download link is unavailable".to_owned()
    } else if lower.contains("browser download was not detected") {
        "Browser download was not detected".to_owned()
    } else if lower.contains("archive is corrupted")
        || lower.contains("headers error")
        || lower.contains("data error")
    {
        format!("Archive is corrupted\n{text}")
    } else if lower.contains("god conversion failed") || lower.contains("error writing part file") {
        format!("GOD conversion failed\n{text}")
    } else if lower.contains("package verification failed")
        || lower.contains("converted god package is incomplete")
    {
        format!("Package verification failed\n{text}")
    } else if lower.contains("connection refused") {
        "Xbox FTP connection was refused".to_owned()
    } else if lower.contains("destination directory is unavailable") {
        "Xbox destination directory is unavailable".to_owned()
    } else if lower.contains("upload was interrupted") {
        "Upload was interrupted".to_owned()
    } else {
        text
    }
}

fn pause_to_continue() -> Result<(), Error> {
    if env::var_os("ISO2GOD_QUEUE_JOB").is_some() {
        return Ok(());
    }
    print!("\nPress Enter to return to the main menu...");
    io::stdout().flush().context("error writing prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("error reading input")?;
    Ok(())
}

struct LineInputGuard;

impl LineInputGuard {
    fn enter() -> Result<Self, Error> {
        enable_raw_mode().context("could not enable terminal input")?;
        Ok(Self)
    }
}

impl Drop for LineInputGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn prompt_terminal_text(
    prompt: &str,
    default: Option<&str>,
    hidden: bool,
) -> Result<Option<String>, Error> {
    match default {
        Some(default) => print!("{prompt} [{default}]: "),
        None if hidden => print!("{prompt} (hidden): "),
        None => print!("{prompt}: "),
    }
    io::stdout().flush().context("error writing prompt")?;
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        println!();
        return Ok(None);
    }

    let _line_mode = LineInputGuard::enter()?;
    let mut value = String::new();
    loop {
        let Event::Key(key) = read().context("could not read terminal input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                println!();
                return Ok(None);
            }
            KeyCode::Char(character) => {
                value.push(character);
                print!("{}", if hidden { '*' } else { character });
                io::stdout().flush().context("error updating input")?;
            }
            KeyCode::Backspace => {
                if value.pop().is_some() {
                    print!("\x08 \x08");
                    io::stdout().flush().context("error updating input")?;
                }
            }
            KeyCode::Enter => {
                if value.is_empty() {
                    if let Some(default) = default {
                        println!();
                        return Ok(Some(default.to_owned()));
                    }
                    print!("\x07");
                    io::stdout().flush().context("error updating input")?;
                    continue;
                }
                println!();
                return Ok(Some(value));
            }
            KeyCode::Esc => {
                println!("Cancelled.");
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn prompt_optional_terminal_text(
    prompt: &str,
    default: Option<&str>,
    hidden: bool,
) -> Result<Option<String>, Error> {
    if hidden {
        print!("{prompt}: ");
    } else if let Some(default) = default {
        print!("{prompt} [{default}]: ");
    } else {
        print!("{prompt}: ");
    }
    io::stdout().flush().context("error writing prompt")?;
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        println!();
        return Ok(None);
    }

    let _line_mode = LineInputGuard::enter()?;
    let mut value = String::new();
    loop {
        let Event::Key(key) = read().context("could not read terminal input")? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                println!();
                return Ok(None);
            }
            KeyCode::Char(character) => {
                value.push(character);
                print!("{}", if hidden { '*' } else { character });
                io::stdout().flush().context("error updating input")?;
            }
            KeyCode::Backspace => {
                if value.pop().is_some() {
                    print!("\x08 \x08");
                    io::stdout().flush().context("error updating input")?;
                }
            }
            KeyCode::Enter => {
                println!();
                return Ok(Some(
                    if value.is_empty() {
                        default.unwrap_or_default()
                    } else {
                        &value
                    }
                    .to_owned(),
                ));
            }
            KeyCode::Esc => {
                println!("Cancelled.");
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn prompt_dropped_game_file() -> Result<Option<PathBuf>, Error> {
    prompt_dropped_path("Waiting for game file or archive folder", None)
}

fn prompt_dropped_archive_input() -> Result<Option<PathBuf>, Error> {
    prompt_dropped_path("Waiting for archive or folder", None)
}

fn prompt_dropped_path(
    prompt: &str,
    require_directory: Option<bool>,
) -> Result<Option<PathBuf>, Error> {
    print!("{prompt}: ");
    io::stdout().flush().context("error writing prompt")?;
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        println!();
        return Ok(None);
    }

    let _line_mode = LineInputGuard::enter()?;
    let mut value = String::new();
    loop {
        match read().context("could not read terminal input")? {
            Event::Paste(text) => {
                value.push_str(&text);
                print!("{text}");
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        println!();
                        return Ok(None);
                    }
                    KeyCode::Char(character) => {
                        value.push(character);
                        print!("{character}");
                    }
                    KeyCode::Backspace => {
                        if value.pop().is_some() {
                            print!("\x08 \x08");
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(path) = complete_dropped_path(&value, require_directory) {
                            println!();
                            return Ok(Some(path));
                        }
                        print!("\nPath not found or not valid for this prompt. Try again: ");
                        value.clear();
                    }
                    KeyCode::Esc => {
                        println!("Cancelled.");
                        return Ok(None);
                    }
                    _ => {}
                }
            }
            _ => continue,
        }
        io::stdout().flush().context("error updating input")?;
    }
}

fn prompt_optional_dropped_path(
    prompt: &str,
    require_directory: Option<bool>,
) -> Result<Option<PathBuf>, Error> {
    if !prompt.is_empty() {
        print!("{prompt}: ");
        io::stdout().flush().context("error writing prompt")?;
    }
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        println!();
        return Ok(None);
    }

    let _line_mode = LineInputGuard::enter()?;
    let mut value = String::new();
    loop {
        match read().context("could not read terminal input")? {
            Event::Paste(text) => {
                value.push_str(&text);
                print!("{text}");
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        println!();
                        return Ok(None);
                    }
                    KeyCode::Char(character) => {
                        value.push(character);
                        print!("{character}");
                    }
                    KeyCode::Backspace => {
                        if value.pop().is_some() {
                            print!("\x08 \x08");
                        }
                    }
                    KeyCode::Enter => {
                        if value.trim().is_empty() {
                            println!();
                            return Ok(None);
                        }
                        if let Some(path) = complete_dropped_path(&value, require_directory) {
                            println!();
                            return Ok(Some(path));
                        }
                        print!("\nPath not found or not valid for this prompt. Try again: ");
                        value.clear();
                    }
                    KeyCode::Esc => {
                        println!("Skipped.");
                        return Ok(None);
                    }
                    _ => {}
                }
            }
            _ => continue,
        }
        io::stdout().flush().context("error updating input")?;
    }
}

fn complete_dropped_path(value: &str, require_directory: Option<bool>) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let value = if let Some(value) = value.strip_prefix('"') {
        value.strip_suffix('"')?
    } else {
        value
    };
    let path = PathBuf::from(value);
    match require_directory {
        Some(true) => path.is_dir().then_some(path),
        Some(false) => path.is_file().then_some(path),
        None => (path.is_file() || path.is_dir()).then_some(path),
    }
}

fn prompt_yes_no_key(prompt: &str, default: bool) -> Result<Option<bool>, Error> {
    print!("{prompt} [Y/N] ");
    io::stdout().flush().context("error writing prompt")?;
    if env::var_os("ISO2GOD_TERMINAL_SMOKE_TEST").as_deref() == Some(OsStr::new("1")) {
        println!("{}", if default { 'Y' } else { 'N' });
        return Ok(Some(default));
    }

    let _raw_mode = RawModeGuard::enter()?;
    loop {
        let Event::Key(key) = read().context("could not read terminal input")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                println!("Yes");
                return Ok(Some(true));
            }
            KeyCode::Char('n' | 'N') => {
                println!("No");
                return Ok(Some(false));
            }
            KeyCode::Enter => {
                println!("{}", if default { "Yes" } else { "No" });
                return Ok(Some(default));
            }
            KeyCode::Esc => {
                println!("Cancelled.");
                return Ok(None);
            }
            _ => {}
        }
    }
}

fn pause_before_exit() {
    print!("\nPress Enter to close...");
    let _ = io::stdout().flush();
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
}

fn convert_and_maybe_upload(args: Cli) -> Result<(), Error> {
    let upload_ftp = args.upload_ftp;
    let copy_usb = args.copy_usb;
    let use_saved_ftp = args.use_saved_ftp;
    let prompt_delivery = args.prompt_delivery;
    let selected_usb_drive = args.selected_usb_drive.clone();
    if (upload_ftp || copy_usb || use_saved_ftp) && args.dry_run {
        anyhow::bail!("transfer options cannot be combined with --dry-run");
    }
    report_gui_stage("Checking input", 0);
    let mut output = run(args)?;
    report_gui_stage("Verifying converted game", 0);
    verify_converted_game(&output)?;
    report_gui_progress("Verifying converted game", 100);
    let delivery = if prompt_delivery {
        choose_delivery_method()?
    } else if use_saved_ftp {
        DeliveryMethod::SavedFtp
    } else if upload_ftp {
        DeliveryMethod::Ftp
    } else if copy_usb {
        DeliveryMethod::Usb
    } else {
        DeliveryMethod::LocalOnly
    };
    match delivery {
        DeliveryMethod::Ftp => upload_to_xbox(&output)?,
        DeliveryMethod::SavedFtp => upload_to_saved_xbox(&output)?,
        DeliveryMethod::Usb => copy_to_removable_drive(&output, selected_usb_drive)?,
        DeliveryMethod::LocalOnly => {
            println!("Converted files were kept on this PC. No transfer was requested.");
        }
    }
    output.cleanup_extracted_files();
    report_gui_stage("Complete", 100);
    Ok(())
}

struct ConversionOutput {
    local_title_dir: PathBuf,
    title_id: String,
    extracted_temp: Option<TempDir>,
}

impl ConversionOutput {
    fn cleanup_extracted_files(&mut self) {
        if let Some(mut temp_dir) = self.extracted_temp.take() {
            temp_dir.cleanup = true;
            drop(temp_dir);
        }
    }
}

enum DeliveryMethod {
    Ftp,
    SavedFtp,
    Usb,
    LocalOnly,
}

fn choose_delivery_method() -> Result<DeliveryMethod, Error> {
    println!("\nChoose delivery method:\n");
    let options = [
        MenuOption {
            value: 1_u8,
            label: "1. Upload to Xbox using FTP".to_owned(),
        },
        MenuOption {
            value: 2_u8,
            label: "2. Save to USB drive".to_owned(),
        },
        MenuOption {
            value: 3_u8,
            label: "3. Save to a folder on this PC".to_owned(),
        },
    ];
    Ok(match select_menu(&options)?.unwrap_or(3) {
        1 => DeliveryMethod::Ftp,
        2 => DeliveryMethod::Usb,
        _ => DeliveryMethod::LocalOnly,
    })
}

fn run(args: Cli) -> Result<ConversionOutput, Error> {
    if args.num_threads == 1 {
        eprintln!("Using one worker thread for drive compatibility. Override with -j <N>.");
    }

    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(args.num_threads)
        .build_global();

    let prepared_source = prepare_source(&args.source_iso)?;
    let source_path = prepared_source.path();

    println!("Reading ISO metadata...");

    let source_iso_file = File::open(source_path).context("error opening source ISO file")?;

    let source_iso_file_meta =
        fs::metadata(source_path).context("error reading source ISO file metadata")?;

    let mut source_iso =
        iso::IsoReader::read(source_iso_file).context("error reading source ISO")?;

    let title_info =
        TitleInfo::from_image(&mut source_iso).context("error reading image executable")?;

    let exe_info = title_info.execution_info;
    let content_type = title_info.content_type;

    {
        let title_id = format!("{:08X}", exe_info.title_id);
        let name = game_list::find_title_by_id(exe_info.title_id).unwrap_or("(unknown)".to_owned());

        println!("Title ID: {title_id}");
        println!("    Name: {name}");
        match content_type {
            ContentType::GamesOnDemand => println!("    Type: Games on Demand"),
            ContentType::XboxOriginal => println!("    Type: Xbox Original"),
        }
    }

    if args.dry_run {
        return Ok(ConversionOutput {
            local_title_dir: args.dest_dir.join(format!("{:08X}", exe_info.title_id)),
            title_id: format!("{:08X}", exe_info.title_id),
            extracted_temp: prepared_source.into_temp_dir(),
        });
    }

    let data_size = if args.trim.unwrap_or_default() == TrimMode::FromEnd {
        source_iso.get_max_used_prefix_size()
    } else {
        let root_offset = source_iso.volume_descriptor.root_offset;
        source_iso_file_meta.len() - root_offset
    };

    let block_count = data_size.div_ceil(god::BLOCK_SIZE);
    let part_count = block_count.div_ceil(god::BLOCKS_PER_PART);
    if part_count == 0 {
        anyhow::bail!("the ISO contains no convertible game data");
    }

    let file_layout = god::FileLayout::new(&args.dest_dir, &exe_info, content_type);

    report_gui_stage("Converting to GOD", 0);
    println!("\nPlease wait while the game is converted.");
    let conversion_progress = ProgressDisplay::new();
    conversion_progress.update("Preparing output", 2);

    ensure_empty_dir(&file_layout.data_dir_path()).context("error clearing data directory")?;

    conversion_progress.update("Writing game data", 5);

    let progress = AtomicUsize::new(0);

    (0..part_count).into_par_iter().try_for_each(|part_index| {
        let mut iso_data_volume = File::open(source_path)?;
        iso_data_volume.seek(SeekFrom::Start(source_iso.volume_descriptor.root_offset))?;

        let part_file = file_layout.part_file_path(part_index);

        let part_file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&part_file)
            .context("error creating part file")?;

        god::write_part(iso_data_volume, part_index, part_file)
            .context("error writing part file")?;

        let cur = 1 + progress.fetch_add(1, Ordering::Relaxed);
        let percent = 5 + (cur as u64 * 80 / part_count.max(1)) as u8;
        conversion_progress.update("Writing game data", percent);

        Ok::<_, anyhow::Error>(())
    })?;

    conversion_progress.update("Calculating hash chain", 86);

    let mut mht =
        read_part_mht(&file_layout, part_count - 1).context("error reading part file MHT")?;

    for prev_part_index in (0..part_count - 1).rev() {
        let mut prev_mht =
            read_part_mht(&file_layout, prev_part_index).context("error reading part file MHT")?;

        prev_mht.add_hash(&mht.digest());

        write_part_mht(&file_layout, prev_part_index, &prev_mht)
            .context("error writing part file MHT")?;

        mht = prev_mht;

        let completed = part_count - 1 - prev_part_index;
        let percent = 86 + (completed * 11 / (part_count - 1).max(1)) as u8;
        conversion_progress.update("Calculating hash chain", percent);
    }

    let last_part_size = fs::metadata(file_layout.part_file_path(part_count - 1))
        .map(|m| m.len())
        .context("error reading part file")?;

    conversion_progress.update("Finalising package", 98);

    let mut con_header = god::ConHeaderBuilder::new()
        .with_execution_info(&exe_info)
        .with_block_counts(block_count as u32, 0)
        .with_data_parts_info(
            part_count as u32,
            last_part_size + (part_count - 1) * god::BLOCK_SIZE * 0xa290,
        )
        .with_content_type(content_type)
        .with_mht_hash(&mht.digest());

    let game_title = args
        .game_title
        .or(game_list::find_title_by_id(exe_info.title_id));
    if let Some(game_title) = game_title {
        con_header = con_header.with_game_title(&game_title);
    }

    let con_header = con_header.finalize();

    let mut con_header_file = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(file_layout.con_header_file_path())
        .context("cannot open con header file")?;

    con_header_file
        .write_all(&con_header)
        .context("error writing con header file")?;

    conversion_progress.finish("Conversion complete");
    println!("Converted files: {}", args.dest_dir.display());

    Ok(ConversionOutput {
        local_title_dir: args.dest_dir.join(format!("{:08X}", exe_info.title_id)),
        title_id: format!("{:08X}", exe_info.title_id),
        extracted_temp: prepared_source.into_temp_dir(),
    })
}

struct PreparedSource {
    path: PathBuf,
    temp_dir: Option<TempDir>,
}

impl PreparedSource {
    fn direct(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            temp_dir: None,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn into_temp_dir(mut self) -> Option<TempDir> {
        self.temp_dir.take()
    }
}

struct TempDir {
    path: PathBuf,
    cleanup: bool,
}

impl TempDir {
    fn new() -> Result<Self, Error> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();

        for attempt in 0..100 {
            let path = env::temp_dir().join(format!(
                "iso2god-{}-{timestamp}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        cleanup: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("error creating temporary directory"),
            }
        }

        anyhow::bail!("could not create a unique temporary directory")
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.cleanup {
            eprintln!(
                "Temporary recovery files were preserved at: {}",
                self.path.display()
            );
            return;
        }
        println!("\nRemoving temporary extracted files...");
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "Warning: could not remove temporary folder {}: {error}",
                self.path.display()
            );
        }
    }
}

#[derive(Clone, Copy)]
enum ArchiveKind {
    SevenZip,
    WinRar,
}

struct ArchiveTool {
    kind: ArchiveKind,
    path: PathBuf,
}

fn prepare_source(source: &Path) -> Result<PreparedSource, Error> {
    report_gui_stage("Checking input", 10);
    if !source.is_file() {
        anyhow::bail!("input file does not exist: {}", source.display());
    }

    let extension = source
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();

    if extension == "iso" {
        return Ok(PreparedSource::direct(source));
    }

    let archive = detect_archive_set(source)?;
    let tool = if archive.multipart {
        find_7zip().map(|path| ArchiveTool {
            kind: ArchiveKind::SevenZip,
            path,
        })
    } else {
        find_archive_tool()
    }
    .ok_or_else(|| {
        anyhow::anyhow!(
            "no compatible archive extractor was found; install 7-Zip from {SEVEN_ZIP_DOWNLOAD_URL} and try again"
        )
    })?;
    if archive.multipart {
        println!(
            "Found {} related archive parts; integrity is checked during extraction.",
            archive.part_count
        );
    }
    let temp_dir = TempDir::new()?;

    let tool_name = match tool.kind {
        ArchiveKind::SevenZip => "7-Zip",
        ArchiveKind::WinRar => "WinRAR",
    };
    report_gui_stage("Extracting archives", 0);
    println!("\nPlease wait while the archive is extracted with {tool_name}.");
    let extraction_progress = ProgressDisplay::new();
    extraction_progress.update("Extracting archive", 0);

    let status = match tool.kind {
        ArchiveKind::SevenZip => extract_with_7zip(
            &tool.path,
            &archive.entry_path,
            temp_dir.path(),
            &extraction_progress,
        ),
        ArchiveKind::WinRar => Command::new(&tool.path)
            .arg("x")
            .arg("-ibck")
            .arg("-idq")
            .arg("-y")
            .arg("-o+")
            .arg(&archive.entry_path)
            .arg(temp_dir.path())
            .status()
            .with_context(|| format!("error starting {tool_name}")),
    }?;

    if !status.success() {
        anyhow::bail!("{tool_name} could not extract the archive (exit status {status})");
    }
    extraction_progress.finish("Extraction complete");

    report_gui_stage("Finding ISO", 0);
    let iso_path = find_largest_iso(temp_dir.path())?.ok_or_else(|| {
        anyhow::anyhow!("the archive was extracted, but it did not contain an ISO file")
    })?;
    report_gui_progress("Finding ISO", 100);
    println!("Using extracted ISO: {}", iso_path.display());

    Ok(PreparedSource {
        path: iso_path,
        temp_dir: Some(temp_dir),
    })
}

#[derive(Debug)]
struct ArchiveSet {
    entry_path: PathBuf,
    multipart: bool,
    part_count: usize,
}

fn detect_archive_set(source: &Path) -> Result<ArchiveSet, Error> {
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let file_name = source
        .file_name()
        .and_then(OsStr::to_str)
        .context("the input archive name is not valid Unicode")?;
    let lower = file_name.to_ascii_lowercase();
    let siblings = fs::read_dir(parent)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| (name.to_ascii_lowercase(), entry.path()))
        })
        .collect::<Vec<_>>();

    if let Some(split) = lower.rfind('.') {
        let digits = &lower[split + 1..];
        let prefix = &lower[..split + 1];
        if digits.len() >= 2
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && (prefix.ends_with(".7z.") || prefix.ends_with(".zip.") || digits.len() == 3)
        {
            let parts = collect_numbered_parts(&siblings, prefix, "", 1)?;
            let entry = parts
                .iter()
                .find(|(part, _)| *part == 1)
                .map(|(_, path)| path.clone())
                .context("multi-part archive is missing part 001")?;
            return Ok(ArchiveSet {
                entry_path: entry,
                multipart: true,
                part_count: parts.len(),
            });
        }
    }

    if lower.ends_with(".rar") {
        let stem = &lower[..lower.len() - 4];
        let part_info = if let Some(part_index) = stem.rfind(".part") {
            Some((part_index + 5, &lower[..part_index + 5]))
        } else if let Some(part_index) = stem.rfind("_part") {
            Some((part_index + 5, &lower[..part_index + 5]))
        } else if let Some(part_index) = stem.rfind("-part") {
            Some((part_index + 5, &lower[..part_index + 5]))
        } else if stem.starts_with("part") {
            Some((4, &lower[..4]))
        } else {
            None
        };
        if let Some((prefix_len, prefix)) = part_info {
            let digits = &stem[prefix_len..];
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                let parts = collect_numbered_parts(&siblings, prefix, ".rar", 1)?;
                let entry = parts
                    .iter()
                    .find(|(part, _)| *part == 1)
                    .map(|(_, path)| path.clone())
                    .context("multi-part RAR archive is missing part 1")?;
                return Ok(ArchiveSet {
                    entry_path: entry,
                    multipart: true,
                    part_count: parts.len(),
                });
            }
        }
    }

    if let Some(base) = lower.strip_suffix(".rar") {
        let parts = collect_numbered_parts_if_present(&siblings, &format!("{base}.r"), "", 0)?;
        if !parts.is_empty() {
            return Ok(ArchiveSet {
                entry_path: source.to_path_buf(),
                multipart: true,
                part_count: parts.len() + 1,
            });
        }
    }

    if lower.len() >= 4
        && lower[lower.len() - 4..].starts_with(".r")
        && lower[lower.len() - 2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        let base = &lower[..lower.len() - 4];
        let main = siblings
            .iter()
            .find(|(name, _)| name == &format!("{base}.rar"))
            .map(|(_, path)| path.clone())
            .context("multi-part RAR archive is missing its .rar file")?;
        let parts = collect_numbered_parts(&siblings, &format!("{base}.r"), "", 0)?;
        return Ok(ArchiveSet {
            entry_path: main,
            multipart: true,
            part_count: parts.len() + 1,
        });
    }

    let zip_base = lower.strip_suffix(".zip").map(str::to_owned).or_else(|| {
        (lower.len() >= 4
            && lower[lower.len() - 4..].starts_with(".z")
            && lower[lower.len() - 2..]
                .bytes()
                .all(|byte| byte.is_ascii_digit()))
        .then(|| lower[..lower.len() - 4].to_owned())
    });
    if let Some(base) = zip_base {
        let parts = collect_numbered_parts_if_present(&siblings, &format!("{base}.z"), "", 1)?;
        if !parts.is_empty() {
            let main = siblings
                .iter()
                .find(|(name, _)| name == &format!("{base}.zip"))
                .map(|(_, path)| path.clone())
                .context("split ZIP archive is missing its .zip file")?;
            return Ok(ArchiveSet {
                entry_path: main,
                multipart: true,
                part_count: parts.len() + 1,
            });
        }
    }

    if matches!(
        lower.rsplit_once('.').map(|(_, extension)| extension),
        Some("zip" | "7z" | "rar")
    ) {
        return Ok(ArchiveSet {
            entry_path: source.to_path_buf(),
            multipart: false,
            part_count: 1,
        });
    }
    anyhow::bail!(
        "unsupported input type; use an ISO, ZIP, 7Z, RAR, or supported multi-part archive"
    )
}

fn collect_numbered_parts(
    siblings: &[(String, PathBuf)],
    prefix: &str,
    suffix: &str,
    expected_start: u32,
) -> Result<Vec<(u32, PathBuf)>, Error> {
    let parts = collect_numbered_parts_if_present(siblings, prefix, suffix, expected_start)?;
    if parts.is_empty() {
        anyhow::bail!("no related multi-part archive files were found");
    }
    Ok(parts)
}

fn collect_numbered_parts_if_present(
    siblings: &[(String, PathBuf)],
    prefix: &str,
    suffix: &str,
    expected_start: u32,
) -> Result<Vec<(u32, PathBuf)>, Error> {
    let mut parts = siblings
        .iter()
        .filter_map(|(name, path)| {
            let middle = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
            (!middle.is_empty() && middle.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| middle.parse::<u32>().ok().map(|part| (part, path.clone())))
                .flatten()
        })
        .collect::<Vec<_>>();
    parts.sort_by_key(|(part, _)| *part);
    if let Some((first, _)) = parts.first()
        && *first != expected_start
    {
        anyhow::bail!("multi-part archive is missing part {expected_start}");
    }
    for pair in parts.windows(2) {
        if pair[1].0 != pair[0].0 + 1 {
            anyhow::bail!("multi-part archive is missing part {}", pair[0].0 + 1);
        }
    }
    Ok(parts)
}

fn extract_with_7zip(
    executable: &Path,
    source: &Path,
    destination: &Path,
    progress: &ProgressDisplay,
) -> Result<std::process::ExitStatus, Error> {
    let password = current_archive_password();
    let mut command = Command::new(executable);
    command
        .arg("x")
        .arg("-y")
        .arg("-bso0")
        .arg("-bsp1")
        .arg("-bse1")
        .arg("-sccUTF-8")
        .arg(format!("-o{}", destination.display()))
        .stderr(Stdio::piped());
    if let Some(password) = password.as_deref() {
        command.arg(format!("-p{password}"));
    }
    let mut child = command
        .arg("--")
        .arg(source)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("error starting 7-Zip")?;

    let mut output = child
        .stdout
        .take()
        .context("could not read 7-Zip progress")?;
    let mut error_output = child
        .stderr
        .take()
        .context("could not capture 7-Zip diagnostics")?;
    let error_reader = std::thread::spawn(move || {
        let mut recent = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let count = error_output.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            recent.extend_from_slice(&buffer[..count]);
            if recent.len() > 32_768 {
                recent.drain(..recent.len() - 32_768);
            }
        }
        Ok::<Vec<u8>, io::Error>(recent)
    });
    let mut buffer = [0_u8; 1024];
    let mut recent = Vec::new();
    loop {
        let count = output.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        recent.extend_from_slice(&buffer[..count]);
        if recent.len() > 4096 {
            recent.drain(..recent.len() - 4096);
        }
        if let Some(percent) = last_percentage(&recent) {
            progress.update("Extracting archive", percent);
        }
    }

    let status = child.wait().context("error waiting for 7-Zip")?;
    let errors = error_reader
        .join()
        .map_err(|_| anyhow::anyhow!("7-Zip diagnostic reader stopped unexpectedly"))?
        .context("could not read 7-Zip diagnostics")?;
    if !status.success() {
        let details = archive_diagnostics(&recent, &errors);
        if details.to_ascii_lowercase().contains("wrong password") {
            anyhow::bail!("Archive password is incorrect");
        }
        anyhow::bail!(
            "7-Zip extraction failed for {} ({status}); originals preserved.\n{details}",
            source.display()
        );
    }
    Ok(status)
}

thread_local! {
    static ARCHIVE_PASSWORD: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn set_archive_password(password: Option<&str>) {
    ARCHIVE_PASSWORD.with(|slot| *slot.borrow_mut() = password.map(str::to_owned));
}

fn current_archive_password() -> Option<String> {
    ARCHIVE_PASSWORD.with(|slot| slot.borrow().clone())
}

fn test_archive_password(seven_zip: &Path, archive: &Path, password: &str) -> Result<bool, Error> {
    let output = Command::new(seven_zip)
        .args(["t", "-bso0", "-bse1", "-y"])
        .arg(format!("-p{password}"))
        .arg("--")
        .arg(archive)
        .stdin(Stdio::null())
        .output()
        .context("could not test the archive password")?;
    let details = archive_diagnostics(&output.stdout, &output.stderr).to_ascii_lowercase();
    if details.contains("wrong password") {
        return Ok(false);
    }
    Ok(output.status.success())
}

fn last_percentage(output: &[u8]) -> Option<u8> {
    for percent_index in (0..output.len())
        .rev()
        .filter(|index| output[*index] == b'%')
    {
        let mut start = percent_index;
        while start > 0 && output[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start < percent_index
            && let Ok(value) = std::str::from_utf8(&output[start..percent_index])
            && let Ok(value) = value.parse::<u8>()
            && value <= 100
        {
            return Some(value);
        }
    }
    None
}

fn find_largest_iso(root: &Path) -> Result<Option<PathBuf>, Error> {
    let mut directories = vec![root.to_path_buf()];
    let mut largest: Option<(u64, PathBuf)> = None;

    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("error reading extracted folder {}", directory.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .path()
                    .extension()
                    .and_then(OsStr::to_str)
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("iso"))
            {
                let size = entry.metadata()?.len();
                if largest
                    .as_ref()
                    .is_none_or(|(largest_size, _)| size > *largest_size)
                {
                    largest = Some((size, entry.path()));
                }
            }
        }
    }

    Ok(largest.map(|(_, path)| path))
}

fn find_archive_tool() -> Option<ArchiveTool> {
    find_7zip()
        .map(|path| ArchiveTool {
            kind: ArchiveKind::SevenZip,
            path,
        })
        .or_else(|| {
            find_executable_on_path(&["WinRAR.exe", "winrar"])
                .or_else(|| program_files_executable("WinRAR", "WinRAR.exe"))
                .map(|path| ArchiveTool {
                    kind: ArchiveKind::WinRar,
                    path,
                })
        })
}

fn find_7zip() -> Option<PathBuf> {
    find_executable_on_path(&["7z.exe", "7zz.exe", "7z", "7zz"])
        .or_else(|| program_files_executable("7-Zip", "7z.exe"))
}

fn find_executable_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .find(|candidate| candidate.is_file())
}

fn program_files_executable(program: &str, executable: &str) -> Option<PathBuf> {
    ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .map(|directory| directory.join(program).join(executable))
        .find(|candidate| candidate.is_file())
}

struct ProgressDisplay {
    state: Mutex<ProgressState>,
    total_bytes: Option<u64>,
}

struct ProgressState {
    baseline_percent: Option<u8>,
    baseline_time: Instant,
}

impl ProgressDisplay {
    fn new() -> Self {
        Self::new_with_total(None)
    }

    fn with_total(total_bytes: u64) -> Self {
        Self::new_with_total((total_bytes > 0).then_some(total_bytes))
    }

    fn new_with_total(total_bytes: Option<u64>) -> Self {
        Self {
            state: Mutex::new(ProgressState {
                baseline_percent: None,
                baseline_time: Instant::now(),
            }),
            total_bytes,
        }
    }

    fn update(&self, message: &str, percent: u8) {
        self.render(message, percent, None, true);
    }

    fn update_with_detail(&self, message: &str, percent: u8, detail: Option<&str>) {
        self.render(message, percent, detail, false);
    }

    fn render(&self, message: &str, percent: u8, detail: Option<&str>, add_eta: bool) {
        if gui_backend_enabled() {
            report_gui_progress(message, percent);
            return;
        }
        let percent = percent.min(100);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        let baseline = match state.baseline_percent {
            Some(baseline) if percent >= baseline => baseline,
            _ => {
                state.baseline_percent = Some(percent);
                state.baseline_time = now;
                percent
            }
        };
        let eta_seconds = estimate_progress_remaining(
            state.baseline_time.elapsed().as_secs_f64(),
            baseline,
            percent,
        );
        let automatic_detail = add_eta.then(|| {
            let remaining = eta_seconds
                .map(format_remaining_time)
                .unwrap_or_else(|| "calculating time remaining".to_owned());
            if let Some(total) = self.total_bytes {
                let completed = total.saturating_mul(u64::from(percent)) / 100;
                let baseline_bytes = total.saturating_mul(u64::from(baseline)) / 100;
                let speed = completed.saturating_sub(baseline_bytes) as f64
                    / state.baseline_time.elapsed().as_secs_f64().max(0.001);
                format!(
                    "{} / {}  •  {}  •  {remaining}",
                    format_byte_size(completed),
                    format_byte_size(total),
                    if speed > 1.0 {
                        format_transfer_speed(speed)
                    } else {
                        "calculating speed".to_owned()
                    }
                )
            } else {
                remaining
            }
        });
        let detail = detail.or(automatic_detail.as_deref()).unwrap_or("");
        drop(state);
        render_progress_screen("Preparing game", message, percent, Some(detail));
    }

    fn finish(&self, message: &str) {
        self.render(message, 100, None, false);
    }
}

fn smooth_progress_bar(percent: u8, width: usize) -> String {
    let filled = (usize::from(percent.min(100)) * width / 100).min(width);
    format!(
        "\x1b[38;5;208m{}\x1b[90m{}\x1b[0m",
        "█".repeat(filled),
        "█".repeat(width.saturating_sub(filled))
    )
}

fn estimate_progress_remaining(
    elapsed_seconds: f64,
    baseline_percent: u8,
    current_percent: u8,
) -> Option<f64> {
    let completed = current_percent.checked_sub(baseline_percent)?;
    if elapsed_seconds < 1.0 || completed == 0 || current_percent >= 100 {
        return None;
    }
    Some(elapsed_seconds * f64::from(100 - current_percent) / f64::from(completed))
}

fn gui_backend_enabled() -> bool {
    env::var_os("ISO2GOD_GUI_BACKEND").as_deref() == Some(OsStr::new("1"))
}

fn report_gui_stage(stage: &str, percent: u8) {
    if gui_backend_enabled() {
        println!("GUI_STAGE|{}|{}", percent.min(100), stage);
    }
}

fn report_gui_progress(stage: &str, percent: u8) {
    if gui_backend_enabled() {
        println!("GUI_PROGRESS|{}|{}", percent.min(100), stage);
    }
}

fn verify_converted_game(output: &ConversionOutput) -> Result<(), Error> {
    if !output.local_title_dir.is_dir() {
        anyhow::bail!(
            "the converted Title ID folder is missing: {}",
            output.local_title_dir.display()
        );
    }
    let files = collect_upload_files(&output.local_title_dir)?;
    if files.iter().any(|file| file.size == 0) {
        anyhow::bail!("the converted package contains an empty file");
    }
    let has_header = files.iter().any(|file| {
        !file.relative_path.contains(".data/") && !file.relative_path.contains(".data\\")
    });
    let data_parts = files
        .iter()
        .filter(|file| {
            file.relative_path.contains(".data/") || file.relative_path.contains(".data\\")
        })
        .count();
    if !has_header || data_parts == 0 {
        anyhow::bail!("the converted GOD package is incomplete or has an invalid layout");
    }
    Ok(())
}

#[derive(Clone)]
struct RemovableDrive {
    root: PathBuf,
    label: String,
    free_bytes: u64,
}

fn select_removable_drive() -> Result<Option<RemovableDrive>, Error> {
    println!("\nRemovable USB transfer");
    println!("----------------------");
    loop {
        let drives = detect_removable_drives()?;
        if drives.is_empty() {
            println!("No removable USB drive is currently detected.");
            println!("Connect a USB drive, then choose Refresh.\n");
            let choices = [
                MenuOption {
                    value: true,
                    label: "Refresh drive list".to_owned(),
                },
                MenuOption {
                    value: false,
                    label: "Return to main menu".to_owned(),
                },
            ];
            if select_menu(&choices)?.unwrap_or(false) {
                continue;
            }
            return Ok(None);
        }

        println!("Removable drives detected:\n");
        let choices = drives
            .iter()
            .enumerate()
            .map(|(index, drive)| MenuOption {
                value: index,
                label: format!(
                    "{}. {} — {} free",
                    index + 1,
                    drive.label,
                    format_byte_size(drive.free_bytes)
                ),
            })
            .collect::<Vec<_>>();
        let Some(selected) = select_menu(&choices)? else {
            return Ok(None);
        };
        return Ok(drives.into_iter().nth(selected));
    }
}

fn copy_to_removable_drive(
    output: &ConversionOutput,
    selected_drive: Option<RemovableDrive>,
) -> Result<(), Error> {
    let drive = match selected_drive {
        Some(drive) => drive,
        None => select_removable_drive()?.context("USB transfer was cancelled")?,
    };

    let files = collect_upload_files(&output.local_title_dir)?;
    let total_bytes = files.iter().map(|file| file.size).sum::<u64>();
    if files.is_empty() || total_bytes == 0 {
        anyhow::bail!(
            "no converted files were found in {}",
            output.local_title_dir.display()
        );
    }
    if drive.free_bytes < total_bytes {
        anyhow::bail!(
            "{} does not have enough free space ({} required, {} available)",
            drive.label,
            format_byte_size(total_bytes),
            format_byte_size(drive.free_bytes)
        );
    }

    let destination = drive
        .root
        .join("Content")
        .join("0000000000000000")
        .join(&output.title_id);
    fs::create_dir_all(&destination).with_context(|| {
        format!(
            "could not create the Xbox content folder on {}",
            drive.root.display()
        )
    })?;

    println!("\nCopying to {}", destination.display());
    println!("Please wait while the converted game is copied to USB.");
    report_gui_stage("Copying game to USB", 0);
    let progress = ProgressDisplay::new();
    progress.update("Copying game to USB", 0);
    let mut copied = 0_u64;

    for file in files {
        let target = destination.join(Path::new(&file.relative_path));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut source = File::open(&file.local_path)?;
        let mut target_file = File::create(&target)
            .with_context(|| format!("could not create {}", target.display()))?;
        let mut file_copied = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            target_file.write_all(&buffer[..count])?;
            file_copied += count as u64;
            let current = copied.saturating_add(file_copied);
            let percent = (current.saturating_mul(100) / total_bytes).min(100) as u8;
            progress.update("Copying game to USB", percent);
        }
        target_file.flush()?;
        copied = copied.saturating_add(file.size);
    }

    progress.finish("USB copy complete");
    println!("Game copied successfully to {}.", drive.label);
    println!("Safely eject the USB drive before unplugging it.");
    Ok(())
}

#[cfg(windows)]
fn detect_removable_drives() -> Result<Vec<RemovableDrive>, Error> {
    let script = concat!(
        "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; ",
        "$roots = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::OrdinalIgnoreCase); ",
        "[System.IO.DriveInfo]::GetDrives() | Where-Object { $_.IsReady -and $_.DriveType -eq [System.IO.DriveType]::Removable } | ForEach-Object { [void]$roots.Add($_.Name) }; ",
        "try { Get-Disk -ErrorAction Stop | Where-Object BusType -eq 'USB' | ForEach-Object { $_ | Get-Partition -ErrorAction Stop | Where-Object DriveLetter | ForEach-Object { [void]$roots.Add(('{0}:\\' -f $_.DriveLetter)) } } } catch {}; ",
        "$roots | ForEach-Object { $drive = New-Object System.IO.DriveInfo($_); if ($drive.IsReady) { '{0}|{1}|{2}' -f $drive.Name, $drive.VolumeLabel, $drive.AvailableFreeSpace } }"
    );
    let output = Command::new("powershell.exe")
        .arg("-NoLogo")
        .arg("-NoProfile")
        .arg("-Command")
        .arg(script)
        .output()
        .context("could not ask Windows PowerShell for removable drives")?;
    if !output.status.success() {
        anyhow::bail!(
            "Windows PowerShell could not search for removable drives: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(parse_windows_removable_drives(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

#[cfg(windows)]
fn parse_windows_removable_drives(output: &str) -> Vec<RemovableDrive> {
    let mut drives = Vec::new();
    for line in output.lines() {
        let mut fields = line.trim().splitn(3, '|');
        let Some(device_id) = fields.next().filter(|value| !value.is_empty()) else {
            continue;
        };
        let device_id = device_id.trim_end_matches(['\\', '/']);
        let volume_name = fields.next().unwrap_or_default();
        let free_bytes = fields
            .next()
            .unwrap_or_default()
            .parse::<u64>()
            .unwrap_or(0);
        let label = if volume_name.is_empty() {
            format!("Removable drive ({device_id})")
        } else {
            format!("{volume_name} ({device_id})")
        };
        drives.push(RemovableDrive {
            root: PathBuf::from(format!("{device_id}\\")),
            label,
            free_bytes,
        });
    }
    drives.sort_by(|left, right| left.root.cmp(&right.root));
    drives
}

#[cfg(any(target_os = "linux", test))]
#[derive(serde::Deserialize)]
struct LinuxBlockDeviceList {
    blockdevices: Vec<LinuxBlockDevice>,
}

#[cfg(any(target_os = "linux", test))]
#[derive(serde::Deserialize)]
struct LinuxBlockDevice {
    name: String,
    #[serde(default)]
    tran: Option<String>,
    #[serde(default)]
    rm: Option<bool>,
    #[serde(default)]
    mountpoints: Option<Vec<Option<String>>>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    fsavail: Option<u64>,
    #[serde(default)]
    children: Vec<LinuxBlockDevice>,
}

#[cfg(target_os = "linux")]
fn detect_removable_drives() -> Result<Vec<RemovableDrive>, Error> {
    let output = Command::new("lsblk")
        .args([
            "--json",
            "--bytes",
            "--output",
            "NAME,TRAN,RM,MOUNTPOINTS,LABEL,FSAVAIL",
        ])
        .output()
        .context("could not ask Linux lsblk for removable drives")?;
    if !output.status.success() {
        anyhow::bail!(
            "Linux could not search for removable drives: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_linux_removable_drives(&output.stdout)
}

#[cfg(any(target_os = "linux", test))]
fn parse_linux_removable_drives(output: &[u8]) -> Result<Vec<RemovableDrive>, Error> {
    let devices: LinuxBlockDeviceList =
        serde_json::from_slice(output).context("lsblk returned invalid drive information")?;
    let mut drives = Vec::new();
    for device in &devices.blockdevices {
        collect_linux_removable_drives(device, false, &mut drives);
    }
    drives.sort_by(|left, right| left.root.cmp(&right.root));
    drives.dedup_by(|left, right| left.root == right.root);
    Ok(drives)
}

#[cfg(any(target_os = "linux", test))]
fn collect_linux_removable_drives(
    device: &LinuxBlockDevice,
    removable_parent: bool,
    drives: &mut Vec<RemovableDrive>,
) {
    let removable = removable_parent
        || device.rm.unwrap_or(false)
        || device
            .tran
            .as_deref()
            .is_some_and(|transport| transport.eq_ignore_ascii_case("usb"));
    if removable {
        for mountpoint in device.mountpoints.iter().flatten().flatten() {
            let root = PathBuf::from(mountpoint);
            let name = device.label.as_deref().unwrap_or(&device.name);
            drives.push(RemovableDrive {
                root,
                label: format!("{name} ({mountpoint})"),
                free_bytes: device.fsavail.unwrap_or(0),
            });
        }
    }
    for child in &device.children {
        collect_linux_removable_drives(child, removable, drives);
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn detect_removable_drives() -> Result<Vec<RemovableDrive>, Error> {
    anyhow::bail!("USB drive detection is unsupported on this operating system")
}

fn format_byte_size(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / GIB)
    } else {
        format!("{:.1} MB", bytes as f64 / MIB)
    }
}

#[derive(Clone)]
struct FtpSettings {
    host: String,
    port: u16,
    background_port: Option<u16>,
    username: String,
    password: String,
    destination_path: String,
}

fn upload_to_xbox(output: &ConversionOutput) -> Result<(), Error> {
    println!("\nXbox 360 FTP upload");
    println!("-------------------");
    let (settings, save_after_login) = get_ftp_settings()?;

    println!(
        "Connecting to {}:{} as {}...",
        settings.host, settings.port, settings.username
    );
    report_gui_stage("Connecting to Xbox 360", 0);
    let ftp = store_worker::delivery_connection(&settings)?;
    report_gui_progress("Connecting to Xbox 360", 100);
    if save_after_login {
        save_ftp_settings(&settings)?;
        save_password(&settings.username, &settings.password)?;
        println!("FTP details saved for future use.");
    }

    let drives = detect_storage_drives(&ftp)?;
    let drive = choose_storage_drive(&drives)?;
    let remote_base = format!(
        "/{}/Content/0000000000000000/{}",
        drive.remote_name, output.title_id
    );

    upload_converted_files(&ftp, output, &remote_base, &drive.label)
}

fn upload_to_saved_xbox(output: &ConversionOutput) -> Result<(), Error> {
    let mut settings = load_ftp_settings()?.context(
        "Please configure and log in to your Xbox 360 FTP connection before continuing.",
    )?;
    settings.password = load_saved_password()?.context(
        "Please configure and log in to your Xbox 360 FTP connection before continuing.",
    )?;
    report_gui_stage("Connecting to Xbox 360", 0);
    let ftp = RcloneFtp::new(&settings)?.connect_available()?;
    report_gui_progress("Connecting to Xbox 360", 100);
    let drives = detect_storage_drives(&ftp)?;
    let drive = choose_storage_drive(&drives)?;
    let remote_base = format!(
        "/{}/Content/0000000000000000/{}",
        drive.remote_name, output.title_id
    );
    upload_converted_files(&ftp, output, &remote_base, "Xbox 360")
}

fn upload_converted_files(
    ftp: &RcloneFtp,
    output: &ConversionOutput,
    remote_base: &str,
    destination_label: &str,
) -> Result<(), Error> {
    upload_directory_files(ftp, &output.local_title_dir, remote_base, destination_label)
}

fn upload_directory_files(
    ftp: &RcloneFtp,
    source: &Path,
    remote_base: &str,
    destination_label: &str,
) -> Result<(), Error> {
    let files = collect_upload_files(source)?;
    let total_bytes = files.iter().map(|file| file.size).sum::<u64>();
    if files.is_empty() || total_bytes == 0 {
        anyhow::bail!("no prepared game files were found in {}", source.display());
    }

    println!(
        "\nUploading {} file(s) to {} ({})...",
        files.len(),
        destination_label,
        remote_base
    );
    report_gui_stage("Uploading to Xbox through FTP", 0);
    println!("Uploading to Xbox through FTP");
    // A missing destination must never be accepted as a complete upload. Some
    // Xbox FTP servers return an ambiguous size response for both missing and
    // unusual files, so only attempt the resume/repair shortcut when the title
    // directory itself is present.
    for _ in 0..if ftp.directory_exists(remote_base) {
        16
    } else {
        0
    } {
        match ftp.verify_directory(source, remote_base) {
            Ok(()) => {
                report_gui_progress("Verifying FTP transfer", 100);
                println!("Xbox already has the complete verified game; upload skipped.");
                return Ok(());
            }
            Err(error) => {
                println!("Xbox verification requires repair: {error:#}");
                let Some((local_file, remote_file)) =
                    verification_repair_target(&error, source, remote_base)
                else {
                    break;
                };
                render_progress_screen(
                    "Preparing game",
                    "Repairing Xbox installation",
                    96,
                    Some(&format!("Replacing {}", local_file.display())),
                );
                ftp.copy_file_once(&local_file, &remote_file)?;
            }
        }
    }
    let active_port = ftp.copy_directory(source, remote_base)?;
    verify_and_repair_ftp(ftp, source, remote_base, active_port)?;
    report_gui_progress("Verifying FTP transfer", 100);
    println!("Game uploaded and verified successfully to {destination_label}.");
    Ok(())
}

fn verify_and_repair_ftp(
    ftp: &RcloneFtp,
    source: &Path,
    remote_base: &str,
    mut active_port: u16,
) -> Result<(), Error> {
    report_gui_stage("Verifying FTP transfer", 0);
    for repair_pass in 0..=2 {
        render_progress_screen(
            "Preparing game",
            "Checking Xbox installation",
            95,
            Some("Comparing every prepared file with the selected Xbox drive"),
        );
        match ftp.verify_directory_with_failover(source, remote_base, active_port) {
            Ok(()) => return Ok(()),
            Err(error) if repair_pass < 2 => {
                render_progress_screen(
                    "Preparing game",
                    "Repairing Xbox installation",
                    96,
                    Some("Uploading missing or wrong-sized files; completed files are skipped"),
                );
                active_port = ftp.copy_directory(source, remote_base).with_context(|| {
                    format!("could not repair the incomplete Xbox installation: {error:#}")
                })?;
            }
            Err(error) => {
                return Err(error).context(
                    "Xbox installation is still incomplete after two automatic repair passes",
                );
            }
        }
    }
    unreachable!()
}

fn verification_repair_target(
    error: &Error,
    source: &Path,
    remote_base: &str,
) -> Option<(PathBuf, String)> {
    let message = format!("{error:#}");
    let remote_file = if let Some((_, tail)) = message.split_once("missing file: ") {
        tail.lines().next()?.trim()
    } else if let Some((_, tail)) = message.split_once("verification failed for ") {
        tail.split(": expected").next()?.trim()
    } else {
        return None;
    };
    let prefix = format!("{}/", remote_base.trim_end_matches('/'));
    let relative = remote_file.strip_prefix(&prefix)?;
    let local_file = source.join(relative.replace('/', &std::path::MAIN_SEPARATOR.to_string()));
    local_file
        .is_file()
        .then(|| (local_file, remote_file.to_owned()))
}

fn get_ftp_settings() -> Result<(FtpSettings, bool), Error> {
    if let Some(mut settings) = load_ftp_settings()?
        && let Some(password) = load_saved_password()?
    {
        settings.password = password;
        println!(
            "Saved connection: {}@{}:{}",
            settings.username, settings.host, settings.port
        );
        if prompt_yes_no("Use the saved FTP connection?", true)? {
            return Ok((settings, false));
        }
    }

    println!("Enter the FTP details shown by your Xbox dashboard.");
    let host = normalise_ftp_host(&prompt_text("Xbox IP address or hostname", None)?)?;
    let port_text = prompt_text("FTP port", Some("21"))?;
    let port = port_text
        .parse::<u16>()
        .context("FTP port must be a number from 1 to 65535")?;
    let username = prompt_text("FTP username", Some(DEFAULT_FTP_USERNAME))?;
    let show_password = prompt_yes_no("Show password while typing?", false)?;
    let password = if show_password {
        prompt_text("FTP password", None)?
    } else {
        rpassword::prompt_password("FTP password (hidden): ")
            .context("could not read FTP password")?
    };
    if password.is_empty() {
        anyhow::bail!("FTP password is required");
    }

    Ok((
        FtpSettings {
            host,
            port,
            background_port: Some(DEFAULT_BACKGROUND_FTP_PORT),
            username,
            password,
            destination_path: DEFAULT_FTP_DESTINATION.to_owned(),
        },
        true,
    ))
}

fn prompt_text(prompt: &str, default: Option<&str>) -> Result<String, Error> {
    match default {
        Some(default) => print!("{prompt} [{default}]: "),
        None => print!("{prompt}: "),
    }
    io::stdout().flush().context("error writing prompt")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("error reading input")?;
    let input = input.trim();
    let value = if input.is_empty() {
        default.unwrap_or_default()
    } else {
        input
    };
    if value.is_empty() {
        anyhow::bail!("{prompt} is required");
    }
    if value.contains(['\r', '\n']) {
        anyhow::bail!("{prompt} contains invalid characters");
    }
    Ok(value.to_owned())
}

fn prompt_yes_no(prompt: &str, default: bool) -> Result<bool, Error> {
    Ok(prompt_yes_no_key(prompt, default)?.unwrap_or(default))
}

fn normalise_ftp_host(host: &str) -> Result<String, Error> {
    let mut host = host.trim();
    for label in ["host/ip address:", "ip address:", "host:", "ip:"] {
        if host
            .get(..label.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(label))
        {
            host = host[label.len()..].trim();
            break;
        }
    }
    let host = host
        .strip_prefix("ftp://")
        .or_else(|| host.strip_prefix("FTP://"))
        .unwrap_or(host)
        .trim()
        .trim_end_matches('/')
        .trim();
    if host.is_empty() || host.contains(['/', '\\', '\r', '\n']) {
        anyhow::bail!("invalid Xbox FTP address");
    }
    Ok(host.to_owned())
}

fn ftp_settings_path() -> Result<PathBuf, Error> {
    Ok(app_settings::config_dir()?.join("ftp-settings.txt"))
}

fn save_ftp_settings(settings: &FtpSettings) -> Result<(), Error> {
    let path = ftp_settings_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("could not create the settings folder")?;
    }
    let contents = format!(
        "host={}\nport={}\nbackground_port={}\nusername={}\ndestination_path={}\n",
        settings.host,
        settings.port,
        settings
            .background_port
            .unwrap_or(DEFAULT_BACKGROUND_FTP_PORT),
        settings.username,
        settings.destination_path
    );
    fs::write(&path, contents).context("could not save FTP settings")
}

fn load_ftp_settings() -> Result<Option<FtpSettings>, Error> {
    if let Some(settings) = store_worker::configured_ftp()? {
        return Ok(Some(settings));
    }
    load_ftp_settings_from(&ftp_settings_path()?)
}

fn load_ftp_settings_from(path: &Path) -> Result<Option<FtpSettings>, Error> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("could not read saved FTP settings"),
    };
    let value = |key: &str| {
        contents
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .map(str::to_owned)
    };
    let (Some(host), Some(port), Some(username)) =
        (value("host"), value("port"), value("username"))
    else {
        return Ok(None);
    };
    let host = normalise_ftp_host(&host).context("saved FTP host is invalid")?;
    let port = port
        .trim()
        .parse::<u16>()
        .context("saved FTP port is invalid")?;
    let username = username.trim().to_owned();
    if username.is_empty() {
        anyhow::bail!("saved FTP username is invalid");
    }
    Ok(Some(FtpSettings {
        host,
        port,
        background_port: value("background_port")
            .map(|port| port.trim().parse::<u16>())
            .transpose()
            .context("saved background FTP port is invalid")?
            .or(Some(DEFAULT_BACKGROUND_FTP_PORT)),
        username,
        password: String::new(),
        destination_path: value("destination_path")
            .unwrap_or_else(|| DEFAULT_FTP_DESTINATION.to_owned()),
    }))
}

fn platform_ftp_settings_path(platform: iso2god::forge::Platform) -> Result<PathBuf, Error> {
    Ok(app_settings::config_dir()?
        .join("ftp")
        .join(format!("{}.txt", platform.short_code())))
}

fn save_platform_ftp_settings(
    platform: iso2god::forge::Platform,
    settings: &FtpSettings,
) -> Result<(), Error> {
    let path = platform_ftp_settings_path(platform)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("could not create the FTP settings folder")?;
    }
    let contents = format!(
        "host={}\nport={}\nusername={}\ndestination_path={}\n",
        settings.host, settings.port, settings.username, settings.destination_path
    );
    fs::write(path, contents).context("could not save the console FTP settings")
}

fn load_platform_ftp_settings(
    platform: iso2god::forge::Platform,
) -> Result<Option<FtpSettings>, Error> {
    load_ftp_settings_from(&platform_ftp_settings_path(platform)?)
}

fn load_saved_password() -> Result<Option<String>, Error> {
    if let Some(settings) = store_worker::configured_ftp()? {
        return Ok(Some(settings.password));
    }
    load_local_saved_password()
}

#[cfg(windows)]
const CREDENTIAL_TARGET: &str = "iso2god Xbox FTP";

#[cfg(windows)]
#[repr(C)]
struct WindowsCredential {
    flags: u32,
    credential_type: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written_low: u32,
    last_written_high: u32,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut std::ffi::c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

#[cfg(windows)]
#[link(name = "Advapi32")]
unsafe extern "system" {
    fn CredWriteW(credential: *const WindowsCredential, flags: u32) -> i32;
    fn CredReadW(
        target_name: *const u16,
        credential_type: u32,
        flags: u32,
        credential: *mut *mut WindowsCredential,
    ) -> i32;
    fn CredFree(buffer: *mut std::ffi::c_void);
}

#[cfg(windows)]
fn save_password(username: &str, password: &str) -> Result<(), Error> {
    save_windows_credential(CREDENTIAL_TARGET, username, password)
}

#[cfg(windows)]
fn save_windows_credential(target_name: &str, username: &str, password: &str) -> Result<(), Error> {
    const CRED_TYPE_GENERIC: u32 = 1;
    const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;

    let mut target = target_name
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut username = username.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut password = password.encode_utf16().collect::<Vec<_>>();
    let credential = WindowsCredential {
        flags: 0,
        credential_type: CRED_TYPE_GENERIC,
        target_name: target.as_mut_ptr(),
        comment: std::ptr::null_mut(),
        last_written_low: 0,
        last_written_high: 0,
        credential_blob_size: (password.len() * std::mem::size_of::<u16>()) as u32,
        credential_blob: password.as_mut_ptr().cast(),
        persist: CRED_PERSIST_LOCAL_MACHINE,
        attribute_count: 0,
        attributes: std::ptr::null_mut(),
        target_alias: std::ptr::null_mut(),
        user_name: username.as_mut_ptr(),
    };
    let written = unsafe { CredWriteW(&credential, 0) };
    password.fill(0);
    if written == 0 {
        return Err(io::Error::last_os_error()).context("could not save password securely");
    }
    Ok(())
}

#[cfg(windows)]
fn load_local_saved_password() -> Result<Option<String>, Error> {
    load_windows_credential(CREDENTIAL_TARGET)
}

#[cfg(windows)]
fn load_windows_credential(target_name: &str) -> Result<Option<String>, Error> {
    const CRED_TYPE_GENERIC: u32 = 1;
    let target = target_name
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut credential = std::ptr::null_mut();
    let read = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
    if read == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(1168) {
            return Ok(None);
        }
        return Err(error).context("could not read the saved FTP password");
    }

    let password = unsafe {
        let credential_ref = &*credential;
        let length = credential_ref.credential_blob_size as usize / std::mem::size_of::<u16>();
        if length == 0 {
            Ok(String::new())
        } else {
            let value =
                std::slice::from_raw_parts(credential_ref.credential_blob.cast::<u16>(), length);
            String::from_utf16(value).context("saved FTP password is invalid")
        }
    };
    unsafe { CredFree(credential.cast()) };
    password.map(Some)
}

#[cfg(target_os = "linux")]
fn save_password(_username: &str, password: &str) -> Result<(), Error> {
    save_linux_secret("xbox-ftp", "ISO 2 GOD Xbox FTP", password)
}

#[cfg(target_os = "linux")]
fn save_linux_secret(account: &str, label: &str, password: &str) -> Result<(), Error> {
    let mut child = Command::new("secret-tool")
        .args([
            "store",
            &format!("--label={label}"),
            "application",
            "iso2god",
            "account",
            account,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .context(
            "Linux Secret Service is unavailable; install libsecret-tools to save the password securely",
        )?;
    child
        .stdin
        .take()
        .context("could not securely pass the password to Linux Secret Service")?
        .write_all(password.as_bytes())?;
    let status = child
        .wait()
        .context("error waiting for Linux Secret Service")?;
    if !status.success() {
        anyhow::bail!("Linux Secret Service could not save the FTP password ({status})");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn load_local_saved_password() -> Result<Option<String>, Error> {
    load_linux_secret("xbox-ftp")
}

#[cfg(target_os = "linux")]
fn load_linux_secret(account: &str) -> Result<Option<String>, Error> {
    let output = match Command::new("secret-tool")
        .args(["lookup", "application", "iso2god", "account", account])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("could not start Linux Secret Service"),
    };
    if !output.status.success() {
        if output.stderr.is_empty() {
            return Ok(None);
        }
        anyhow::bail!(
            "Linux Secret Service could not read the FTP password: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let password = String::from_utf8(output.stdout)
        .context("Linux Secret Service returned an invalid password")?
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    Ok((!password.is_empty()).then_some(password))
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn save_password(_username: &str, _password: &str) -> Result<(), Error> {
    anyhow::bail!("secure FTP password saving is unsupported on this operating system")
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn load_local_saved_password() -> Result<Option<String>, Error> {
    Ok(None)
}

fn platform_credential_key(platform: iso2god::forge::Platform) -> String {
    format!("digital-forge FTP {}", platform.short_code())
}

#[cfg(windows)]
fn save_platform_ftp_password(
    platform: iso2god::forge::Platform,
    username: &str,
    password: &str,
) -> Result<(), Error> {
    save_windows_credential(&platform_credential_key(platform), username, password)
}

#[cfg(windows)]
fn load_platform_ftp_password(platform: iso2god::forge::Platform) -> Result<Option<String>, Error> {
    load_windows_credential(&platform_credential_key(platform))
}

#[cfg(target_os = "linux")]
fn save_platform_ftp_password(
    platform: iso2god::forge::Platform,
    _username: &str,
    password: &str,
) -> Result<(), Error> {
    let account = platform_credential_key(platform);
    save_linux_secret(&account, "Digital Forge console FTP", password)
}

#[cfg(target_os = "linux")]
fn load_platform_ftp_password(platform: iso2god::forge::Platform) -> Result<Option<String>, Error> {
    load_linux_secret(&platform_credential_key(platform))
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn save_platform_ftp_password(
    _platform: iso2god::forge::Platform,
    _username: &str,
    password: &str,
) -> Result<(), Error> {
    if password.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("secure FTP password saving is unsupported on this operating system")
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn load_platform_ftp_password(
    _platform: iso2god::forge::Platform,
) -> Result<Option<String>, Error> {
    Ok(None)
}

struct StorageDrive {
    remote_name: String,
    label: String,
    external: bool,
}

fn storage_drive_from_remote_name(name: &str) -> Option<StorageDrive> {
    let name = name.trim().trim_matches('/');
    if name.is_empty() {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    let (label, external) = if lower == "hdd1" || lower == "onboardmu" {
        (format!("Internal drive ({name})"), false)
    } else if lower.starts_with("usb") {
        (
            if lower == "usb0" {
                format!("External drive ({name}) — commonly the exploit drive")
            } else {
                format!("External drive ({name})")
            },
            true,
        )
    } else {
        return None;
    };
    Some(StorageDrive {
        remote_name: name.to_owned(),
        label,
        external,
    })
}

fn detect_storage_drives(ftp: &RcloneFtp) -> Result<Vec<StorageDrive>, Error> {
    let listed = ftp.list_directories("/");
    let listing_failed = listed.is_err();
    let entries = listed.unwrap_or_default();
    let mut drives = Vec::new();
    for entry in entries {
        let name = entry
            .trim()
            .trim_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        let Some(drive) = storage_drive_from_remote_name(name) else {
            continue;
        };
        if !drives
            .iter()
            .any(|drive: &StorageDrive| drive.remote_name.eq_ignore_ascii_case(name))
        {
            drives.push(drive);
        }
    }

    // Only probe conventional names when the FTP server cannot provide a root
    // listing. A successful listing is authoritative and can include any
    // number of attached USB devices or plugin-specific mount names.
    if listing_failed || drives.is_empty() {
        let mut candidates = vec!["Hdd1".to_owned(), "OnBoardMU".to_owned()];
        for index in 0..16 {
            candidates.push(format!("Usb{index}"));
            candidates.push(format!("UsbMU{index}"));
        }
        for name in candidates {
            if ftp.directory_exists(&format!("/{name}")) {
                if let Some(drive) = storage_drive_from_remote_name(&name) {
                    drives.push(drive);
                }
            }
        }
    }
    drives.sort_by_key(|drive| (drive.external, drive.remote_name.clone()));
    if drives.is_empty() {
        anyhow::bail!(
            "the Xbox FTP server did not report an internal or external drive (expected Hdd1 or Usb0)"
        );
    }
    Ok(drives)
}

fn choose_storage_drive(drives: &[StorageDrive]) -> Result<&StorageDrive, Error> {
    if let Some(requested) = env::var_os("ISO2GOD_FTP_DRIVE") {
        let requested = requested.to_string_lossy();
        return drives
            .iter()
            .find(|drive| drive.remote_name.eq_ignore_ascii_case(&requested))
            .context("the queued Xbox destination drive is no longer connected");
    }
    render_terminal_header()?;
    println!("Choose the destination drive currently connected to the Xbox.\n");
    let options = drives
        .iter()
        .enumerate()
        .map(|(index, drive)| MenuOption {
            value: index,
            label: format!("{}. {}", index + 1, drive.label),
        })
        .collect::<Vec<_>>();
    let selected = select_menu(&options)?.context("Xbox drive selection was cancelled")?;
    drives
        .get(selected)
        .context("the selected Xbox drive is no longer available")
}

fn choose_saved_xbox_storage_name() -> Result<String, Error> {
    let mut settings = load_ftp_settings()?.context("saved FTP settings are missing")?;
    settings.password = load_saved_password()?.context("saved FTP password is missing")?;
    let ftp = RcloneFtp::new(&settings)?.connect_available()?;
    let drives = detect_storage_drives(&ftp)?;
    Ok(choose_storage_drive(&drives)?.remote_name.clone())
}

struct UploadFile {
    local_path: PathBuf,
    relative_path: String,
    size: u64,
}

fn collect_upload_files(root: &Path) -> Result<Vec<UploadFile>, Error> {
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("could not read {}", directory.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file() {
                let local_path = entry.path();
                let relative_path = local_path
                    .strip_prefix(root)?
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                files.push(UploadFile {
                    size: entry.metadata()?.len(),
                    local_path,
                    relative_path,
                });
            }
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

struct RcloneFtp {
    executable: PathBuf,
    _embedded_tool: Option<Arc<EmbeddedRclone>>,
    environment: Vec<(OsString, OsString)>,
    host: String,
    primary_port: u16,
    port: u16,
    background_port: Option<u16>,
    username: String,
    password: String,
}

impl RcloneFtp {
    fn transfer_path(&self, path: &str) -> String {
        background_ftp_path(path, self.port == DEFAULT_BACKGROUND_FTP_PORT)
    }

    fn remote(&self, path: &str) -> String {
        rclone_remote(&self.transfer_path(path))
    }

    fn login(&self) -> (&str, &str) {
        if self.port == DEFAULT_BACKGROUND_FTP_PORT {
            ("xbox", "xbox")
        } else {
            (&self.username, &self.password)
        }
    }
    fn new(settings: &FtpSettings) -> Result<Self, Error> {
        if let Some(executable) = env::var_os("ISO2GOD_RCLONE_PATH").map(PathBuf::from)
            && executable.is_file()
        {
            return Self::new_with_executable(settings, executable);
        }
        let embedded_tool = extract_embedded_rclone()?;
        let executable = embedded_tool.executable.clone();
        Self::new_with_tool(settings, executable, Some(Arc::new(embedded_tool)))
    }

    fn new_with_executable(settings: &FtpSettings, executable: PathBuf) -> Result<Self, Error> {
        Self::new_with_tool(settings, executable, None)
    }

    fn new_with_tool(
        settings: &FtpSettings,
        executable: PathBuf,
        embedded_tool: Option<Arc<EmbeddedRclone>>,
    ) -> Result<Self, Error> {
        let obscured_password = obscure_rclone_password(&executable, &settings.password)?;
        Ok(Self {
            executable,
            _embedded_tool: embedded_tool,
            host: settings.host.clone(),
            primary_port: settings.port,
            port: settings.port,
            background_port: settings
                .background_port
                .filter(|port| *port != settings.port),
            username: settings.username.clone(),
            password: settings.password.clone(),
            environment: vec![
                ("RCLONE_CONFIG_XBOX_TYPE".into(), "ftp".into()),
                (
                    "RCLONE_CONFIG_XBOX_HOST".into(),
                    settings.host.clone().into(),
                ),
                (
                    "RCLONE_CONFIG_XBOX_USER".into(),
                    settings.username.clone().into(),
                ),
                (
                    "RCLONE_CONFIG_XBOX_PORT".into(),
                    settings.port.to_string().into(),
                ),
                ("RCLONE_CONFIG_XBOX_PASS".into(), obscured_password.into()),
                ("RCLONE_CONFIG_XBOX_DISABLE_EPSV".into(), "true".into()),
                ("RCLONE_CONFIG_XBOX_DISABLE_MLSD".into(), "true".into()),
                ("RCLONE_CONFIG_XBOX_DISABLE_UTF8".into(), "true".into()),
                ("RCLONE_CONFIG_XBOX_CONCURRENCY".into(), "3".into()),
            ],
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.envs(self.environment.iter().cloned());
        command.env(
            "RCLONE_CONFIG",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
        command
    }

    fn test_connection(&self) -> Result<(), Error> {
        self.run_checked([
            "lsd",
            "xbox:/",
            "--max-depth",
            "1",
            "--contimeout",
            "3s",
            "--timeout",
            "5s",
            "--retries",
            "1",
            "--low-level-retries",
            "1",
        ])
        .map(|_| ())
        .context("rclone could not connect and log in to the Xbox FTP server")
    }

    fn connect_available(&self) -> Result<Self, Error> {
        let mut last_error = None;
        let mut ports = vec![self.port];
        for port in [
            self.primary_port,
            self.background_port.unwrap_or(self.primary_port),
        ] {
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
        for port in ports {
            let endpoint = self.with_port(port)?;
            match endpoint.test_connection() {
                Ok(()) => {
                    return Ok(endpoint);
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.context("no Xbox FTP service was available")?).context(format!(
            "Could not connect on dashboard port {} or background port {}",
            self.primary_port,
            self.background_port.unwrap_or(DEFAULT_BACKGROUND_FTP_PORT)
        ))
    }

    fn list_directories(&self, remote_path: &str) -> Result<Vec<String>, Error> {
        let remote = self.remote(remote_path);
        let output = self.run_checked([
            "lsjson",
            remote.as_str(),
            "--dirs-only",
            "--no-mimetype",
            "--no-modtime",
        ])?;
        let entries: Vec<RcloneDirectory> =
            serde_json::from_slice(&output).context("rclone returned an invalid directory list")?;
        Ok(entries
            .into_iter()
            .map(|entry| {
                if remote_path == "/" && self.port == DEFAULT_BACKGROUND_FTP_PORT {
                    canonical_background_drive(&entry.path)
                } else {
                    entry.path
                }
            })
            .collect())
    }

    fn directory_exists(&self, remote_path: &str) -> bool {
        curl_ftp_directory_exists(
            &self.host,
            self.port,
            self.login().0,
            self.login().1,
            &self.transfer_path(remote_path),
        )
    }

    fn create_directory(&self, remote_path: &str) -> Result<(), Error> {
        let remote = self.remote(remote_path);
        self.run_checked(["mkdir", remote.as_str()])
            .map(|_| ())
            .context("could not create the Xbox plugin directory")
    }

    fn upload_installer_file(&self, source: &Path, remote_path: &str) -> Result<(), Error> {
        let executable = find_curl()?;
        let url = ftp_url(
            &self.host,
            self.port,
            &self.transfer_path(remote_path),
            false,
        );
        let mut child = Command::new(executable)
            .args([
                "--config",
                "-",
                "--fail",
                "--silent",
                "--show-error",
                "--disable-epsv",
                "--ftp-create-dirs",
                "--max-time",
                "90",
                "--upload-file",
            ])
            .arg(source)
            .arg("--url")
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not start the Xbox plugin upload")?;
        let credentials = format!("{}:{}", self.login().0, self.login().1);
        let config = format!("user = \"{}\"\n", curl_config_value(&credentials));
        child
            .stdin
            .take()
            .context("could not securely pass the FTP credentials to curl")?
            .write_all(config.as_bytes())?;
        let output = child
            .wait_with_output()
            .context("Xbox plugin upload did not complete")?;
        if !output.status.success() {
            anyhow::bail!(
                "Xbox plugin upload failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn file_size(&self, remote_path: &str) -> Result<u64, Error> {
        let remote = self.remote(remote_path);
        let output = self.run_checked([
            "lsjson",
            remote.as_str(),
            "--stat",
            "--no-mimetype",
            "--no-modtime",
        ])?;
        let value: serde_json::Value =
            serde_json::from_slice(&output).context("FTP returned invalid file metadata")?;
        value
            .get("Size")
            .and_then(serde_json::Value::as_u64)
            .context("FTP did not report the remote file size")
    }

    fn listed_file_size(&self, remote_path: &str) -> Result<u64, Error> {
        let normalised = remote_path.trim_end_matches('/');
        let (parent, name) = normalised
            .rsplit_once('/')
            .context("remote file path has no parent directory")?;
        let parent = if parent.is_empty() { "/" } else { parent };
        let remote = self.remote(parent);
        let output = self.run_checked([
            "lsjson",
            remote.as_str(),
            "--files-only",
            "--no-mimetype",
            "--no-modtime",
        ])?;
        let files: Vec<RcloneFile> = serde_json::from_slice(&output)
            .context("FTP returned an invalid directory file manifest")?;
        let available = files
            .iter()
            .map(|file| format!("{} ({} bytes)", file.path, file.size))
            .collect::<Vec<_>>()
            .join(", ");
        files
            .into_iter()
            .find(|file| {
                file.path
                    .replace('\\', "/")
                    .rsplit('/')
                    .next()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            })
            .map(|file| file.size)
            .with_context(|| {
                format!(
                    "object not found on Xbox FTP: {remote_path}; directory contained: {available}"
                )
            })
    }

    fn read_file(&self, remote_path: &str) -> Result<Vec<u8>, Error> {
        curl_ftp_read_file(
            &self.host,
            self.port,
            self.login().0,
            self.login().1,
            &self.transfer_path(remote_path),
        )
    }

    fn copy_directory(&self, source: &Path, remote_path: &str) -> Result<u16, Error> {
        if self.background_port.is_none() && env::var_os("DIGITAL360_STAY_OPEN").is_none() {
            return self
                .copy_directory_once(source, remote_path)
                .map(|_| self.port);
        }
        let mut last_error = anyhow::anyhow!("Xbox FTP is unavailable");
        let ports = if env::var_os("DIGITAL360_STAY_OPEN").is_some() {
            vec![self.port]
        } else if let Some(background) = self.background_port {
            vec![self.primary_port, background]
        } else {
            vec![self.primary_port]
        };
        for _ in 0..180 {
            for &port in &ports {
                let endpoint = self.with_port(port)?;
                if endpoint.test_connection().is_err() {
                    continue;
                }
                render_progress_screen(
                    "Preparing game",
                    "Uploading to Xbox through FTP",
                    0,
                    Some(&format!(
                        "Using {} FTP (port {port})",
                        if port == self.primary_port {
                            "dashboard"
                        } else {
                            "background"
                        }
                    )),
                );
                match endpoint.copy_directory_once(source, remote_path) {
                    Ok(()) => {
                        return Ok(port);
                    }
                    Err(error) => {
                        let route_changed = error.downcast_ref::<FtpConnectionLost>().is_some();
                        last_error = error;
                        render_progress_screen(
                            "Preparing game",
                            "Transfer paused",
                            0,
                            Some("Keeping completed files; reconnecting automatically."),
                        );
                        if route_changed {
                            break;
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        Err(last_error).context(
            "Xbox FTP could not reconnect after repeated attempts; completed files are retained",
        )
    }

    fn copy_directory_once(&self, source: &Path, remote_path: &str) -> Result<(), Error> {
        let remote = self.remote(remote_path);
        let source = source.as_os_str();
        let mut command = self.command();
        command.arg("copy").arg(source).arg(remote).args([
            "--size-only",
            "--transfers",
            "1",
            "--checkers",
            "1",
            "--retries",
            "1",
            "--low-level-retries",
            "2",
            "--retries-sleep",
            "2s",
            "--contimeout",
            "5s",
            "--timeout",
            "15s",
            "--stats",
            "1s",
            "--use-json-log",
            "--log-level",
            "INFO",
            "--ftp-no-check-upload",
            "--inplace",
        ]);
        let label = if self.port == self.primary_port {
            "Uploading via dashboard FTP"
        } else {
            "Uploading via background FTP"
        };
        run_rclone_with_progress(command, label, self)
    }

    fn copy_file_once(&self, source: &Path, remote_path: &str) -> Result<(), Error> {
        let remote = self.remote(remote_path);
        let output = self
            .command()
            .arg("copyto")
            .arg(source)
            .arg(remote)
            .args([
                "--retries",
                "3",
                "--low-level-retries",
                "5",
                "--contimeout",
                "5s",
                "--timeout",
                "30s",
                "--ftp-no-check-upload",
                "--inplace",
            ])
            .output()
            .context("could not start the bundled rclone FTP repair")?;
        if !output.status.success() {
            anyhow::bail!("{}", rclone_error(&output));
        }
        Ok(())
    }

    fn verify_directory_with_failover(
        &self,
        source: &Path,
        remote_path: &str,
        preferred_port: u16,
    ) -> Result<(), Error> {
        let mut last_error = None;
        let mut ports = vec![preferred_port];
        for port in [
            self.primary_port,
            self.background_port.unwrap_or(self.primary_port),
        ] {
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
        for port in ports {
            let endpoint = self.with_port(port)?;
            match endpoint.verify_directory(source, remote_path) {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.context("no FTP endpoint was available for verification")?)
            .context("FTP verification failed on both dashboard and background services")
    }

    fn failover_ports(&self, current: u16) -> Vec<u16> {
        let mut ports = Vec::with_capacity(2);
        for port in [
            self.primary_port,
            self.background_port.unwrap_or(self.primary_port),
        ] {
            if port != current && !ports.contains(&port) {
                ports.push(port);
            }
        }
        if !ports.contains(&current) {
            ports.push(current);
        }
        ports
    }

    fn with_port(&self, port: u16) -> Result<Self, Error> {
        let settings = FtpSettings {
            host: self.host.clone(),
            port: self.primary_port,
            background_port: self.background_port,
            username: self.username.clone(),
            password: self.password.clone(),
            destination_path: DEFAULT_FTP_DESTINATION.to_owned(),
        };
        let mut endpoint = Self::new_with_tool(
            &settings,
            self.executable.clone(),
            self._embedded_tool.clone(),
        )?;
        endpoint.port = port;
        if port == DEFAULT_BACKGROUND_FTP_PORT {
            let password = obscure_rclone_password(&endpoint.executable, "xbox")?;
            for (name, value) in &mut endpoint.environment {
                if name == "RCLONE_CONFIG_XBOX_USER" {
                    *value = "xbox".into();
                }
                if name == "RCLONE_CONFIG_XBOX_PASS" {
                    *value = password.clone().into();
                }
            }
        }
        if let Some((_, configured_port)) = endpoint
            .environment
            .iter_mut()
            .find(|(name, _)| name == "RCLONE_CONFIG_XBOX_PORT")
        {
            *configured_port = port.to_string().into();
        }
        Ok(endpoint)
    }

    fn verify_directory(&self, source: &Path, remote_path: &str) -> Result<(), Error> {
        let files = collect_upload_files(source)?;
        if files.is_empty() {
            anyhow::bail!("no uploaded files were available to verify");
        }
        let mut directories = BTreeMap::<String, Vec<&UploadFile>>::new();
        for file in &files {
            let directory = file
                .relative_path
                .rsplit_once('/')
                .map(|(directory, _)| directory)
                .unwrap_or_default()
                .to_owned();
            directories.entry(directory).or_default().push(file);
        }
        let mut verified = 0_usize;
        for (directory, local_files) in directories {
            let remote_directory = if directory.is_empty() {
                remote_path.trim_end_matches('/').to_owned()
            } else {
                format!("{}/{}", remote_path.trim_end_matches('/'), directory)
            };
            let remote = self.remote(&remote_directory);
            let output = self.run_checked([
                "lsjson",
                remote.as_str(),
                "--files-only",
                "--no-mimetype",
                "--no-modtime",
            ])?;
            let remote_files: Vec<RcloneFile> = serde_json::from_slice(&output)
                .context("FTP returned an invalid directory file manifest")?;
            let remote_sizes = remote_files
                .into_iter()
                .map(|file| {
                    let name = file.path.replace('\\', "/");
                    (
                        name.rsplit('/').next().unwrap_or(&name).to_lowercase(),
                        file.size,
                    )
                })
                .collect::<BTreeMap<_, _>>();
            for file in local_files {
                let name = file
                    .relative_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&file.relative_path);
                let remote_file = format!(
                    "{}/{}",
                    remote_path.trim_end_matches('/'),
                    file.relative_path
                );
                let remote_size = match remote_sizes.get(&name.to_lowercase()).copied() {
                    Some(size) => size,
                    None => match self.file_size(&remote_file) {
                        Ok(size) => size,
                        Err(error)
                            if format!("{error:#}")
                                .contains("FTP did not report the remote file size") =>
                        {
                            // Rclone can return Size=-1 both for unusual existing
                            // files and for a missing path. Confirm existence and
                            // size independently before accepting the file.
                            curl_ftp_file_size(
                                &self.host,
                                self.port,
                                self.login().0,
                                self.login().1,
                                &self.transfer_path(&remote_file),
                            )
                            .with_context(|| {
                                format!("FTP verification found a missing file: {remote_file}")
                            })?
                        }
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("FTP verification found a missing file: {remote_file}")
                            });
                        }
                    },
                };
                if remote_size != file.size {
                    anyhow::bail!(
                        "FTP verification failed for {remote_file}: expected {} bytes, found {remote_size}",
                        file.size
                    );
                }
                verified += 1;
                report_gui_progress(
                    "Verifying FTP transfer",
                    (verified * 100 / files.len()) as u8,
                );
            }
        }
        Ok(())
    }

    fn run_checked<const N: usize>(&self, args: [&str; N]) -> Result<Vec<u8>, Error> {
        let output = self
            .command()
            .args(args)
            .output()
            .context("could not start the bundled rclone FTP engine")?;
        if !output.status.success() {
            anyhow::bail!("{}", rclone_error(&output));
        }
        Ok(output.stdout)
    }
}

#[derive(serde::Deserialize)]
struct RcloneDirectory {
    #[serde(rename = "Path")]
    path: String,
}

#[derive(serde::Deserialize)]
struct RcloneFile {
    #[serde(rename = "Path")]
    path: String,
    #[serde(rename = "Size")]
    size: u64,
}

#[cfg(all(windows, target_arch = "x86_64"))]
const EMBEDDED_RCLONE: &[u8] = include_bytes!("../../vendor/rclone/windows-x64.exe");
#[cfg(all(windows, target_arch = "x86"))]
const EMBEDDED_RCLONE: &[u8] = include_bytes!("../../vendor/rclone/windows-x86.exe");
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const EMBEDDED_RCLONE: &[u8] = include_bytes!("../../vendor/rclone/linux-x64");
#[cfg(all(target_os = "linux", target_arch = "x86"))]
const EMBEDDED_RCLONE: &[u8] = include_bytes!("../../vendor/rclone/linux-x86");

struct EmbeddedRclone {
    executable: PathBuf,
    directory: PathBuf,
}

impl Drop for EmbeddedRclone {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn extract_embedded_rclone() -> Result<EmbeddedRclone, Error> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let directory =
        env::temp_dir().join(format!("iso2god-rclone-{}-{timestamp}", std::process::id()));
    fs::create_dir(&directory).context("could not create temporary FTP engine folder")?;
    let executable = directory.join(if cfg!(windows) {
        "rclone.exe"
    } else {
        "rclone"
    });
    if let Err(error) = fs::write(&executable, EMBEDDED_RCLONE) {
        let _ = fs::remove_dir_all(&directory);
        return Err(error).context("could not prepare the embedded FTP engine");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .context("could not make the embedded FTP engine executable")?;
    }
    Ok(EmbeddedRclone {
        executable,
        directory,
    })
}

fn find_curl() -> Result<PathBuf, Error> {
    if let Some(path) = env::var_os("ISO2GOD_CURL_PATH") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }
    let executable_name = if cfg!(windows) { "curl.exe" } else { "curl" };
    if let Ok(current_exe) = env::current_exe()
        && let Some(directory) = current_exe.parent()
    {
        for candidate in [
            directory.join("Tools").join(executable_name),
            directory.join(executable_name),
        ] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    #[cfg(windows)]
    if let Some(system_root) = env::var_os("SystemRoot") {
        let candidate = PathBuf::from(system_root).join("System32").join("curl.exe");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    find_executable_on_path(&[executable_name]).context(
        "curl is required for Xbox FTP verification; install curl or reinstall the converter package",
    )
}

fn curl_ftp_file_size(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    remote_path: &str,
) -> Result<u64, Error> {
    let output = run_curl_ftp(host, port, username, password, remote_path, true)?;
    parse_curl_content_length(&output.stdout)
        .context("the Xbox FTP server did not report the uploaded file size")
}

fn curl_ftp_read_file(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    remote_path: &str,
) -> Result<Vec<u8>, Error> {
    let size = curl_ftp_file_size(host, port, username, password, remote_path)?;
    if size == 0 {
        return Ok(Vec::new());
    }
    let executable = find_curl()?;
    let url = ftp_url(host, port, remote_path, false);
    let range = format!("0-{}", size - 1);
    let mut child = Command::new(executable)
        .args([
            "--config",
            "-",
            "--silent",
            "--show-error",
            "--disable-epsv",
            "--max-time",
            "30",
            "--range",
            range.as_str(),
            "--url",
            url.as_str(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start curl for Xbox file verification")?;
    let credentials = format!("{username}:{password}");
    let config = format!("user = \"{}\"\n", curl_config_value(&credentials));
    child
        .stdin
        .take()
        .context("could not securely pass the FTP credentials to curl")?
        .write_all(config.as_bytes())?;
    let output = child
        .wait_with_output()
        .context("curl could not read the Xbox file")?;
    if output.stdout.len() as u64 != size {
        anyhow::bail!(
            "Xbox FTP returned {} of {size} bytes for {remote_path}: {}",
            output.stdout.len(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn curl_ftp_directory_exists(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    remote_path: &str,
) -> bool {
    run_curl_ftp(host, port, username, password, remote_path, false).is_ok()
}

fn run_curl_ftp(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
    remote_path: &str,
    head: bool,
) -> Result<std::process::Output, Error> {
    if username.contains(['\r', '\n']) || password.contains(['\r', '\n']) {
        anyhow::bail!("FTP credentials contain an invalid line break");
    }
    let executable = find_curl()?;
    let url = ftp_url(host, port, remote_path, !head);
    let mut command = Command::new(executable);
    command.args([
        "--config",
        "-",
        "--fail",
        "--silent",
        "--show-error",
        "--disable-epsv",
        "--connect-timeout",
        "5",
        "--max-time",
        "15",
    ]);
    if head {
        command.arg("--head");
    } else {
        command.arg("--list-only");
    }
    let mut child = command
        .arg("--url")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start curl for console FTP verification")?;
    let credentials = format!("{username}:{password}");
    let config = format!("user = \"{}\"\n", curl_config_value(&credentials));
    child
        .stdin
        .take()
        .context("could not securely pass the FTP credentials to curl")?
        .write_all(config.as_bytes())?;
    let output = child
        .wait_with_output()
        .context("curl could not complete console FTP verification")?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Console FTP verification failed: {}", error.trim());
    }
    Ok(output)
}

fn ftp_url(host: &str, port: u16, remote_path: &str, directory: bool) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let mut encoded = String::new();
    for byte in remote_path.trim().trim_start_matches('/').bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    if directory && !encoded.ends_with('/') {
        encoded.push('/');
    }
    format!("ftp://{host}:{port}/{encoded}")
}

fn curl_config_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_curl_content_length(headers: &[u8]) -> Option<u64> {
    String::from_utf8_lossy(headers).lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<u64>().ok())
            .flatten()
    })
}

fn obscure_rclone_password(executable: &Path, password: &str) -> Result<String, Error> {
    if password.contains(['\r', '\n']) {
        anyhow::bail!("FTP passwords cannot contain a line break");
    }
    let mut child = Command::new(executable)
        .args(["obscure", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start the bundled rclone password helper")?;
    child
        .stdin
        .take()
        .context("could not securely pass the FTP password to rclone")?
        .write_all(format!("{password}\n").as_bytes())?;
    let output = child
        .wait_with_output()
        .context("rclone could not prepare the FTP password")?;
    if !output.status.success() {
        anyhow::bail!(
            "rclone could not prepare the FTP password: {}",
            rclone_error(&output)
        );
    }
    let value = String::from_utf8(output.stdout)
        .context("rclone returned an invalid password value")?
        .trim()
        .to_owned();
    if value.is_empty() {
        anyhow::bail!("rclone returned an empty password value");
    }
    Ok(value)
}

fn rclone_remote(path: &str) -> String {
    format!("xbox:/{}", path.trim().trim_start_matches('/'))
}

fn canonical_background_drive(name: &str) -> String {
    let name = name.trim_matches('/');
    if name.eq_ignore_ascii_case("fHdd") {
        "Hdd1".into()
    } else if name
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("fUsb"))
    {
        name[1..].to_owned()
    } else {
        name.to_owned()
    }
}

fn background_ftp_path(path: &str, background: bool) -> String {
    let mut parts = path.trim_start_matches('/').splitn(2, '/');
    let drive = parts.next().unwrap_or_default();
    let tail = parts.next();
    let mapped = if background && drive.eq_ignore_ascii_case("Hdd1") {
        "fHdd".into()
    } else if background
        && drive
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Usb"))
    {
        format!("f{drive}")
    } else {
        drive.to_owned()
    };
    match tail {
        Some(tail) => format!("/{mapped}/{tail}"),
        None => format!("/{mapped}"),
    }
}

fn rclone_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if !stderr.is_empty() {
        stderr
    } else {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }
}

fn trim_utf8_tail(value: &mut String, maximum_bytes: usize) {
    if value.len() <= maximum_bytes {
        return;
    }
    let mut boundary = value.len() - maximum_bytes;
    while !value.is_char_boundary(boundary) {
        boundary += 1;
    }
    value.drain(..boundary);
}

#[derive(Debug)]
struct FtpConnectionLost;
impl std::fmt::Display for FtpConnectionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FTP connection lost")
    }
}
impl std::error::Error for FtpConnectionLost {}

fn run_rclone_with_progress(
    mut command: Command,
    label: &str,
    ftp: &RcloneFtp,
) -> Result<(), Error> {
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start the bundled rclone FTP transfer")?;
    let mut stderr = child
        .stderr
        .take()
        .context("could not read rclone progress")?;
    let progress = ProgressDisplay::new();
    progress.update(label, 0);
    // Read logs separately so stalled transfers cannot prevent route changes.
    let (sender, receiver) = std::sync::mpsc::sync_channel(32);
    let reader = std::thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    if sender.send(Ok(chunk[..count].to_vec())).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    let mut pending = Vec::new();
    let mut diagnostic = String::new();
    let mut last_probe = std::time::Instant::now();
    loop {
        let connection_lost = last_probe.elapsed() >= Duration::from_secs(5) && {
            last_probe = std::time::Instant::now();
            ftp.test_connection().is_err()
        };
        if connection_lost {
            let _ = child.kill();
            let _ = child.wait();
            drop(receiver);
            let _ = reader.join();
            return Err(FtpConnectionLost.into());
        }
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(chunk)) => pending.extend_from_slice(&chunk),
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Some(end) = pending
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'))
        {
            let line = String::from_utf8_lossy(&pending[..end]);
            if let Some(transfer) = rclone_progress_from_json(&line) {
                let detail = format_transfer_detail(&transfer);
                progress.update_with_detail(label, transfer.percent, Some(&detail));
            } else if let Some(percent) = percentage_in_text(&line) {
                progress.update(label, percent);
            }
            if !line.trim().is_empty() {
                diagnostic.push_str(line.trim());
                diagnostic.push('\n');
                trim_utf8_tail(&mut diagnostic, 32_768);
            }
            pending.drain(..=end);
        }
    }
    let _ = reader.join();
    if !pending.is_empty() {
        let line = String::from_utf8_lossy(&pending);
        if let Some(transfer) = rclone_progress_from_json(&line) {
            let detail = format_transfer_detail(&transfer);
            progress.update_with_detail(label, transfer.percent, Some(&detail));
        } else if let Some(percent) = percentage_in_text(&line) {
            progress.update(label, percent);
        }
        if !line.trim().is_empty() {
            diagnostic.push_str(line.trim());
        }
    }
    let status = child
        .wait()
        .context("could not finish the rclone FTP transfer")?;
    if !status.success() {
        anyhow::bail!("rclone FTP transfer failed: {}", diagnostic.trim());
    }
    progress.finish("Upload complete");
    Ok(())
}

#[derive(Debug, PartialEq)]
struct RcloneProgress {
    percent: u8,
    bytes_per_second: Option<f64>,
    eta_seconds: Option<f64>,
}

fn rclone_progress_from_json(line: &str) -> Option<RcloneProgress> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let stats = value.get("stats")?;
    let bytes = stats.get("bytes")?.as_u64()?;
    let total = stats.get("totalBytes")?.as_u64()?;
    if total == 0 {
        return None;
    }
    Some(RcloneProgress {
        percent: (bytes.saturating_mul(100) / total).min(100) as u8,
        bytes_per_second: stats
            .get("speed")
            .and_then(serde_json::Value::as_f64)
            .filter(|speed| *speed > 0.0),
        eta_seconds: stats
            .get("eta")
            .and_then(serde_json::Value::as_f64)
            .filter(|eta| *eta >= 0.0),
    })
}

fn format_transfer_detail(progress: &RcloneProgress) -> String {
    if progress
        .bytes_per_second
        .is_some_and(|speed| speed < 1024.0)
    {
        return "Waiting for transfer data | estimating time remaining".into();
    }

    let speed = progress
        .bytes_per_second
        .map(format_transfer_speed)
        .unwrap_or_else(|| "calculating speed".to_owned());
    let remaining = progress
        .eta_seconds
        .map(format_remaining_time)
        .unwrap_or_else(|| "calculating time remaining".to_owned());
    format!("{speed} | {remaining}")
}

fn format_transfer_speed(bytes_per_second: f64) -> String {
    if bytes_per_second >= 1024.0 * 1024.0 {
        format!("{:.1} MiB/s", bytes_per_second / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KiB/s", bytes_per_second / 1024.0)
    }
}

fn format_remaining_time(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    if seconds < 60 {
        format!("about {seconds} sec remaining")
    } else if seconds < 60 * 60 {
        format!("about {} min remaining", seconds.div_ceil(60))
    } else {
        let hours = seconds / (60 * 60);
        let minutes = (seconds % (60 * 60)).div_ceil(60);
        if minutes == 0 {
            format!("about {hours} hr remaining")
        } else {
            format!("about {hours} hr {minutes} min remaining")
        }
    }
}

fn percentage_in_text(text: &str) -> Option<u8> {
    let percent = text.rfind('%')?;
    let before = &text[..percent];
    let start = before
        .rfind(|character: char| !character.is_ascii_digit())
        .map_or(0, |index| index + 1);
    before[start..]
        .parse::<u8>()
        .ok()
        .filter(|value| *value <= 100)
}

fn ensure_empty_dir(path: &Path) -> Result<(), Error> {
    if fs::exists(path)? {
        fs::remove_dir_all(path)?;
    };
    fs::create_dir_all(path)?;
    Ok(())
}

fn read_part_mht(file_layout: &god::FileLayout, part_index: u64) -> Result<god::HashList, Error> {
    let part_file = file_layout.part_file_path(part_index);
    let mut part_file = File::options().read(true).open(part_file)?;
    god::HashList::read(&mut part_file)
}

fn write_part_mht(
    file_layout: &god::FileLayout,
    part_index: u64,
    mht: &god::HashList,
) -> Result<(), Error> {
    let part_file = file_layout.part_file_path(part_index);
    let mut part_file = File::options().write(true).open(part_file)?;
    mht.write(&mut part_file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    pub(crate) fn test_xex(title_id: u32) -> Vec<u8> {
        let mut bytes = vec![0_u8; 52];
        bytes[0..4].copy_from_slice(b"XEX2");
        bytes[20..24].copy_from_slice(&1_u32.to_be_bytes());
        bytes[24..28].copy_from_slice(&0x0004_0006_u32.to_be_bytes());
        bytes[28..32].copy_from_slice(&32_u32.to_be_bytes());
        bytes[32..36].copy_from_slice(&0x1122_3344_u32.to_be_bytes());
        bytes[36..40].copy_from_slice(&2_u32.to_be_bytes());
        bytes[40..44].copy_from_slice(&2_u32.to_be_bytes());
        bytes[44..48].copy_from_slice(&title_id.to_be_bytes());
        bytes[50] = 1;
        bytes[51] = 1;
        bytes
    }

    #[test]
    fn pc_menu_exposes_ps5_receive_exit_and_optional_update() {
        let options = main_menu_options(false);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].value, MainAction::ReceivePs5Dump);
        assert_eq!(options[1].value, MainAction::Exit);
        let deferred_options = main_menu_options(true);
        assert_eq!(deferred_options.len(), 3);
        assert_eq!(deferred_options[1].value, MainAction::UpdateNow);
        assert_eq!(deferred_options[1].label, "2. Update now");
        assert_eq!(deferred_options[2].value, MainAction::Exit);
    }

    #[test]
    fn exposes_provider_independent_ai_tools_without_credentials() {
        let tools = mcp_tool_definitions();
        assert_eq!(tools.len(), 6);
        assert_eq!(tools[0]["name"], "converter_status");
        assert_eq!(tools[1]["name"], "scan_game_folder");
        assert_eq!(tools[2]["name"], "inspect_game_input");
        let status = ai_converter_status().unwrap();
        assert_eq!(status["version"], "2.0.0");
        assert_eq!(status["passwordExposed"], false);
        assert!(status.get("password").is_none());
    }

    #[test]
    fn groups_part_rar_sets_and_reports_exact_missing_filename() {
        let root = env::temp_dir().join(format!(
            "iso2god-games-scan-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        for part in [1, 2, 3, 4, 6, 7] {
            fs::write(root.join(format!("FORGHORZIN.part{part}.rar")), b"part").unwrap();
        }
        let sets = scan_multipart_game_sets(&root).unwrap();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].game_name, "FORGHORZIN");
        assert_eq!(sets[0].parts.len(), 6);
        assert_eq!(
            sets[0].missing_part.as_deref(),
            Some("FORGHORZIN.part5.rar")
        );
        assert_eq!(sets[0].entry_path, root.join("FORGHORZIN.part1.rar"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removes_only_the_verified_archive_parts() {
        let root = env::temp_dir().join(format!(
            "iso2god-safe-cleanup-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let part_one = root.join("game.part1.rar");
        let part_two = root.join("game.part2.rar");
        let unrelated = root.join("keep-me.txt");
        fs::write(&part_one, b"one").unwrap();
        fs::write(&part_two, b"two").unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        remove_verified_archive_parts(&root, &[part_one.clone(), part_two.clone()]).unwrap();
        assert!(!part_one.exists());
        assert!(!part_two.exists());
        assert!(unrelated.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn imports_generated_split_7z_as_one_complete_jtag_game() {
        let Some(seven_zip) = find_7zip() else {
            return;
        };
        let root = env::temp_dir().join(format!(
            "iso2god-real-multipart-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source_game = root.join("source").join("Forza Horizon");
        let games = root.join("Games");
        fs::create_dir_all(&source_game).unwrap();
        fs::create_dir(&games).unwrap();
        fs::write(source_game.join("default.xex"), test_xex(0x4D53_0AB5)).unwrap();
        let payload = (0..65_536)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(source_game.join("game-data.bin"), payload).unwrap();
        let archive = games.join("FORGHORZIN.7z");
        let status = Command::new(&seven_zip)
            .current_dir(root.join("source"))
            .args(["a", "-t7z", "-mx=0", "-v16k"])
            .arg(&archive)
            .arg("Forza Horizon")
            .status()
            .unwrap();
        assert!(status.success());

        let sets = scan_multipart_game_sets(&games).unwrap();
        assert_eq!(sets.len(), 1);
        let set = &sets[0];
        assert!(set.parts.len() > 1);
        assert!(set.missing_part.is_none());
        assert!(list_archive_unpacked_size(&seven_zip, &set.entry_path).unwrap() > 0);

        let staging = games.join(".test-staging");
        fs::create_dir(&staging).unwrap();
        let progress = ProgressDisplay::new();
        let status = extract_with_7zip(&seven_zip, &set.entry_path, &staging, &progress).unwrap();
        assert!(status.success());
        verify_nonempty_directory(&staging).unwrap();
        let detected = detect_extracted_xbox_game(&staging).unwrap();
        let prepared = prepare_detected_game(&games, &set.game_name, detected).unwrap();
        verify_prepared_game(&prepared).unwrap();
        remove_verified_archive_parts(&games, &set.parts).unwrap();
        assert!(prepared.root.join("default.xex").is_file());
        assert!(set.parts.iter().all(|part| !part.exists()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_jtag_destination_uses_aurora_content_root_and_title_id() {
        let game = PreparedGame {
            root: PathBuf::from("prepared-game"),
            name: "Display name is not used as the install key".to_owned(),
            kind: PreparedGameKind::Jtag {
                title_id: "4D530AB5".to_owned(),
            },
        };

        let (source, destination) = prepared_source_and_relative_destination(&game);
        assert_eq!(source, Path::new("prepared-game"));
        assert_eq!(
            destination,
            PathBuf::from("Content")
                .join("0000000000000000")
                .join("4D530AB5")
        );
    }

    #[test]
    fn xbox_destination_menu_uses_real_storage_and_all_connected_usb_names() {
        let hdd = storage_drive_from_remote_name("Hdd1").unwrap();
        assert_eq!(hdd.remote_name, "Hdd1");
        assert!(!hdd.external);

        for name in ["Usb0", "Usb1", "Usb7", "UsbMU2"] {
            let drive = storage_drive_from_remote_name(name).unwrap();
            assert_eq!(drive.remote_name, name);
            assert!(drive.external);
        }

        for system_partition in ["HddX", "System", "SysExt", "Flash", "Dvd"] {
            assert!(storage_drive_from_remote_name(system_partition).is_none());
        }
    }

    #[test]
    fn parses_latest_7zip_percentage() {
        assert_eq!(last_percentage(b" 12% file.iso\r 87% file.iso"), Some(87));
        assert_eq!(last_percentage(b"no progress yet"), None);
    }

    #[test]
    fn parses_rclone_transfer_percentage() {
        assert_eq!(
            percentage_in_text("Transferred: 2 GiB / 4 GiB, 50%"),
            Some(50)
        );
        assert_eq!(percentage_in_text("no progress yet"), None);
        assert_eq!(percentage_in_text("100%"), Some(100));
    }

    #[test]
    fn parses_rclone_json_byte_progress() {
        let line =
            r#"{"stats":{"bytes":768909824,"totalBytes":3750146048,"speed":3323985.2,"eta":898}}"#;
        assert_eq!(
            rclone_progress_from_json(line).map(|progress| progress.percent),
            Some(20)
        );
        let progress = rclone_progress_from_json(line).unwrap();
        assert_eq!(progress.percent, 20);
        assert_eq!(
            format_transfer_detail(&progress),
            "3.2 MiB/s | about 15 min remaining"
        );
        assert_eq!(
            rclone_progress_from_json(r#"{"stats":{"bytes":1,"totalBytes":0}}"#)
                .map(|progress| progress.percent),
            None
        );
    }

    #[test]
    fn formats_professional_transfer_estimates() {
        assert_eq!(format_remaining_time(42.0), "about 42 sec remaining");
        assert_eq!(format_remaining_time(121.0), "about 3 min remaining");
        assert_eq!(format_remaining_time(3_661.0), "about 1 hr 2 min remaining");
    }

    #[test]
    fn estimates_remaining_time_for_extraction_and_conversion() {
        let remaining = estimate_progress_remaining(600.0, 5, 79).unwrap();
        assert!((remaining - 170.270_270_270_270_26).abs() < 0.001);
        assert_eq!(format_remaining_time(remaining), "about 3 min remaining");
        assert_eq!(estimate_progress_remaining(0.5, 0, 25), None);
        assert_eq!(estimate_progress_remaining(30.0, 10, 10), None);
        assert_eq!(estimate_progress_remaining(30.0, 10, 100), None);
    }

    #[test]
    fn accepts_a_complete_dropped_file_without_enter() {
        let file = env::current_exe().unwrap();
        assert_eq!(
            complete_dropped_path(file.to_str().unwrap(), Some(false)),
            Some(file.clone())
        );
        assert_eq!(
            complete_dropped_path(&format!("\"{}\"", file.display()), Some(false)),
            Some(file.clone())
        );
        assert_eq!(
            complete_dropped_path("\"unfinished path", Some(false)),
            None
        );

        let folder = file.parent().unwrap().to_path_buf();
        assert_eq!(
            complete_dropped_path(&format!("\"{}\"", folder.display()), None),
            Some(folder)
        );
        assert_eq!(
            complete_dropped_path(file.to_str().unwrap(), None),
            Some(file)
        );
    }

    #[test]
    fn accepts_a_single_archive_in_multipart_import() {
        let root = env::temp_dir().join(format!(
            "iso2god-single-archive-drop-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let archive = root.join("Gta San God.zip");
        fs::write(&archive, b"test archive placeholder").unwrap();
        let (folder, sets, remove_after_success) =
            multipart_sets_for_dropped_input(&archive).unwrap();
        assert_eq!(folder, root);
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].entry_path, archive);
        assert!(!remove_after_success);
        fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn detects_a_multipart_rar_nested_inside_an_outer_archive() {
        let root = env::temp_dir().join(format!(
            "iso2god-nested-archive-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = root.join("Gta San God");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("gta.san.god.todoinmega.part1.rar"), b"part one").unwrap();
        fs::write(nested.join("gta.san.god.todoinmega.part2.rar"), b"part two").unwrap();
        fs::write(
            nested.join("gta.san.god.todoinmega.part1.rev"),
            b"recovery data",
        )
        .unwrap();

        let sets = scan_archive_sets(&root).unwrap();
        assert_eq!(sets.len(), 1);
        let set = &sets[0];
        assert_eq!(set.parts.len(), 2);
        assert!(set.entry_path.ends_with("gta.san.god.todoinmega.part1.rar"));
        assert!(set.missing_part.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scans_all_folder_branches_without_mixing_same_named_sets() {
        let mut temp = TempDir::new().unwrap();
        temp.cleanup = true;
        let root = temp.path();
        let deep = root.join("one/two/three/four/five/six");
        let other = root.join("other");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(&other).unwrap();
        for name in ["game.part1.rar", "game.part2.rar", "single.zip"] {
            fs::write(deep.join(name), b"placeholder").unwrap();
        }
        for name in ["game.part1.rar", "game.part3.rar", "single.7z"] {
            fs::write(other.join(name), b"placeholder").unwrap();
        }
        let (_, sets, _) = multipart_sets_for_dropped_input(root).unwrap();
        assert_eq!(sets.len(), 4);
        let complete = sets
            .iter()
            .find(|set| set.entry_path == deep.join("game.part1.rar"))
            .unwrap();
        assert_eq!(complete.parts.len(), 2);
        assert!(complete.missing_part.is_none());
        let incomplete = sets
            .iter()
            .find(|set| set.entry_path == other.join("game.part1.rar"))
            .unwrap();
        assert_eq!(incomplete.missing_part.as_deref(), Some("game.part2.rar"));
    }

    #[test]
    fn groups_legacy_volumes_once_and_includes_all_parts() {
        let mut temp = TempDir::new().unwrap();
        temp.cleanup = true;
        let root = temp.path();
        for name in [
            "game.rar",
            "game.r00",
            "game.r01",
            "other.zip",
            "other.z01",
            "other.z02",
        ] {
            fs::write(root.join(name), b"placeholder").unwrap();
        }
        let sets = scan_archive_sets(root).unwrap();
        assert_eq!(sets.len(), 2);
        assert!(sets.iter().all(|set| set.parts.len() == 3));
        let (_, dropped, _) = multipart_sets_for_dropped_input(&root.join("game.r01")).unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].entry_path, root.join("game.rar"));
    }

    #[test]
    fn archive_errors_include_both_output_streams_or_a_fallback() {
        let text = archive_diagnostics(b"Headers Error", b"Data Error");
        assert!(text.contains("Headers Error"));
        assert!(text.contains("Data Error"));
        assert_eq!(
            archive_diagnostics(b"", b""),
            "7-Zip returned no diagnostic text."
        );
    }

    #[test]
    fn archive_errors_collapse_repeated_unsupported_method_lines() {
        let text = archive_diagnostics(
            b"",
            b"ERROR: Unsupported Method : vinyls\\u_1.carbin\nERROR: Unsupported Method : vinyls\\u_2.carbin\nERROR: Unsupported Method : vinyls\\u_3.carbin",
        );
        assert!(text.contains("3 affected file(s)"));
        assert!(text.contains("vinyls\\u_1.carbin"));
        assert!(!text.contains("vinyls\\u_2.carbin"));
    }

    #[test]
    fn progress_track_uses_solid_blocks_without_fuzzy_fill() {
        let bar = smooth_progress_bar(50, 20);
        assert!(bar.contains('█'));
        assert!(!bar.contains('░'));
        assert!(!bar.contains('▏'));
        assert!(!bar.contains('#'));
    }

    #[test]
    fn dashlaunch_plugin_configuration_uses_first_two_free_slots() {
        let configured = configure_dashlaunch_plugin(
            "[Paths]\r\ndefault = Hdd:\\Aurora\\Aurora.xex\r\n\r\n[Plugins]\r\nplugin1 = Hdd:\\xbdm.xex\r\nplugin2 =\r\n",
        )
        .unwrap();
        assert!(configured.contains(&format!("plugin2 = {DASHLAUNCH_FTP_PATH}\r\n")));
        assert!(!configured.contains(DASHLAUNCH_GUIDE_PATH));
        assert!(configured.contains("plugin1 = Hdd:\\xbdm.xex"));
    }

    #[test]
    fn dashlaunch_plugin_configuration_creates_section_and_is_idempotent() {
        let configured = configure_dashlaunch_plugin("").unwrap();
        assert_eq!(
            configured,
            format!("[Plugins]\nplugin1 = {DASHLAUNCH_FTP_PATH}\n")
        );
        assert_eq!(
            configure_dashlaunch_plugin(&configured).unwrap(),
            configured
        );
    }

    #[test]
    fn dashlaunch_plugin_configuration_migrates_the_obsolete_path() {
        let configured = configure_dashlaunch_plugin(&format!(
            "[Plugins]\nplugin1 = {LEGACY_DASHLAUNCH_GUIDE_PATH}\nplugin2 = Hdd:\\xbdm.xex\n"
        ))
        .unwrap();
        assert!(!configured.contains(LEGACY_DASHLAUNCH_GUIDE_PATH));
        assert!(configured.contains(DASHLAUNCH_FTP_PATH));
        assert!(!configured.contains(DASHLAUNCH_GUIDE_PATH));
        assert!(configured.contains("plugin2 = Hdd:\\xbdm.xex"));
    }

    #[test]
    fn dashlaunch_plugin_configuration_does_not_replace_occupied_slots() {
        let full = "[Plugins]\nplugin1=a.xex\nplugin2=b.xex\nplugin3=c.xex\nplugin4=d.xex\nplugin5=e.xex\n";
        assert!(
            configure_dashlaunch_plugin(full)
                .unwrap_err()
                .to_string()
                .contains("free plugin slot")
        );
    }

    #[test]
    fn background_ftp_uses_same_physical_drive_with_its_own_mount_names() {
        assert_eq!(
            background_ftp_path("/Hdd1/Content/0000000000000000/ABC/file", true),
            "/fHdd/Content/0000000000000000/ABC/file"
        );
        assert_eq!(
            background_ftp_path("/Usb2/Content/file", true),
            "/fUsb2/Content/file"
        );
        assert_eq!(
            background_ftp_path("/Hdd1/Content/file", false),
            "/Hdd1/Content/file"
        );
        assert_eq!(canonical_background_drive("fHdd/"), "Hdd1");
        assert_eq!(canonical_background_drive("fUsb2/"), "Usb2");
        assert!(storage_drive_from_remote_name(&canonical_background_drive("fFlash/")).is_none());
    }

    #[test]
    fn extracted_jtag_game_wins_over_internal_zip_assets() {
        let root = env::temp_dir().join(format!(
            "iso2god-internal-archive-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let game = root.join("Forza Horizon 2 Fast and Furious");
        fs::create_dir_all(game.join("media")).unwrap();
        fs::write(game.join("default.xex"), b"xex").unwrap();
        fs::write(game.join("media").join("Livery.zip"), b"game asset").unwrap();

        let detected = detect_xbox_game_with_nested_archives(
            &root,
            Path::new("extractor-must-not-be-started"),
        )
        .unwrap();
        assert!(matches!(detected, DetectedXboxGame::Jtag(path) if path == game));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trims_whitespace_from_ftp_addresses() {
        assert_eq!(
            normalise_ftp_host("   192.0.2.65   ").unwrap(),
            "192.0.2.65"
        );
        assert_eq!(
            normalise_ftp_host("  ftp://xbox.local/  ").unwrap(),
            "xbox.local"
        );
        assert_eq!(normalise_ftp_host("IP: 192.0.2.65").unwrap(), "192.0.2.65");
        assert_eq!(
            normalise_ftp_host("Host/IP Address: 192.0.2.65").unwrap(),
            "192.0.2.65"
        );
    }

    #[test]
    fn ftp_failover_tries_the_other_service_then_retries_the_current_one() {
        let ftp = RcloneFtp {
            executable: PathBuf::from("unused-rclone"),
            _embedded_tool: None,
            environment: Vec::new(),
            host: "192.0.2.65".to_owned(),
            primary_port: DEFAULT_FTP_PORT,
            port: DEFAULT_FTP_PORT,
            background_port: Some(DEFAULT_BACKGROUND_FTP_PORT),
            username: DEFAULT_FTP_USERNAME.to_owned(),
            password: DEFAULT_FTP_PASSWORD.to_owned(),
        };
        assert_eq!(
            ftp.failover_ports(DEFAULT_FTP_PORT),
            vec![DEFAULT_BACKGROUND_FTP_PORT, DEFAULT_FTP_PORT]
        );
        assert_eq!(
            ftp.failover_ports(DEFAULT_BACKGROUND_FTP_PORT),
            vec![DEFAULT_FTP_PORT, DEFAULT_BACKGROUND_FTP_PORT]
        );
    }

    #[test]
    fn ftp_diagnostic_tail_never_splits_a_utf8_character() {
        let mut diagnostic = format!("{}•{}", "x".repeat(32_767), "y".repeat(100));
        trim_utf8_tail(&mut diagnostic, 32_768);
        assert!(diagnostic.len() <= 32_768);
        assert!(diagnostic.is_char_boundary(0));
        assert!(diagnostic.ends_with(&"y".repeat(100)));
    }

    #[cfg(windows)]
    #[test]
    fn parses_powershell_removable_drives() {
        let drives =
            parse_windows_removable_drives("E:\\|XBOX USB|34359738368\r\nG:\\||1073741824\r\n");
        assert_eq!(drives.len(), 2);
        assert_eq!(drives[0].root, PathBuf::from("E:\\"));
        assert_eq!(drives[0].label, "XBOX USB (E:)");
        assert_eq!(drives[0].free_bytes, 34_359_738_368);
        assert_eq!(drives[1].label, "Removable drive (G:)");
    }

    #[test]
    fn parses_linux_usb_drives_and_inherits_parent_transport() {
        let json = br#"{
            "blockdevices": [{
                "name": "sdb", "tran": "usb", "rm": false,
                "mountpoints": [null], "label": null, "fsavail": null,
                "children": [{
                    "name": "sdb1", "tran": null, "rm": false,
                    "mountpoints": ["/media/riley/XBOX"],
                    "label": "XBOX", "fsavail": 34359738368
                }]
            }]
        }"#;
        let drives = parse_linux_removable_drives(json).unwrap();
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].root, PathBuf::from("/media/riley/XBOX"));
        assert_eq!(drives[0].label, "XBOX (/media/riley/XBOX)");
        assert_eq!(drives[0].free_bytes, 34_359_738_368);
    }

    #[test]
    fn detects_complete_and_incomplete_multipart_archives() {
        let root = env::temp_dir().join(format!(
            "iso2god-multipart-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        for part in ["001", "002", "003"] {
            fs::write(root.join(format!("game.7z.{part}")), b"part").unwrap();
        }
        let archive = detect_archive_set(&root.join("game.7z.002")).unwrap();
        assert!(archive.multipart);
        assert_eq!(archive.part_count, 3);
        assert_eq!(archive.entry_path, root.join("game.7z.001"));

        fs::remove_file(root.join("game.7z.002")).unwrap();
        let error = detect_archive_set(&root.join("game.7z.001")).unwrap_err();
        assert!(error.to_string().contains("missing part 2"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builds_absolute_rclone_remote_paths() {
        assert_eq!(rclone_remote("/Hdd1/Games/Test"), "xbox:/Hdd1/Games/Test");
        assert_eq!(rclone_remote("/"), "xbox:/");
    }

    #[test]
    fn embedded_rclone_is_self_contained_and_temporarily_extracted() {
        let tool = extract_embedded_rclone().unwrap();
        let directory = tool.directory.clone();
        let output = Command::new(&tool.executable)
            .arg("version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("rclone v1.75.0"));
        drop(tool);
        assert!(!directory.exists());
    }

    #[test]
    fn builds_encoded_ftp_urls_and_parses_remote_sizes() {
        assert_eq!(
            ftp_url("192.0.2.65", 21, "/Hdd1/Games/Test Game/file.xex", false),
            "ftp://192.0.2.65:21/Hdd1/Games/Test%20Game/file.xex"
        );
        assert_eq!(
            parse_curl_content_length(b"Content-Length: 1184\r\n"),
            Some(1184)
        );
    }

    #[test]
    fn bundled_rclone_connects_copies_and_verifies_against_local_ftp() {
        if env::var_os("ISO2GOD_RCLONE_INTEGRATION").is_none() {
            return;
        }
        let root = env::temp_dir().join(format!(
            "iso2god-rclone-integration-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let server_root = root.join("server");
        let source = root.join("source");
        fs::create_dir_all(&server_root).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("default.xex"), b"verified test game").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let dashboard_port = listener.local_addr().unwrap().port();
        drop(listener);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let background_port = listener.local_addr().unwrap().port();
        drop(listener);

        let settings = FtpSettings {
            host: "127.0.0.1".to_owned(),
            port: dashboard_port,
            background_port: Some(background_port),
            username: "xbox".to_owned(),
            password: "integration-test".to_owned(),
            destination_path: "/Hdd1/Content/0000000000000000/".to_owned(),
        };
        let ftp = RcloneFtp::new(&settings).unwrap();
        assert!(
            ftp.test_connection().is_err(),
            "the first connection should fail while the Xbox test server is off"
        );
        let mut server = Command::new(&ftp.executable)
            .args(["serve", "ftp"])
            .arg(&server_root)
            .args([
                "--addr",
                &format!("127.0.0.1:{background_port}"),
                "--user",
                "xbox",
                "--pass",
                "integration-test",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut connected = false;
        for _ in 0..30 {
            if ftp.connect_available().is_ok() {
                connected = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(connected, "the local rclone FTP server did not start");
        let ftp = ftp.connect_available().unwrap();
        assert_eq!(ftp.port, background_port);
        assert!(
            ftp.verify_directory(&source, "/Hdd1/Games/Test").is_err(),
            "a missing Xbox destination must not pass verification"
        );
        let active_port = ftp.copy_directory(&source, "/Hdd1/Games/Test").unwrap();
        let uploaded = server_root.join("Hdd1/Games/Test/default.xex");
        ftp.run_checked([
            "deletefile",
            rclone_remote("/Hdd1/Games/Test/default.xex").as_str(),
        ])
        .unwrap();
        assert!(!uploaded.exists());
        verify_and_repair_ftp(&ftp, &source, "/Hdd1/Games/Test", active_port).unwrap();
        assert_eq!(fs::read(uploaded).unwrap(), b"verified test game");
        server.kill().unwrap();
        server.wait().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
