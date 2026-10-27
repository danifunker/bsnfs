//! `bsnfs service install|uninstall`: run bsnfs as a system service that
//! starts at boot, using each platform's own service manager: systemd on
//! Linux, launchd on macOS, the Service Control Manager on Windows.
//!
//! The service reads a config file (see [`crate::config`]) rather than a long
//! command line. `install` writes a starter config if there is none, and adds
//! any folders given to it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config;

pub const NAME: &str = "bsnfs";
/// launchd label on macOS.
pub const LABEL: &str = "io.github.danifunker.bsnfs";

fn usage() -> String {
    format!(
        "\
Usage: bsnfs service install [--user NAME] [--config FILE] [[NAME=]DIR]...
       bsnfs service uninstall

Install bsnfs as a service that starts at boot ({manager}), or remove it.
Run as {admin}.

  --user NAME      account the service runs as; its permissions decide what
                   clients can read and write. {user_default}
  --password PW    the account's password (Windows only)
  --config FILE    config file (default: {config})
  [NAME=]DIR       folders to add to the config file

The config file is created on first install. Edit it, then restart the
service: {restart}
",
        manager = MANAGER,
        admin = if cfg!(windows) { "Administrator" } else { "root (sudo)" },
        user_default = if cfg!(windows) {
            "Default: LocalSystem, which can\n                   read and write everything."
        } else {
            "Default: the user who ran sudo."
        },
        config = default_config().display(),
        restart = RESTART,
    )
}

#[cfg(target_os = "linux")]
const MANAGER: &str = "systemd";
#[cfg(target_os = "macos")]
const MANAGER: &str = "launchd";
#[cfg(windows)]
const MANAGER: &str = "the Service Control Manager";
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
const MANAGER: &str = "not supported on this platform";

#[cfg(target_os = "linux")]
const RESTART: &str = "sudo systemctl restart bsnfs";
#[cfg(target_os = "macos")]
const RESTART: &str = "sudo launchctl kickstart -k system/io.github.danifunker.bsnfs";
#[cfg(windows)]
const RESTART: &str = "Restart-Service bsnfs";
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
const RESTART: &str = "(unsupported)";

/// Where the service's config file lives by default.
pub fn default_config() -> PathBuf {
    data_dir().join("bsnfs.conf")
}

/// Directory for the config file and, except on Linux, state and logs.
fn data_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/bsnfs")
    } else if cfg!(windows) {
        std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData")).join("bsnfs")
    } else {
        PathBuf::from("/etc/bsnfs")
    }
}

/// Directory the service may write to (the exports file).
fn state_dir() -> PathBuf {
    if cfg!(target_os = "linux") { PathBuf::from("/var/lib/bsnfs") } else { data_dir() }
}

struct Install {
    user: Option<String>,
    #[cfg_attr(not(windows), allow(dead_code))]
    password: Option<String>,
    config: PathBuf,
    shares: Vec<String>,
}

pub fn main(args: &[OsString]) -> i32 {
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    let result = match args.first().map(String::as_str) {
        Some("install") => parse_install(&args[1..]).and_then(|o| install(&o)),
        Some("uninstall") => uninstall(),
        // Started by the Windows Service Control Manager, never by hand.
        Some("run") => run(&args[1..]),
        // For packaging: the config file a fresh install starts with.
        Some("starter-config") => {
            print!("{}", starter_config(&[]));
            return 0;
        }
        Some("-h" | "--help") | None => {
            print!("{}", usage());
            return 0;
        }
        Some(other) => Err(format!("unknown service command {other:?}\n\n{}", usage())),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("bsnfs service: {e}");
            1
        }
    }
}

fn parse_install(args: &[String]) -> Result<Install, String> {
    let mut o = Install { user: None, password: None, config: default_config(), shares: Vec::new() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--user" => o.user = Some(value()?),
            "--password" => o.password = Some(value()?),
            "--config" | "-C" => o.config = PathBuf::from(value()?),
            "-h" | "--help" => return Err(usage()),
            f if f.starts_with('-') => return Err(format!("unknown option {f}")),
            share => o.shares.push(share.to_string()),
        }
    }
    if o.user.is_none() && cfg!(unix) {
        o.user = std::env::var("SUDO_USER").ok().filter(|u| !u.is_empty() && u != "root");
    }
    Ok(o)
}

