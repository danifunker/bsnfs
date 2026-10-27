use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};

use bsnfs::config::{self, Parsed};
use bsnfs::{app, service};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "service") {
        std::process::exit(service::main(&args[1..]));
    }
    let cfg = match config::parse(args) {
        Ok(Parsed::Run(cfg)) => *cfg,
        Ok(Parsed::Help) => {
            print!("{}", config::USAGE);
            return;
        }
        Ok(Parsed::Version) => {
            println!("bsnfs {}", bsnfs::VERSION);
            return;
        }
        Err(e) => {
            eprintln!("bsnfs: {e}\n\n{}", config::USAGE);
            std::process::exit(2);
        }
    };

    // SAFETY: the handler only stores to an atomic, which is signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    if let Err(e) = app::run(cfg, &STOP) {
        eprintln!("bsnfs: {e}");
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            eprintln!("hint: ports below 1024 may need root here; try --port 12049 --portmap off");
        }
        std::process::exit(1);
    }
}
