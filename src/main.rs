use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bsnfs::app::{self, PortmapState};
use bsnfs::config::{self, Parsed};
use bsnfs::{info, log, warn};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn main() {
    let cfg = match config::parse(std::env::args_os().skip(1)) {
        Ok(Parsed::Run(cfg)) => cfg,
        Ok(Parsed::Help) => {
            print!("{}", config::USAGE);
            return;
        }
        Err(e) => {
            eprintln!("bsnfs: {e}\n\n{}", config::USAGE);
            std::process::exit(2);
        }
    };
    log::set_level(cfg.verbosity);

    let server = match app::start(cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bsnfs: {e}");
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                eprintln!("hint: ports below 1024 may need root here; try --port 12049 --portmap off");
            }
            std::process::exit(1);
        }
    };

    let p = server.ports;
    let transports = match (server.cfg.udp, server.cfg.tcp) {
        (true, true) => "udp+tcp",
        (true, false) => "udp",
        _ => "tcp",
    };
    let versions: Vec<String> = server.cfg.versions.iter().map(|v| format!("v{v}")).collect();
    info!("bsnfs {} serving NFS {} on {}:{} ({transports})", env!("CARGO_PKG_VERSION"), versions.join("/"), server.cfg.bind, p.nfs);
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
    let exports = server.vfs.exports();
    if exports.is_empty() {
        warn!("no folders shared yet; add some in the admin UI");
    }
    for (_, e) in &exports {
        info!("  {} -> {}{}", e.name, e.root.display(), if e.read_only() { " (read-only)" } else { "" });
    }
    if let Some(name) = exports.first().map(|(_, e)| e.name.clone()) {
        info!("  e.g.: {}", server.mount_hint("THIS_HOST", &name));
    }

    // SAFETY: the handler only stores to an atomic, which is signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    while !STOP.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
    }
    info!("shutting down");
    server.shutdown();
}