/// The starter config file.
fn starter_config(shares: &[String]) -> String {
    let state = state_dir();
    let mut text = format!(
        "\
# bsnfs service configuration.
# One option per line, named like bsnfs's long options without the dashes
# (`bsnfs --help` lists them). Restart the service after editing:
#   {RESTART}

# The admin web page. Use 0.0.0.0:8080 to reach it from other machines,
# and set a password when you do.
admin 127.0.0.1:8080
# admin-password change-me

# Shares added on the admin page are saved here.
exports-file {}
",
        state.join("exports").display()
    );
    if cfg!(windows) {
        text.push_str(&format!("\n# Services have no console, so log to a file.\nlog-file {}\n", data_dir().join("bsnfs.log").display()));
    }
    text.push_str("\n# Folders to share, one per line: share [NAME=]DIR\n");
    if shares.is_empty() {
        text.push_str("# share irix=/path/to/folder\n");
    }
    for s in shares {
        text.push_str(&format!("share {s}\n"));
    }
    text
}

/// `[NAME=]DIR` with DIR made absolute, since the service runs elsewhere.
fn absolute_share(spec: &str) -> Result<String, String> {
    let e = config::export_spec(spec)?;
    let dir = std::fs::canonicalize(&e.path).map_err(|err| format!("{}: {err}", e.path.display()))?;
    Ok(format!("{}={}", e.name.trim_start_matches('/'), dir.display()))
}

/// Create the config file, or add `shares` to an existing one. Then check
/// the service will be able to start with it.
fn write_config(o: &Install) -> Result<(), String> {
    let shares = o.shares.iter().map(|s| absolute_share(s)).collect::<Result<Vec<_>, _>>()?;
    let dir = o.config.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::create_dir_all(state_dir()).map_err(|e| format!("{}: {e}", state_dir().display()))?;
    if o.config.exists() {
        println!("Keeping the existing config {}", o.config.display());
        if !shares.is_empty() {
            let mut text = std::fs::read_to_string(&o.config).map_err(|e| e.to_string())?;
            if !text.ends_with('\n') {
                text.push('\n');
            }
            for s in &shares {
                text.push_str(&format!("share {s}\n"));
            }
            std::fs::write(&o.config, text).map_err(|e| format!("{}: {e}", o.config.display()))?;
            println!("  added {} share(s)", shares.len());
        }
    } else {
        std::fs::write(&o.config, starter_config(&shares)).map_err(|e| format!("{}: {e}", o.config.display()))?;
        println!("Wrote {}", o.config.display());
    }
    // Catch mistakes now rather than in a service log.
    config::parse([OsString::from("--config"), o.config.clone().into_os_string()])
        .map_err(|e| format!("the config doesn't work yet: {e}"))?;
    Ok(())
}

fn exe() -> Result<PathBuf, String> {
    let p = std::env::current_exe().map_err(|e| format!("can't find this program's path: {e}"))?;
    std::fs::canonicalize(&p).map_err(|e| format!("{}: {e}", p.display()))
}

#[cfg(unix)]
fn command(cmd: &str, args: &[&str]) -> Result<(), String> {
    println!("  $ {cmd} {}", args.join(" "));
    let status = std::process::Command::new(cmd).args(args).status().map_err(|e| format!("{cmd}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{cmd} {} failed ({status})", args.join(" "))) }
}

#[cfg(unix)]
fn require_root() -> Result<(), String> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        return Err("run this with sudo".into());
    }
    Ok(())
}

/// uid and gid of a local account.
#[cfg(unix)]
fn lookup_user(name: &str) -> Result<(u32, u32), String> {
    let c = std::ffi::CString::new(name).map_err(|_| "bad user name".to_string())?;
    // SAFETY: getpwnam returns null or a pointer to a static passwd record,
    // which we read immediately.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    if pw.is_null() {
        return Err(format!("no such user {name:?}"));
    }
    // SAFETY: checked non-null above.
    let pw = unsafe { &*pw };
    Ok((pw.pw_uid, pw.pw_gid))
}

#[cfg(unix)]
fn require_user(o: &Install) -> Result<(String, u32, u32), String> {
    let user = o.user.clone().ok_or("pass --user NAME: the account whose permissions the service uses")?;
    let (uid, gid) = lookup_user(&user)?;
    Ok((user, uid, gid))
}

