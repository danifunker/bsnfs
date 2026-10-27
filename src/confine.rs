//! Filesystem access confined to an export, for [`crate::vfs`].
//!
//! Every operation takes the export root and a path relative to it, and on
//! Unix never hands the kernel a full path: it opens the root, walks down one
//! component at a time with `openat(O_NOFOLLOW | O_DIRECTORY)`, and performs
//! the final step relative to that directory with the `*at()` call and a
//! no-follow flag. A symlink anywhere along the way, including one swapped in
//! by a concurrent request between our check and our use, makes the call fail
//! instead of leading out of the export.
//!
//! Elsewhere (Windows) the same functions use path-based std calls, with a
//! no-symlink check on each step. That is best effort: it narrows, but does
//! not close, the window for a race.

use std::io;
use std::path::Path;

use crate::vfs::Time;

/// What an object is, as far as the backend cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    File,
    Dir,
    Symlink,
    /// Device, FIFO, socket: listed, never opened.
    Other,
}

/// The few host attributes the backend reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    pub kind: Type,
    pub size: u64,
    /// Any execute bit set on the host (always false off Unix).
    pub exec: bool,
    pub atime: Time,
    pub mtime: Time,
}

#[cfg(unix)]
pub use unix::*;

#[cfg(not(unix))]
pub use portable::*;

fn invalid() -> io::Error {
    io::Error::from(io::ErrorKind::InvalidInput)
}

/// The final component of `rel`, which must have one.
fn split(rel: &Path) -> io::Result<(&Path, &std::ffi::OsStr)> {
    let name = rel.file_name().ok_or_else(invalid)?;
    Ok((rel.parent().unwrap_or(Path::new("")), name))
}

