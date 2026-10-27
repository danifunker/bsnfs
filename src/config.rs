//! Command-line configuration, and the small exports file the admin UI
//! saves its changes to.

use std::ffi::OsString;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};


pub const USAGE: &str = "\
Usage: bsnfs [OPTIONS] [[NAME=]DIR]...

Share host folders over NFS v2 and v3, on UDP and TCP.
Each DIR is exported as /NAME (default: the folder's own name).

Options:
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
      --admin-password PW  require this password for the admin UI (user
                           \"admin\"); also read from BSNFS_ADMIN_PASSWORD
  -c, --exports-file FILE  load exports from FILE and save admin UI changes
                           to it
  -v, --verbose            log every call (-vv: also each error)
  -q, --quiet              warnings only
  -h, --help
";

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
            verbosity: 1,
        }
    }
}

pub enum Parsed {
    Run(Config),
    Help,
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Parsed, String> {
    let mut cfg = Config::default();
    let mut read_only = false;
    let mut args = args.into_iter().map(|a| a.into_string().map_err(|a| format!("argument {a:?} is not valid UTF-8")));
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
    if cfg.exports.is_empty() && cfg.admin.is_none() {
        return Err("nothing to share: give at least one folder (or keep the admin UI on to add them later)".into());
    }
    Ok(Parsed::Run(cfg))
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
            Parsed::Run(c) => Ok(c),
            Parsed::Help => Err("help".into()),
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