/// Let the service user write its state, and read (but not change) the config.
#[cfg(unix)]
fn hand_over(o: &Install, uid: u32, gid: u32) -> Result<(), String> {
    use std::os::unix::fs::{PermissionsExt, chown};
    chown(state_dir(), Some(uid), Some(gid)).map_err(|e| format!("{}: {e}", state_dir().display()))?;
    let exports = state_dir().join("exports");
    if exports.exists() {
        chown(&exports, Some(uid), Some(gid)).map_err(|e| format!("{}: {e}", exports.display()))?;
    }
    // The config may hold the admin password: root-owned, group-readable.
    chown(&o.config, Some(0), Some(gid)).map_err(|e| format!("{}: {e}", o.config.display()))?;
    std::fs::set_permissions(&o.config, std::fs::Permissions::from_mode(0o640)).map_err(|e| e.to_string())?;
    Ok(())
}

// ── Linux: systemd ──────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
const UNIT: &str = "/etc/systemd/system/bsnfs.service";

#[cfg(target_os = "linux")]
pub fn systemd_unit(exe: &Path, config: &Path, user: &str) -> String {
    format!(
        "\
[Unit]
Description=bsnfs NFS server
After=network-online.target
Wants=network-online.target

[Service]
User={user}
ExecStart=\"{}\" --config \"{}\"
# Lets the service user listen on the portmapper's port 111.
AmbientCapabilities=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
",
        exe.display(),
        config.display()
    )
}

#[cfg(target_os = "linux")]
fn install(o: &Install) -> Result<(), String> {
    require_root()?;
    let (user, uid, gid) = require_user(o)?;
    write_config(o)?;
    hand_over(o, uid, gid)?;
    std::fs::write(UNIT, systemd_unit(&exe()?, &o.config, &user)).map_err(|e| format!("{UNIT}: {e}"))?;
    println!("Wrote {UNIT} (runs as {user})");
    command("systemctl", &["daemon-reload"])?;
    command("systemctl", &["enable", "--now", NAME])?;
    command("systemctl", &["restart", NAME])?;
    println!("\nbsnfs is running and starts at boot.");
    println!("  status:  systemctl status bsnfs");
    println!("  log:     journalctl -u bsnfs -f");
    println!("  config:  {} (then: {RESTART})", o.config.display());
    println!("If a firewall is on, allow TCP+UDP 111 and 2049 (e.g. sudo ufw allow 2049 && sudo ufw allow 111).");
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall() -> Result<(), String> {
    require_root()?;
    let _ = command("systemctl", &["disable", "--now", NAME]);
    if Path::new(UNIT).exists() {
        std::fs::remove_file(UNIT).map_err(|e| format!("{UNIT}: {e}"))?;
        println!("Removed {UNIT}");
    }
    command("systemctl", &["daemon-reload"])?;
    println!("Kept {} and {}; delete them if you no longer need them.", default_config().display(), state_dir().display());
    Ok(())
}

// ── macOS: launchd ──────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
const PLIST: &str = "/Library/LaunchDaemons/io.github.danifunker.bsnfs.plist";
#[cfg(target_os = "macos")]
const LOG: &str = "/Library/Logs/bsnfs.log";

#[cfg(target_os = "macos")]
pub fn launchd_plist(exe: &Path, config: &Path, user: &str) -> String {
    let x = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>--config</string>
        <string>{}</string>
    </array>
    <key>UserName</key>
    <string>{}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{LOG}</string>
    <key>StandardErrorPath</key>
    <string>{LOG}</string>
</dict>
</plist>
"#,
        x(&exe.display().to_string()),
        x(&config.display().to_string()),
        x(user)
    )
}