#[cfg(unix)]
mod unix {
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Path};

    use super::{Meta, Type, invalid, split};
    use crate::vfs::{SetTime, Time};

    fn cstr(s: &OsStr) -> io::Result<CString> {
        CString::new(s.as_bytes()).map_err(|_| invalid())
    }

    fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
        if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc) }
    }

    fn owned(fd: libc::c_int) -> OwnedFd {
        // SAFETY: `fd` was just returned to us by a successful open call.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }

    fn open_root(root: &Path) -> io::Result<OwnedFd> {
        let p = cstr(root.as_os_str())?;
        // SAFETY: plain open(2) of a NUL-terminated path.
        check(unsafe { libc::open(p.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) }).map(owned)
    }

    /// Open directory `rel` under `root`, refusing a symlink at every step.
    fn open_dir(root: &Path, rel: &Path) -> io::Result<OwnedFd> {
        let mut fd = open_root(root)?;
        for c in rel.components() {
            let Component::Normal(name) = c else { return Err(invalid()) };
            let name = cstr(name)?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            // SAFETY: openat(2) relative to a directory fd we own.
            fd = check(unsafe { libc::openat(fd.as_raw_fd(), name.as_ptr(), flags) }).map(owned)?;
        }
        Ok(fd)
    }

    /// The safely opened directory holding `rel`, and `rel`'s last component.
    fn parent(root: &Path, rel: &Path) -> io::Result<(OwnedFd, CString)> {
        let (dir, name) = split(rel)?;
        Ok((open_dir(root, dir)?, cstr(name)?))
    }

    // stat field widths differ between platforms, hence the casts.
    #[allow(clippy::unnecessary_cast)]
    fn meta(st: &libc::stat) -> Meta {
        let fmt = st.st_mode as u32 & libc::S_IFMT as u32;
        let kind = if fmt == libc::S_IFDIR as u32 {
            Type::Dir
        } else if fmt == libc::S_IFREG as u32 {
            Type::File
        } else if fmt == libc::S_IFLNK as u32 {
            Type::Symlink
        } else {
            Type::Other
        };
        Meta {
            kind,
            size: st.st_size as u64,
            exec: st.st_mode as u32 & 0o111 != 0,
            atime: Time::from_parts(st.st_atime as i64, st.st_atime_nsec as i64),
            mtime: Time::from_parts(st.st_mtime as i64, st.st_mtime_nsec as i64),
        }
    }

    fn stat_at(dir: &OwnedFd, name: &CStr) -> io::Result<Meta> {
        // SAFETY: fstatat(2) writes only into `st`.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        check(unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
        Ok(meta(&st))
    }

    /// Attributes of `rel` itself (a symlink is reported, not followed).
    pub fn lstat(root: &Path, rel: &Path) -> io::Result<Meta> {
        if rel.as_os_str().is_empty() {
            let fd = open_root(root)?;
            // SAFETY: fstat(2) writes only into `st`.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            check(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
            return Ok(meta(&st));
        }
        let (dir, name) = parent(root, rel)?;
        stat_at(&dir, &name)
    }

    /// Open a regular file for reading or writing, optionally creating it
    /// (`Some(true)` = must not exist yet). Never follows a symlink and never
    /// opens a device or FIFO.
    pub fn open_file(root: &Path, rel: &Path, write: bool, create: Option<bool>) -> io::Result<File> {
        let (dir, name) = parent(root, rel)?;
        match stat_at(&dir, &name) {
            Ok(m) if m.kind == Type::Dir => return Err(io::Error::from_raw_os_error(libc::EISDIR)),
            Ok(m) if m.kind != Type::File => return Err(invalid()),
            Ok(_) if create == Some(true) => return Err(io::Error::from_raw_os_error(libc::EEXIST)),
            Err(e) if create.is_none() || e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
        flags |= if write { libc::O_WRONLY } else { libc::O_RDONLY };
        if let Some(excl) = create {
            flags |= libc::O_CREAT | if excl { libc::O_EXCL } else { 0 };
        }
        // SAFETY: openat(2) relative to a directory fd we own. Mode 0666 is
        // narrowed by the umask, like any program creating a file.
        let fd = check(unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o666 as libc::c_uint) })?;
        let file = File::from(owned(fd));
        if !file.metadata()?.is_file() {
            return Err(invalid()); // swapped for something else since we looked
        }
        Ok(file)
    }

    /// Entries of directory `rel` (without `.` and `..`), with attributes.
    pub fn read_dir(root: &Path, rel: &Path) -> io::Result<Vec<(OsString, Meta)>> {
        let dir = open_dir(root, rel)?;
        // SAFETY: fdopendir takes ownership of the dup'd fd; closedir below
        // releases both. readdir's entries are read before the next call.
        let dup = check(unsafe { libc::dup(dir.as_raw_fd()) })?;
        let dp = unsafe { libc::fdopendir(dup) };
        if dp.is_null() {
            let e = io::Error::last_os_error();
            unsafe { libc::close(dup) };
            return Err(e);
        }
        let mut out = Vec::new();
        loop {
            let ent = unsafe { libc::readdir(dp) };
            if ent.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            if let Ok(m) = stat_at(&dir, name) {
                out.push((OsString::from_vec(name.to_bytes().to_vec()), m));
            }
        }
        unsafe { libc::closedir(dp) };
        Ok(out)
    }

    pub fn read_link(root: &Path, rel: &Path) -> io::Result<Vec<u8>> {
        let (dir, name) = parent(root, rel)?;
        let mut buf = vec![0u8; 4096];
        loop {
            // SAFETY: readlinkat(2) writes at most buf.len() bytes into buf.
            let n = unsafe { libc::readlinkat(dir.as_raw_fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if (n as usize) < buf.len() {
                buf.truncate(n as usize);
                return Ok(buf);
            }
            buf.resize(buf.len() * 2, 0);
        }
    }

    pub fn mkdir(root: &Path, rel: &Path) -> io::Result<()> {
        let (dir, name) = parent(root, rel)?;
        // SAFETY: mkdirat(2); 0777 narrowed by the umask.
        check(unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) }).map(drop)
    }

    pub fn symlink(root: &Path, rel: &Path, target: &OsStr) -> io::Result<()> {
        let (dir, name) = parent(root, rel)?;
        let target = cstr(target)?;
        // SAFETY: symlinkat(2) only stores the target text; it isn't followed.
        check(unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) }).map(drop)
    }

    /// Hard-link `from` as `to`. A symlink at `from` is linked, not followed.
    pub fn hard_link(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
        let (fdir, fname) = parent(root, from)?;
        let (tdir, tname) = parent(root, to)?;
        // SAFETY: linkat(2) with flags 0 doesn't follow a symlink at `from`.
        check(unsafe { libc::linkat(fdir.as_raw_fd(), fname.as_ptr(), tdir.as_raw_fd(), tname.as_ptr(), 0) }).map(drop)
    }

    /// Remove a file (or symlink), or with `dir` an empty directory.
    pub fn remove(root: &Path, rel: &Path, dir: bool) -> io::Result<()> {
        let (d, name) = parent(root, rel)?;
        let flags = if dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: unlinkat(2) acts on the entry itself, never through it.
        check(unsafe { libc::unlinkat(d.as_raw_fd(), name.as_ptr(), flags) }).map(drop)
    }

    pub fn rename(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
        let (fdir, fname) = parent(root, from)?;
        let (tdir, tname) = parent(root, to)?;
        // SAFETY: renameat(2) moves entries; it never follows them.
        check(unsafe { libc::renameat(fdir.as_raw_fd(), fname.as_ptr(), tdir.as_raw_fd(), tname.as_ptr()) }).map(drop)
    }

    /// Set access/modification times on `rel` itself.
    pub fn set_times(root: &Path, rel: &Path, atime: SetTime, mtime: SetTime) -> io::Result<()> {
        let ts = |t: SetTime| match t {
            SetTime::DontChange => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT as _ },
            SetTime::ServerTime => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW as _ },
            SetTime::Client(t) => libc::timespec { tv_sec: t.secs as _, tv_nsec: t.nsecs as _ },
        };
        let times = [ts(atime), ts(mtime)];
        if rel.as_os_str().is_empty() {
            let fd = open_root(root)?;
            // SAFETY: futimens(2) reads the two timespecs.
            return check(unsafe { libc::futimens(fd.as_raw_fd(), times.as_ptr()) }).map(drop);
        }
        let (dir, name) = parent(root, rel)?;
        // SAFETY: utimensat(2) reads the two timespecs; the entry isn't followed.
        check(unsafe { libc::utimensat(dir.as_raw_fd(), name.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) })
            .map(drop)
    }
}

