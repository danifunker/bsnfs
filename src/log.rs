//! Minimal leveled logging to stderr, or to a file for services that have
//! no console. Level 0 = warnings only, 1 = normal (startup, mounts),
//! 2 = every RPC call, 3 = call details and errors.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(1);
static FILE: Mutex<Option<File>> = Mutex::new(None);

/// Append log lines to `path` instead of stderr.
pub fn set_file(path: &Path) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    *FILE.lock().unwrap() = Some(f);
    Ok(())
}

/// Write one finished log line.
pub fn emit(line: std::fmt::Arguments) {
    let line = format!("{} {line}\n", timestamp());
    match FILE.lock().unwrap().as_mut() {
        Some(f) => {
            let _ = f.write_all(line.as_bytes());
        }
        None => eprint!("{line}"),
    }
}

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

/// Wall-clock time as `HH:MM:SS.mmm` (local time on Unix, UTC elsewhere).
pub fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    #[cfg(unix)]
    let (h, m, s) = {
        // SAFETY: localtime_r only writes into the tm we pass it. (`as _`
        // rather than naming time_t, whose width musl is changing.)
        let tm = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&(now.as_secs() as _), &mut tm);
            tm
        };
        (tm.tm_hour as u64, tm.tm_min as u64, tm.tm_sec as u64)
    };
    // UTC elsewhere.
    #[cfg(not(unix))]
    let (h, m, s) = ((now.as_secs() / 3600) % 24, (now.as_secs() / 60) % 60, now.as_secs() % 60);
    format!("{h:02}:{m:02}:{s:02}.{:03}", now.subsec_millis())
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $($t:tt)*) => {
        if $crate::log::enabled($lvl) {
            $crate::log::emit(format_args!($($t)*));
        }
    };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::log_at!(0, "warning: {}", format_args!($($t)*)) };
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::log_at!(1, $($t)*) };
}

#[macro_export]
macro_rules! debug {
    ($($t:tt)*) => { $crate::log_at!(2, $($t)*) };
}

#[macro_export]
macro_rules! trace {
    ($($t:tt)*) => { $crate::log_at!(3, $($t)*) };
}
