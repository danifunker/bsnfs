//! Minimal leveled logging to stderr. Level 0 = warnings only, 1 = normal
//! (startup, mounts), 2 = every RPC call, 3 = call details and errors.

use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(1);

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
        let secs = now.as_secs() as libc::time_t;
        // SAFETY: localtime_r only writes into the tm we pass it.
        let tm = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&secs, &mut tm);
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
            eprintln!("{} {}", $crate::log::timestamp(), format_args!($($t)*));
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