/// Path-based fallback (Windows). See the module docs for its limits.
#[cfg(not(unix))]
mod portable {
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, File, FileTimes, OpenOptions};
    use std::io;
    use std::path::{Component, Path, PathBuf};
    use std::time::SystemTime;

    use super::{Meta, Type, invalid, split};
    use crate::vfs::{SetTime, Time};

    fn meta(md: &fs::Metadata) -> Meta {
        let ft = md.file_type();
        let kind = if ft.is_symlink() {
            Type::Symlink
        } else if ft.is_dir() {
            Type::Dir
        } else if ft.is_file() {
            Type::File
        } else {
            Type::Other
        };
        Meta {
            kind,
            size: md.len(),
            exec: false,
            atime: Time::from_system(md.accessed().ok()),
            mtime: Time::from_system(md.modified().ok()),
        }
    }

    /// `root/rel`, after checking no step along the way is a symlink or
    /// junction.
    fn path(root: &Path, rel: &Path) -> io::Result<PathBuf> {
        let mut p = root.to_path_buf();
        let n = rel.components().count();
        for (i, c) in rel.components().enumerate() {
            let Component::Normal(name) = c else { return Err(invalid()) };
            p.push(name);
            if i + 1 < n && !fs::symlink_metadata(&p)?.is_dir() {
                return Err(invalid());
            }
        }
        Ok(p)
    }

    pub fn lstat(root: &Path, rel: &Path) -> io::Result<Meta> {
        Ok(meta(&fs::symlink_metadata(path(root, rel)?)?))
    }

    pub fn open_file(root: &Path, rel: &Path, write: bool, create: Option<bool>) -> io::Result<File> {
        let p = path(root, rel)?;
        match fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => return Err(io::Error::from(io::ErrorKind::IsADirectory)),
            Ok(m) if !m.is_file() => return Err(invalid()),
            Ok(_) if create == Some(true) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(e) if create.is_none() || e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let mut o = OpenOptions::new();
        o.read(!write).write(write);
        match create {
            Some(true) => o.create_new(true),
            Some(false) => o.create(true),
            None => &mut o,
        };
        o.open(p)
    }

    pub fn read_dir(root: &Path, rel: &Path) -> io::Result<Vec<(OsString, Meta)>> {
        let mut out = Vec::new();
        for ent in fs::read_dir(path(root, rel)?)? {
            let ent = ent?;
            if let Ok(m) = fs::symlink_metadata(ent.path()) {
                out.push((ent.file_name(), meta(&m)));
            }
        }
        Ok(out)
    }

    pub fn read_link(root: &Path, rel: &Path) -> io::Result<Vec<u8>> {
        Ok(fs::read_link(path(root, rel)?)?.to_string_lossy().into_owned().into_bytes())
    }

    pub fn mkdir(root: &Path, rel: &Path) -> io::Result<()> {
        fs::create_dir(path(root, rel)?)
    }

    pub fn symlink(_root: &Path, _rel: &Path, _target: &OsStr) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub fn hard_link(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
        fs::hard_link(path(root, from)?, path(root, to)?)
    }

    pub fn remove(root: &Path, rel: &Path, dir: bool) -> io::Result<()> {
        let p = path(root, rel)?;
        if dir { fs::remove_dir(p) } else { fs::remove_file(p) }
    }

    pub fn rename(root: &Path, from: &Path, to: &Path) -> io::Result<()> {
        split(from)?;
        fs::rename(path(root, from)?, path(root, to)?)
    }

    pub fn set_times(root: &Path, rel: &Path, atime: SetTime, mtime: SetTime) -> io::Result<()> {
        let p = path(root, rel)?;
        let md = fs::symlink_metadata(&p)?;
        if !md.is_file() {
            return Ok(()); // directories and links: not portably settable
        }
        let pick = |t: SetTime| match t {
            SetTime::DontChange => None,
            SetTime::ServerTime => Some(SystemTime::now()),
            SetTime::Client(t) => Some(t.to_system()),
        };
        let mut times = FileTimes::new();
        if let Some(t) = pick(atime) {
            times = times.set_accessed(t);
        }
        if let Some(t) = pick(mtime) {
            times = times.set_modified(t);
        }
        OpenOptions::new().write(true).open(p)?.set_times(times)
    }
}

