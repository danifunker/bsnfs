//! Wires the pieces together: builds the backend and RPC programs, binds the
//! listeners, sets up the portmapper, and starts the admin UI.

use std::io;
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{self, Config, ExportSpec, PortmapMode};
use crate::nfs::{self, mount::Mount, nlm::Nlm, Nfs};
use crate::portmap::{self, Mapping, Portmap, IPPROTO_TCP, IPPROTO_UDP};
use crate::rpc::{Dispatcher, Program};
use crate::server;
use crate::vfs::Vfs;
use crate::admin;

/// How clients find our ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortmapState {
    /// We serve portmap ourselves on this port.
    Own(u16),
    /// Registered with the host's portmapper.
    System,
    /// Clients must be told the ports explicitly.
    None,
}

#[derive(Debug, Clone, Copy)]
pub struct Ports {
    pub nfs: u16,
    pub mount: u16,
    pub nlm: Option<u16>,
    pub portmap: PortmapState,
    pub admin: Option<SocketAddr>,
}

/// A running server. Listener threads run until the process exits.
pub struct Server {
    pub cfg: Config,
    pub vfs: Arc<Vfs>,
    pub mount: Arc<Mount>,
    pub dispatcher: Arc<Dispatcher>,
    pub ports: Ports,
    registered: Vec<Mapping>,
    /// Serializes admin changes to the exports and their file.
    pub exports_lock: Mutex<()>,
}

/// Bind UDP and/or TCP on one port, per the config.
fn bind(cfg: &Config, port: u16) -> io::Result<(Option<UdpSocket>, Option<TcpListener>, u16)> {
    let addr = SocketAddr::new(cfg.bind, port);
    let (udp, tcp) = server::bind_pair(addr).map_err(|e| io::Error::new(e.kind(), format!("cannot listen on {addr}: {e}")))?;
    let port = tcp.local_addr()?.port();
    Ok((cfg.udp.then_some(udp), cfg.tcp.then_some(tcp), port))
}

fn serve(d: &Arc<Dispatcher>, socks: (Option<UdpSocket>, Option<TcpListener>, u16)) -> io::Result<u16> {
    let (udp, tcp, port) = socks;
    if let Some(u) = udp {
        server::spawn_udp(u, d.clone())?;
    }
    if let Some(t) = tcp {
        server::spawn_tcp(t, d.clone())?;
    }
    Ok(port)
}

pub fn start(cfg: Config) -> io::Result<Arc<Server>> {
    let vfs = Arc::new(Vfs::new(cfg.exports.iter().map(|e| (e.name.clone(), e.path.clone(), e.read_only)).collect())?);
    vfs.set_symlinks(cfg.symlinks);
    let mount = Arc::new(Mount::new(vfs.clone()));
    let pmap = Arc::new(Portmap::new(Vec::new()));
    let mut programs: Vec<Arc<dyn Program>> =
        vec![Arc::new(Nfs::new(vfs.clone(), cfg.versions.clone())), mount.clone(), pmap.clone()];
    if cfg.nlm {
        programs.push(Arc::new(Nlm));
    }
    let d = Arc::new(Dispatcher::new(programs));

    // Every listener answers every program; the separate ports exist only
    // for clients or firewalls that want them.
    let nfs_port = serve(&d, bind(&cfg, cfg.nfs_port)?)?;
    let mount_port = match cfg.mount_port {
        Some(p) => serve(&d, bind(&cfg, p)?)?,
        None => nfs_port,
    };
    let nlm_port = match (cfg.nlm, cfg.nlm_port) {
        (false, _) => None,
        (true, Some(p)) => Some(serve(&d, bind(&cfg, p)?)?),
        (true, None) => Some(nfs_port),
    };

    let protos: Vec<u32> = [(cfg.udp, IPPROTO_UDP), (cfg.tcp, IPPROTO_TCP)].iter().filter(|p| p.0).map(|p| p.1).collect();
    let mut maps = Vec::new();
    let mut add = |prog: u32, versions: &[u32], port: u16| {
        for &vers in versions {
            for &prot in &protos {
                maps.push(Mapping { prog, vers, prot, port });
            }
        }
    };
    add(nfs::PROG, &cfg.versions, nfs_port);
    add(nfs::mount::PROG, &[1, 2, 3], mount_port);
    if let Some(p) = nlm_port {
        add(nfs::nlm::PROG, &[1, 3, 4], p);
    }

    let mut registered = Vec::new();
    let portmap_state = match cfg.portmap {
        PortmapMode::Off => PortmapState::None,
        PortmapMode::Own | PortmapMode::Auto => match bind(&cfg, cfg.portmap_port) {
            Ok(socks) => {
                let port = serve(&d, socks)?;
                let mut all = maps.clone();
                all.extend(protos.iter().flat_map(|&prot| [2, 3, 4].map(|vers| Mapping { prog: portmap::PROG, vers, prot, port })));
                pmap.set(all);
                PortmapState::Own(port)
            }
            Err(e) if cfg.portmap == PortmapMode::Own => return Err(e),
            Err(e) => {
                info!("portmap: {e}; registering with the host's portmapper instead");
                register_system(&cfg, &maps, &mut registered)
            }
        },
        PortmapMode::System => register_system(&cfg, &maps, &mut registered),
    };

    let ports = Ports { nfs: nfs_port, mount: mount_port, nlm: nlm_port, portmap: portmap_state, admin: None };
    let mut server = Server { cfg, vfs, mount, dispatcher: d, ports, registered, exports_lock: Mutex::new(()) };
    if let Some(addr) = server.cfg.admin {
        let listener = TcpListener::bind(addr).map_err(|e| io::Error::new(e.kind(), format!("admin UI: cannot listen on {addr}: {e}")))?;
        server.ports.admin = Some(listener.local_addr()?);
        let server = Arc::new(server);
        admin::spawn(listener, server.clone())?;
        return Ok(server);
    }
    Ok(Arc::new(server))
}

