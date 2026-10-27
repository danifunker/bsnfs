//! Command-line configuration, and the small exports file the admin UI
//! saves its changes to.

use std::ffi::OsString;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};


pub const USAGE: &str = "\
Usage: bsnfs [OPTIONS] [[NAME=]DIR]...
       bsnfs service install|uninstall [OPTIONS]   (see: bsnfs service --help)

Share host folders over NFS v2 and v3, on UDP and TCP.
Each DIR is exported as /NAME (default: the folder's own name).

Options:
  -C, --config FILE        read options from FILE first (see below)
  -p, --port PORT          NFS port; MOUNT and NLM share it (default 2049)
      --mount-port PORT    run MOUNT on its own port instead
      --nlm-port PORT      run the lock manager on its own port instead
      --portmap MODE       auto | own | system | off (default auto)
                             own:    serve portmap on --portmap-port
                             system: register with the host's portmapper
                             auto:   own if the port is free, else system
      --portmap-port PORT  (default 111)
  -b, --bind ADDR          address to listen on (default 0.0.0.0)
      --versions LIST      NFS versions to serve (default 2,3)
      --no-udp             don't serve over UDP
      --no-tcp             don't serve over TCP
      --no-nlm             don't answer lock requests
  -r, --read-only          export everything read-only
      --admin ADDR:PORT    admin web UI address (default 127.0.0.1:8080)
      --no-admin           disable the admin web UI
      --admin-password PW  require this password for the admin UI (any user
                           name); also read from BSNFS_ADMIN_PASSWORD.
                           Required when --admin listens beyond this machine
      --admin-host NAME    also accept the admin UI under this host name
                           (IP addresses, localhost, *.local and this
                           machine's own name always work)
      --share-root DIR     the admin UI may only share (and browse) folders
                           inside DIR; repeatable
      --no-symlinks        refuse client requests to create symlinks
  -c, --exports-file FILE  load exports from FILE and save admin UI changes
                           to it
      --log-file FILE      write the log to FILE instead of the terminal
  -v, --verbose            log every call (-vv: also each error)
  -q, --quiet              warnings only
  -V, --version
  -h, --help

Config file: one option per line, named like the long options above without
the dashes; folders are `share [NAME=]DIR` lines; `#` starts a comment.
Options on the command line override the file.

  port 2049
  admin 0.0.0.0:8080
  read-only
  share irix=/srv/irix
";

/// Options that take no value. In a config file they may be followed by
/// yes/no (or true/false, on/off).
const FLAGS: &[&str] = &["no-udp", "no-tcp", "no-nlm", "read-only", "no-admin", "no-symlinks", "verbose", "quiet"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortmapMode {
    Auto,
    Own,
    System,
    Off,
}

#[derive(Debug, Clone)]
pub struct ExportSpec {
    pub name: String,
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub exports: Vec<ExportSpec>,
    pub exports_file: Option<PathBuf>,
    pub bind: IpAddr,
    pub nfs_port: u16,
    pub mount_port: Option<u16>,
    pub nlm_port: Option<u16>,
    pub portmap: PortmapMode,
    pub portmap_port: u16,
    pub versions: Vec<u32>,
    pub udp: bool,
    pub tcp: bool,
    pub nlm: bool,
    pub admin: Option<SocketAddr>,
    pub admin_password: Option<String>,
    pub admin_hosts: Vec<String>,
    pub share_roots: Vec<PathBuf>,
    pub symlinks: bool,
    pub log_file: Option<PathBuf>,
    pub verbosity: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            exports: Vec::new(),
            exports_file: None,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            nfs_port: 2049,
            mount_port: None,
            nlm_port: None,
            portmap: PortmapMode::Auto,
            portmap_port: 111,
            versions: vec![2, 3],
            udp: true,
            tcp: true,
            nlm: true,
            admin: Some(SocketAddr::from(([127, 0, 0, 1], 8080))),
            admin_password: None,
            admin_hosts: Vec::new(),
            share_roots: Vec::new(),
            symlinks: true,
            log_file: None,
            verbosity: 1,
        }
    }
}

pub enum Parsed {
    Run(Box<Config>),
    Help,
    Version,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Parsed, String> {
    let args: Vec<String> = args
        .into_iter()
        .map(|a| a.into_string().map_err(|a| format!("argument {a:?} is not valid UTF-8")))
        .collect::<Result<_, _>>()?;
    // A config file's options go first, so the command line overrides them.
    let mut from_file = Vec::new();
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let file = match a.as_str() {
            "-C" | "--config" => it.next().ok_or("--config needs a value")?,
            _ => match a.strip_prefix("--config=") {
                Some(f) => f.to_string(),
                None => {
                    rest.push(a);
                    continue;
                }
            },
        };
        from_file.extend(config_file_args(Path::new(&file))?);
    }
    from_file.extend(rest);
    parse_args(from_file)
}

/// Turn a config file into the equivalent command-line arguments.
pub fn config_file_args(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let at = || format!("{}:{}", path.display(), n + 1);
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `key value`, `key=value` and `key = value` all work.
        let (key, value) = match line.find(|c: char| c.is_whitespace() || c == '=') {
            Some(i) => (&line[..i], line[i..].trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim()),
            None => (line, ""),
        };
        let value = value.trim_matches('"');
        if key.starts_with('-') {
            return Err(format!("{}: write option names without dashes ({key})", at()));
        }
        match key {
            "share" if value.is_empty() => return Err(format!("{}: share needs [NAME=]DIR", at())),
            "share" => out.push(value.to_string()),
            "config" => return Err(format!("{}: config files can't include other config files", at())),
            k if FLAGS.contains(&k) => match value.to_ascii_lowercase().as_str() {
                "" | "yes" | "true" | "on" => out.push(format!("--{k}")),
                "no" | "false" | "off" => {}
                v => return Err(format!("{}: {k} takes yes or no, not {v:?}", at())),
            },
            k if value.is_empty() => return Err(format!("{}: {k} needs a value", at())),
            k => out.push(format!("--{k}={value}")),
        }
    }
    Ok(out)
}

