//! Per-user PC companion for the Xbox UI. No VM or shared developer service.
use anyhow::{Context, Error};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, UdpSocket};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PORT: u16 = 36062;
const XEX: &[u8] = include_bytes!("../../consoles/Xbox/ISO2GOD/vendor/xbox/digital360.xex");
const INSTALL_DIR: &str = "/Hdd1/Content/0000000000000000/ISO2GOD";

fn token(path: &Path) -> Result<String, Error> {
    if path.exists() {
        let value = fs::read_to_string(path)?;
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("Digital 360 pairing token is invalid.");
        }
        return Ok(value);
    }
    let mut bytes = [0_u8; 32];
    #[cfg(windows)]
    {
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            fn BCryptGenRandom(
                algorithm: *mut std::ffi::c_void,
                buffer: *mut u8,
                len: u32,
                flags: u32,
            ) -> i32;
        }
        if unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), 32, 2) } != 0 {
            anyhow::bail!("Windows could not generate a secure pairing token.");
        }
    }
    #[cfg(not(windows))]
    {
        use std::io::Read;
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    }
    let value: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(value.as_bytes())?;
    Ok(value)
}

fn ready(host: Ipv4Addr, token: &str) -> bool {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build()
        .get(&format!("http://{host}:{PORT}/v1/host"))
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .is_ok()
}

pub fn start_and_pair() -> Result<(), Error> {
    let settings = crate::saved_ftp_settings_with_password()?;
    let xbox: Ipv4Addr = settings
        .host
        .parse()
        .context("Use the Xbox's LAN IPv4 address for Digital 360 pairing.")?;
    let route = UdpSocket::bind("0.0.0.0:0")?;
    route.connect((xbox, settings.port))?;
    let std::net::IpAddr::V4(host) = route.local_addr()?.ip() else {
        anyhow::bail!("An IPv4 connection to the Xbox is required.");
    };
    let root = crate::app_settings::config_dir()?;
    fs::create_dir_all(&root)?;
    let token = token(&root.join("digital360-token.txt"))?;
    if !ready(host, &token) {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("digital360-host.log"))?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--store-worker-service")
            .env("ISO2GOD_WORKER_BIND", format!("{host}:{PORT}"))
            .env("ISO2GOD_WORKER_TOKEN", &token)
            .env("DIGITAL360_PC_FTP_SETTINGS", crate::ftp_settings_path()?)
            .env("DIGITAL360_ALLOWED_XBOX", xbox.to_string())
            .env_remove("ISO2GOD_WORKER_CONSOLE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000 | 0x00000008);
        }
        let mut child = command
            .spawn()
            .context("Could not start the Digital 360 background service.")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !ready(host, &token) {
            if child.try_wait()?.is_some() || Instant::now() >= deadline {
                anyhow::bail!(
                    "Digital 360 could not start on this PC. Check digital360-host.log and allow TCP {PORT} from your Xbox through the PC firewall."
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    let ftp = crate::RcloneFtp::new(&settings)?.connect_available()?;
    ftp.create_directory(INSTALL_DIR)?;
    let mut temporary = crate::TempDir::new()?;
    let remote_xex = format!("{INSTALL_DIR}/default.xex");
    let existing = match ftp.read_file(&remote_xex) {
        Ok(bytes) => Some(bytes),
        Err(error) if crate::remote_missing_error(&error) => None,
        Err(error) => return Err(error),
    };
    if existing.as_deref() != Some(XEX) {
        if let Some(existing) = existing {
            fs::write(root.join("digital360-previous.xex"), existing)?;
        }
        let xex = temporary.path().join("default.xex");
        fs::write(&xex, XEX)?;
        ftp.upload_installer_file(&xex, &remote_xex)?;
        crate::verify_remote_file(&ftp, &remote_xex, XEX)?;
    }
    let pairing = format!("host={host}\nport={PORT}\ntoken={token}\n");
    let ini = temporary.path().join("worker.ini");
    fs::write(&ini, &pairing)?;
    let remote_ini = format!("{INSTALL_DIR}/worker.ini");
    ftp.upload_installer_file(&ini, &remote_ini)?;
    crate::verify_remote_file(&ftp, &remote_ini, pairing.as_bytes())?;
    temporary.cleanup = true;
    println!(
        "Digital 360 is paired with this PC. Open it on your Xbox; scan Aurora on first install."
    );
    println!("Allow TCP {PORT} from your Xbox through your PC firewall if prompted.");
    println!(
        "You can close this menu. Keep this PC awake; reopen this app after restarting the PC."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pairing_token_is_random_and_persistent() {
        let root = crate::TempDir::new().unwrap();
        let first = token(&root.path().join("one")).unwrap();
        assert_eq!(first, token(&root.path().join("one")).unwrap());
        assert_ne!(first, token(&root.path().join("two")).unwrap());
        assert_eq!(first.len(), 64);
    }
}