/// Make the file at `rel` exactly `size` bytes long.
pub fn truncate(root: &Path, rel: &Path, size: u64) -> io::Result<()> {
    open_file(root, rel, true, None)?.set_len(size)
}

/// Names and types of a directory's entries, for counting subdirectories.
pub fn subdir_count(root: &Path, rel: &Path) -> io::Result<usize> {
    Ok(read_dir(root, rel)?.into_iter().filter(|(_, m)| m.kind == Type::Dir).count())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::vfs::SetTime;
    #[cfg(unix)]
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("bsnfs_confine_{}_{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        fs::canonicalize(p).unwrap()
    }

    /// A symlink in the middle of a path is never followed, by any operation.
    #[cfg(unix)]
    #[test]
    fn symlinked_directories_lead_nowhere() {
        let root = temp_dir();
        let outside = temp_dir();
        fs::write(outside.join("secret"), b"keep out").unwrap();
        fs::create_dir(outside.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("d")).unwrap();
        let p = Path::new("d/secret");
        assert!(lstat(&root, p).is_err());
        assert!(open_file(&root, p, false, None).is_err());
        assert!(open_file(&root, p, true, None).is_err());
        assert!(open_file(&root, Path::new("d/new"), true, Some(true)).is_err());
        assert!(read_dir(&root, Path::new("d")).is_err());
        assert!(mkdir(&root, Path::new("d/x")).is_err());
        assert!(remove(&root, p, false).is_err());
        assert!(remove(&root, Path::new("d/sub"), true).is_err());
        assert!(rename(&root, p, Path::new("stolen")).is_err());
        assert!(hard_link(&root, p, Path::new("stolen")).is_err());
        assert!(symlink(&root, Path::new("d/x"), OsStr::new("y")).is_err());
        assert!(set_times(&root, p, SetTime::ServerTime, SetTime::ServerTime).is_err());
        assert!(truncate(&root, p, 0).is_err());
        // Untouched.
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"keep out");
        assert!(outside.join("sub").is_dir());
        assert!(!outside.join("new").exists() && !outside.join("x").exists());
        // The link itself is visible, and can be removed or renamed.
        assert_eq!(lstat(&root, Path::new("d")).unwrap().kind, Type::Symlink);
        rename(&root, Path::new("d"), Path::new("e")).unwrap();
        remove(&root, Path::new("e"), false).unwrap();
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    /// A symlink as the final component is never opened through.
    #[cfg(unix)]
    #[test]
    fn symlinked_files_are_not_opened() {
        let root = temp_dir();
        let outside = temp_dir();
        fs::write(outside.join("secret"), b"keep out").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("f")).unwrap();
        assert!(open_file(&root, Path::new("f"), false, None).is_err());
        assert!(open_file(&root, Path::new("f"), true, None).is_err());
        assert!(open_file(&root, Path::new("f"), true, Some(false)).is_err(), "create through a link");
        assert!(truncate(&root, Path::new("f"), 0).is_err());
        set_times(&root, Path::new("f"), SetTime::DontChange, SetTime::Client(Time { secs: 5, nsecs: 0 })).unwrap();
        assert_ne!(lstat(&root, Path::new("f")).unwrap().mtime, Time::default(), "lstat sees the link");
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"keep out");
        assert_ne!(fs::metadata(outside.join("secret")).unwrap().modified().unwrap(), std::time::UNIX_EPOCH + std::time::Duration::from_secs(5));
        assert_eq!(read_link(&root, Path::new("f")).unwrap(), outside.join("secret").as_os_str().as_encoded_bytes());
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn ordinary_operations_work() {
        let root = temp_dir();
        mkdir(&root, Path::new("a")).unwrap();
        let mut f = open_file(&root, Path::new("a/f"), true, Some(true)).unwrap();
        std::io::Write::write_all(&mut f, b"hi").unwrap();
        assert_eq!(lstat(&root, Path::new("a/f")).unwrap().size, 2);
        assert_eq!(lstat(&root, Path::new("")).unwrap().kind, Type::Dir);
        let names: Vec<_> = read_dir(&root, Path::new("a")).unwrap().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, [OsString::from("f")]);
        assert_eq!(subdir_count(&root, Path::new("")).unwrap(), 1);
        rename(&root, Path::new("a/f"), Path::new("g")).unwrap();
        hard_link(&root, Path::new("g"), Path::new("a/h")).unwrap();
        truncate(&root, Path::new("a/h"), 1).unwrap();
        assert_eq!(lstat(&root, Path::new("g")).unwrap().size, 1);
        remove(&root, Path::new("g"), false).unwrap();
        remove(&root, Path::new("a/h"), false).unwrap();
        remove(&root, Path::new("a"), true).unwrap();
        fs::remove_dir_all(&root).ok();
    }
}