fn parse_args(args: Vec<String>) -> Result<Parsed, String> {
    let mut cfg = Config::default();
    let mut read_only = false;
    let mut args = args.into_iter().map(Ok::<String, String>);
    while let Some(arg) = args.next() {
        let arg = arg?;
        // Accept --opt=value as well as --opt value.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| -> Result<String, String> {
            match inline.clone() {
                Some(v) => Ok(v),
                None => args.next().transpose()?.ok_or_else(|| format!("{what} needs a value")),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--log-file" => cfg.log_file = Some(PathBuf::from(value(&flag)?)),
            "-p" | "--port" => cfg.nfs_port = port(&value(&flag)?)?,
            "--mount-port" => cfg.mount_port = Some(port(&value(&flag)?)?),
            "--nlm-port" => cfg.nlm_port = Some(port(&value(&flag)?)?),
            "--portmap-port" => cfg.portmap_port = port(&value(&flag)?)?,
            "--portmap" => {
                cfg.portmap = match value(&flag)?.as_str() {
                    "auto" => PortmapMode::Auto,
                    "own" => PortmapMode::Own,
                    "system" => PortmapMode::System,
                    "off" => PortmapMode::Off,
                    m => return Err(format!("unknown portmap mode {m:?}")),
                }
            }
            "-b" | "--bind" => cfg.bind = value(&flag)?.parse().map_err(|e| format!("--bind: {e}"))?,
            "--versions" => {
                let list = value(&flag)?;
                cfg.versions = list
                    .split(',')
                    .map(|v| match v.trim() {
                        "2" => Ok(2),
                        "3" => Ok(3),
                        v => Err(format!("unsupported NFS version {v:?} (2 and 3 are available)")),
                    })
                    .collect::<Result<_, _>>()?;
                cfg.versions.sort();
                cfg.versions.dedup();
            }
            "--no-udp" => cfg.udp = false,
            "--no-tcp" => cfg.tcp = false,
            "--no-nlm" => cfg.nlm = false,
            "-r" | "--read-only" => read_only = true,
            "--admin" => cfg.admin = Some(value(&flag)?.parse().map_err(|e| format!("--admin: {e} (expected ADDR:PORT)"))?),
            "--no-admin" => cfg.admin = None,
            "--admin-password" => cfg.admin_password = Some(value(&flag)?),
            "--admin-host" => cfg.admin_hosts.push(value(&flag)?),
            "--share-root" => {
                let dir = PathBuf::from(value(&flag)?);
                let canon = std::fs::canonicalize(&dir).map_err(|e| format!("--share-root {}: {e}", dir.display()))?;
                cfg.share_roots.push(canon);
            }
            "--no-symlinks" => cfg.symlinks = false,
            "-c" | "--exports-file" => cfg.exports_file = Some(PathBuf::from(value(&flag)?)),
            "-v" | "--verbose" => cfg.verbosity += 1,
            "-vv" => cfg.verbosity += 2,
            "-q" | "--quiet" => cfg.verbosity = 0,
            f if f.starts_with('-') && f.len() > 1 => return Err(format!("unknown option {f}")),
            spec => cfg.exports.push(export_spec(spec)?),
        }
    }
    if !cfg.udp && !cfg.tcp {
        return Err("--no-udp and --no-tcp leave nothing to serve on".into());
    }
    if cfg.versions.is_empty() {
        return Err("no NFS versions selected".into());
    }
    if cfg.admin_password.is_none() {
        cfg.admin_password = std::env::var("BSNFS_ADMIN_PASSWORD").ok().filter(|p| !p.is_empty());
    }
    if let Some(file) = &cfg.exports_file
        && file.exists() {
            // Folders given on the command line win over same-named file entries.
            let saved = load_exports(file).map_err(|e| format!("{}: {e}", file.display()))?;
            for e in saved {
                if !cfg.exports.iter().any(|c| c.name == e.name) {
                    cfg.exports.push(e);
                }
            }
        }
    if read_only {
        cfg.exports.iter_mut().for_each(|e| e.read_only = true);
    }
    if let Some(a) = cfg.admin
        && !a.ip().is_loopback()
        && cfg.admin_password.is_none()
    {
        return Err(format!(
            "the admin UI on {a} would let anyone on the network share any folder: set --admin-password (or BSNFS_ADMIN_PASSWORD), or keep it on 127.0.0.1"
        ));
    }
    if cfg.exports.is_empty() && cfg.admin.is_none() {
        return Err("nothing to share: give at least one folder (or keep the admin UI on to add them later)".into());
    }
    Ok(Parsed::Run(Box::new(cfg)))
}