#[cfg(target_os = "macos")]
fn install(o: &Install) -> Result<(), String> {
    require_root()?;
    let (user, uid, gid) = require_user(o)?;
    write_config(o)?;
    hand_over(o, uid, gid)?;
    // launchd opens the log as root; make it the user's so rotation works.
    std::fs::OpenOptions::new().create(true).append(true).open(LOG).map_err(|e| format!("{LOG}: {e}"))?;
    std::os::unix::fs::chown(LOG, Some(uid), Some(gid)).map_err(|e| format!("{LOG}: {e}"))?;
    let exe = exe()?;
    let target = format!("system/{LABEL}");
    let _ = std::process::Command::new("launchctl").args(["bootout", &target]).output();
    std::fs::write(PLIST, launchd_plist(&exe, &o.config, &user)).map_err(|e| format!("{PLIST}: {e}"))?;
    println!("Wrote {PLIST} (runs as {user})");
    command("launchctl", &["enable", &target])?;
    command("launchctl", &["bootstrap", "system", PLIST])?;
    // Let it through the application firewall, if that's on.
    let fw = "/usr/libexec/ApplicationFirewall/socketfilterfw";
    let exe_s = exe.display().to_string();
    let _ = std::process::Command::new(fw).args(["--add", &exe_s]).output();
    let _ = std::process::Command::new(fw).args(["--unblockapp", &exe_s]).output();
    println!("\nbsnfs is running and starts at boot.");
    println!("  status:  sudo launchctl print system/{LABEL} | head");
    println!("  log:     tail -f {LOG}");
    println!("  config:  {} (then: {RESTART})", o.config.display());
    println!(
        "\nmacOS privacy protection blocks services from Desktop, Documents, Downloads,\n\
         iCloud Drive and external disks. To share folders there, add\n  {exe_s}\n\
         under System Settings > Privacy & Security > Full Disk Access."
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall() -> Result<(), String> {
    require_root()?;
    let _ = command("launchctl", &["bootout", &format!("system/{LABEL}")]);
    if Path::new(PLIST).exists() {
        std::fs::remove_file(PLIST).map_err(|e| format!("{PLIST}: {e}"))?;
        println!("Removed {PLIST}");
    }
    println!("Kept {} and {LOG}; delete them if you no longer need them.", data_dir().display());
    Ok(())
}

#[cfg(unix)]
fn run(_args: &[String]) -> Result<(), String> {
    Err("`service run` is for Windows; on this system the service runs `bsnfs --config FILE`".into())
}

// ── Windows: Service Control Manager ────────────────────────────────────────

#[cfg(windows)]
mod windows {
    use std::ffi::{OsStr, OsString};
    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
        ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::{Install, NAME, RESTART, data_dir, exe, write_config};

    const FIREWALL_RULE: &str = "name=bsnfs";

    fn netsh(args: &[&str]) {
        let _ = std::process::Command::new("netsh").args(["advfirewall", "firewall"]).args(args).output();
    }

    pub fn install(o: &Install) -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)
            .map_err(|e| format!("can't reach the Service Control Manager ({e}); run as Administrator"))?;
        write_config(o)?;
        let exe = exe()?;
        if o.user.is_none() {
            println!("Note: running as LocalSystem, which can read and write every file. Use --user to limit that.");
        }
        let info = ServiceInfo {
            name: NAME.into(),
            display_name: "bsnfs NFS server".into(),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: exe.clone(),
            launch_arguments: vec!["service".into(), "run".into(), "--config".into(), o.config.clone().into_os_string()],
            dependencies: vec![],
            account_name: o.user.as_ref().map(OsString::from),
            account_password: o.password.as_ref().map(OsString::from),
        };
        let access = ServiceAccess::CHANGE_CONFIG | ServiceAccess::START | ServiceAccess::STOP | ServiceAccess::QUERY_STATUS;
        // Already installed (an upgrade, or a second install): reconfigure it.
        let service = match manager.open_service(NAME, access) {
            Ok(existing) => {
                stop_and_wait(&existing);
                existing.change_config(&info).map_err(|e| format!("updating the service failed: {e}"))?;
                println!("Updated the existing bsnfs service");
                existing
            }
            Err(_) => manager.create_service(&info, access).map_err(|e| format!("creating the service failed: {e}"))?,
        };
        let _ = service.set_description("Shares folders with NFS v2/v3 clients.");
        let program = format!("program={}", exe.display());
        netsh(&["delete", "rule", FIREWALL_RULE]);
        netsh(&["add", "rule", FIREWALL_RULE, "dir=in", "action=allow", &program, "enable=yes", "profile=any"]);
        service.start::<&OsStr>(&[]).map_err(|e| format!("the service was installed but didn't start: {e}"))?;
        println!("\nbsnfs is running and starts at boot.");
        println!("  status:  Get-Service bsnfs");
        println!("  log:     {}", data_dir().join("bsnfs.log").display());
        println!("  config:  {} (then: {RESTART})", o.config.display());
        Ok(())
    }

    pub fn uninstall() -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|e| format!("can't reach the Service Control Manager ({e}); run as Administrator"))?;
        let service = manager
            .open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)
            .map_err(|e| format!("can't open the bsnfs service: {e}"))?;
        stop_and_wait(&service);
        service.delete().map_err(|e| format!("removing the service failed: {e}"))?;
        netsh(&["delete", "rule", FIREWALL_RULE]);
        println!("Removed the bsnfs service. Kept {}.", data_dir().display());
        Ok(())
    }

    fn stop_and_wait(service: &windows_service::service::Service) {
        if service.query_status().map(|s| s.current_state != ServiceState::Stopped).unwrap_or(false) {
            let _ = service.stop();
            for _ in 0..50 {
                if service.query_status().map(|s| s.current_state == ServiceState::Stopped).unwrap_or(true) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    static CONFIG: OnceLock<PathBuf> = OnceLock::new();
    static STOP: AtomicBool = AtomicBool::new(false);

    define_windows_service!(ffi_service_main, service_main);

    /// Entry point when the SCM starts us (`bsnfs service run --config FILE`).
    pub fn run(args: &[String]) -> Result<(), String> {
        let config = match args {
            [flag, file] if flag == "--config" || flag == "-C" => PathBuf::from(file),
            _ => super::default_config(),
        };
        let _ = CONFIG.set(config);
        service_dispatcher::start(NAME, ffi_service_main)
            .map_err(|e| format!("{e} (this command is run by Windows; use `bsnfs service install`)"))
    }

    fn status(state: ServiceState, exit: u32) -> ServiceStatus {
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            },
            exit_code: if exit == 0 { ServiceExitCode::NO_ERROR } else { ServiceExitCode::ServiceSpecific(exit) },
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        }
    }

    fn service_main(_args: Vec<OsString>) {
        let handler = |control: ServiceControl| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                STOP.store(true, Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let Ok(handle) = service_control_handler::register(NAME, handler) else { return };
        let config = CONFIG.get().cloned().unwrap_or_else(super::default_config);
        let cfg = match crate::config::parse([OsString::from("--config"), config.into_os_string()]) {
            Ok(crate::config::Parsed::Run(cfg)) => *cfg,
            Ok(_) => {
                let _ = handle.set_service_status(status(ServiceState::Stopped, 1));
                return;
            }
            Err(e) => {
                // Nowhere else to say it: write next to the config.
                let _ = std::fs::write(data_dir().join("bsnfs-error.log"), format!("bsnfs: {e}\n"));
                let _ = handle.set_service_status(status(ServiceState::Stopped, 2));
                return;
            }
        };
        let _ = handle.set_service_status(status(ServiceState::Running, 0));
        let code = match crate::app::run(cfg, &STOP) {
            Ok(()) => 0,
            Err(e) => {
                crate::warn!("{e}");
                let _ = std::fs::write(data_dir().join("bsnfs-error.log"), format!("bsnfs: {e}\n"));
                3
            }
        };
        let _ = handle.set_service_status(status(ServiceState::Stopped, code));
    }
}

#[cfg(windows)]
fn install(o: &Install) -> Result<(), String> {
    windows::install(o)
}

#[cfg(windows)]
fn uninstall() -> Result<(), String> {
    windows::uninstall()
}

#[cfg(windows)]
fn run(args: &[String]) -> Result<(), String> {
    windows::run(args)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn install(_o: &Install) -> Result<(), String> {
    Err("installing a service isn't supported on this platform; run `bsnfs --config FILE` from your init system".into())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn uninstall() -> Result<(), String> {
    install(&Install { user: None, password: None, config: default_config(), shares: Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_config_parses() {
        let dir = std::env::temp_dir();
        let f = dir.join(format!("bsnfs_starter_{}.conf", std::process::id()));
        std::fs::write(&f, starter_config(&[format!("demo={}", dir.display())])).unwrap();
        let args = config::config_file_args(&f).unwrap();
        assert!(args.contains(&"--admin=127.0.0.1:8080".to_string()), "{args:?}");
        assert!(args.iter().any(|a| a.starts_with("--exports-file=")));
        assert!(args.contains(&format!("demo={}", dir.display())));
        std::fs::remove_file(f).ok();
    }

    #[test]
    fn install_options() {
        let o = parse_install(&["--user".into(), "alice".into(), "--config".into(), "/x.conf".into(), "/srv".into()]).unwrap();
        assert_eq!(o.user.as_deref(), Some("alice"));
        assert_eq!(o.config, PathBuf::from("/x.conf"));
        assert_eq!(o.shares, ["/srv"]);
        assert!(parse_install(&["--bogus".into()]).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_escapes_paths() {
        let p = launchd_plist(Path::new("/opt/a&b/bsnfs"), Path::new("/etc/x.conf"), "dani");
        assert!(p.contains("<string>/opt/a&amp;b/bsnfs</string>"));
        assert!(p.contains("<key>UserName</key>\n    <string>dani</string>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unit_runs_as_user() {
        let u = systemd_unit(Path::new("/usr/bin/bsnfs"), Path::new("/etc/bsnfs/bsnfs.conf"), "dani");
        assert!(u.contains("User=dani\n"));
        assert!(u.contains("ExecStart=\"/usr/bin/bsnfs\" --config \"/etc/bsnfs/bsnfs.conf\""));
    }
}