fn register_system(cfg: &Config, maps: &[Mapping], registered: &mut Vec<Mapping>) -> PortmapState {
    let pm = SocketAddr::from(([127, 0, 0, 1], cfg.portmap_port));
    match portmap::register_with(pm, maps) {
        Ok(n) if n == maps.len() => {
            registered.extend_from_slice(maps);
            PortmapState::System
        }
        Ok(n) => {
            warn!("the host portmapper accepted only {n} of {} registrations; clients may need explicit ports", maps.len());
            registered.extend_from_slice(maps);
            PortmapState::System
        }
        Err(e) => {
            warn!("no portmapper reachable on {pm} ({e}); clients must be given port options explicitly");
            PortmapState::None
        }
    }
}

/// Run a server until `stop` is set. This is the whole of `bsnfs` from the
/// command line, and the body of the Windows service.
pub fn run(cfg: Config, stop: &AtomicBool) -> io::Result<()> {
    crate::log::set_level(cfg.verbosity);
    if let Some(f) = &cfg.log_file {
        crate::log::set_file(f).map_err(|e| io::Error::new(e.kind(), format!("log file {}: {e}", f.display())))?;
    }
    let server = start(cfg)?;
    server.log_banner();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    info!("shutting down");
    server.shutdown();
    Ok(())
}

impl Server {
    /// Log what we're serving and how clients reach it.
    pub fn log_banner(&self) {
        let p = self.ports;
        let transports = match (self.cfg.udp, self.cfg.tcp) {
            (true, true) => "udp+tcp",
            (true, false) => "udp",
            _ => "tcp",
        };
        let versions: Vec<String> = self.cfg.versions.iter().map(|v| format!("v{v}")).collect();
        info!("bsnfs {} serving NFS {} on {}:{} ({transports})", crate::VERSION, versions.join("/"), self.cfg.bind, p.nfs);
        if p.mount != p.nfs {
            info!("  MOUNT on port {}", p.mount);
        }
        match p.nlm {
            Some(n) if n != p.nfs => info!("  lock manager on port {n}"),
            Some(_) => {}
            None => info!("  lock manager off: clients should mount with -o nolock"),
        }
        match p.portmap {
            PortmapState::Own(port) => info!("  portmapper on port {port}"),
            PortmapState::System => info!("  registered with the host's portmapper"),
            PortmapState::None => info!("  no portmapper: clients need -o port={},mountport={}", p.nfs, p.mount),
        }
        if let Some(a) = p.admin {
            info!("  admin page on http://{a}/");
        }
        let exports = self.vfs.exports();
        if exports.is_empty() {
            warn!("no folders shared yet; add some on the admin page");
        }
        for (_, e) in &exports {
            info!("  {} -> {}{}", e.name, e.root.display(), if e.read_only() { " (read-only)" } else { "" });
        }
        if let Some((_, e)) = exports.first() {
            info!("  e.g.: {}", self.mount_hint("THIS_HOST", &e.name));
        }
    }

    /// Undo portmapper registrations. Call before exiting.
    pub fn shutdown(&self) {
        if !self.registered.is_empty() {
            portmap::unregister_with(SocketAddr::from(([127, 0, 0, 1], self.cfg.portmap_port)), &self.registered);
        }
    }

    /// Persist the current exports, if an exports file is configured.
    pub fn save_exports(&self) -> io::Result<()> {
        let Some(file) = &self.cfg.exports_file else { return Ok(()) };
        let specs: Vec<ExportSpec> = self
            .vfs
            .exports()
            .into_iter()
            .map(|(_, e)| ExportSpec { name: e.name.clone(), path: e.root.clone(), read_only: e.read_only() })
            .collect();
        config::save_exports(file, &specs)
    }

    /// A mount command line that works for this server, for display.
    pub fn mount_hint(&self, host: &str, export: &str) -> String {
        let mut opts = vec!["vers=3".to_string()];
        if !self.cfg.tcp {
            opts.push("udp".into());
        }
        if self.ports.portmap == PortmapState::None || self.ports.nfs != 2049 {
            opts.push(format!("port={}", self.ports.nfs));
            opts.push(format!("mountport={}", self.ports.mount));
        }
        if self.ports.nlm.is_none() {
            opts.push("nolock".into());
        }
        format!("mount -t nfs -o {} {host}:{export} /mnt", opts.join(","))
    }
}