fn port(s: &str) -> Result<u16, String> {
    s.parse().map_err(|_| format!("bad port {s:?}"))
}

/// `[NAME=]DIR`, where a leading `/` on NAME is optional.
pub fn export_spec(spec: &str) -> Result<ExportSpec, String> {
    // Only treat the text before `=` as a name if it looks like one, so a
    // path that merely contains `=` still works.
    let looks_like_name = |n: &str| {
        let n = n.strip_prefix('/').unwrap_or(n);
        !n.is_empty() && !n.contains('/')
    };
    let (name, path) = match spec.split_once('=') {
        Some((n, p)) if looks_like_name(n) => (n.to_string(), PathBuf::from(p)),
        _ => (String::new(), PathBuf::from(spec)),
    };
    let name = if name.is_empty() { default_name(&path)? } else { name };
    Ok(ExportSpec { name: format!("/{}", name.trim_start_matches('/')), path, read_only: false })
}

fn default_name(path: &Path) -> Result<String, String> {
    let canon = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(canon.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into()))
}

/// The exports file: one export per line, `NAME<TAB>PATH[<TAB>ro]`.
/// Blank lines and `#` comments are ignored.
pub fn load_exports(file: &Path) -> io::Result<Vec<ExportSpec>> {
    let text = std::fs::read_to_string(file)?;
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let mut f = line.split('\t');
        let (Some(name), Some(path)) = (f.next(), f.next()) else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("line {}: expected NAME<TAB>PATH", n + 1)));
        };
        out.push(ExportSpec { name: name.into(), path: path.into(), read_only: f.next() == Some("ro") });
    }
    Ok(out)
}

pub fn save_exports(file: &Path, exports: &[ExportSpec]) -> io::Result<()> {
    let mut text = String::from("# bsnfs exports: NAME<TAB>PATH[<TAB>ro]\n");
    for e in exports {
        text.push_str(&format!("{}\t{}{}\n", e.name, e.path.display(), if e.read_only { "\tro" } else { "" }));
    }
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Config, String> {
        match parse(args.iter().map(OsString::from))? {
            Parsed::Run(c) => Ok(*c),
            Parsed::Help | Parsed::Version => Err("help".into()),
        }
    }

    #[test]
    fn export_names() {
        let tmp = std::env::temp_dir();
        let t = tmp.to_str().unwrap();
        let c = run(&[&format!("pics={t}"), "--port=12049", "-r"]).unwrap();
        assert_eq!(c.exports[0].name, "/pics");
        assert!(c.exports[0].read_only);
        assert_eq!(c.nfs_port, 12049);
        let c = run(&[t]).unwrap();
        assert!(!c.exports[0].name[1..].contains('/'));
        assert!(run(&["--versions", "4", t]).is_err());
        assert!(run(&["--no-udp", "--no-tcp", t]).is_err());
    }

    #[test]
    fn config_file_then_command_line() {
        let dir = std::env::temp_dir();
        let f = dir.join(format!("bsnfs_conf_{}", std::process::id()));
        std::fs::write(
            &f,
            format!("# comment\nport 12049\nadmin = 127.0.0.1:9999\nread-only yes\nno-nlm no\nshare cfg={}\n", dir.display()),
        )
        .unwrap();
        let c = run(&["--config", f.to_str().unwrap(), "--port", "13049"]).unwrap();
        assert_eq!(c.nfs_port, 13049, "command line wins");
        assert_eq!(c.admin, Some("127.0.0.1:9999".parse().unwrap()));
        assert!(c.nlm, "`no-nlm no` leaves it on");
        assert_eq!(c.exports[0].name, "/cfg");
        assert!(c.exports[0].read_only);

        std::fs::write(&f, "port\n").unwrap();
        assert!(run(&["-C", f.to_str().unwrap()]).unwrap_err().contains(":1: port needs a value"));
        std::fs::write(&f, "--port 1\n").unwrap();
        assert!(run(&["-C", f.to_str().unwrap()]).is_err());
        std::fs::remove_file(f).ok();
    }

    #[test]
    fn exports_file_roundtrip() {
        let f = std::env::temp_dir().join(format!("bsnfs_exports_{}", std::process::id()));
        let specs = vec![
            ExportSpec { name: "/a".into(), path: "/tmp".into(), read_only: false },
            ExportSpec { name: "/b c".into(), path: "/var/tmp".into(), read_only: true },
        ];
        save_exports(&f, &specs).unwrap();
        let back = load_exports(&f).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].name, "/b c");
        assert!(back[1].read_only && !back[0].read_only);
        std::fs::remove_file(f).ok();
    }
}
