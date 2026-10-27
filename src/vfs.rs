//! The shared-folder backend, modelled on iris's in-core NFS backend: each
//! export keeps a `fileid`↔path map, attributes are synthesized rather than
//! taken from the host, and only std file I/O touches the host (plus a free
//! space query). Nothing here depends on host uids, permission bits or inode
//! numbers, so every host platform serves the same view. Protocol-neutral, so NFS (and later FTP or
//! HTTP) sit on the same backend.
//!
//! **Fileids** are handed out sequentially per export as paths are first
//! seen. The export root is always [`ROOT_ID`] (2), the root inode of every
//! Unix filesystem; 0 and 1 are never used. IRIX's inode monitor (`imon`,
//! used by `fam` and the desktop) treats inode 1 as a pathname-record marker
//! and panics the kernel when a real file carries it.
//!
//! **Handles** carry the export, the fileid, and a per-run generation. Ids
//! are reassigned on every start, so a handle from a previous run would name
//! some other file; the generation turns it into a clean STALE instead. The
//! export root's handle has no generation, so it stays valid across restarts
//! and clients can walk back down to what they had open.
//!
//! **Containment**: wire names are validated single components, `..` is
//! clamped at the export root, and every host access goes through
//! [`crate::confine`], which never follows a symlink, so nothing a client
//! does (including racing its own requests) reaches outside the export.
//! Symlinks inside the share are served as symlinks for the client to
//! resolve.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::confine::{self, Meta, Type};

/// Fileid of every export's root directory.
pub const ROOT_ID: u64 = 2;

/// Longest single name we accept (and report as `name_max`).
pub const NAME_MAX: usize = 255;

/// Filesystem errors, named after the NFS status codes they map to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Perm,
    NoEnt,
    Io,
    Nxio,
    Acces,
    Exist,
    Xdev,
    NotDir,
    IsDir,
    Inval,
    FBig,
    NoSpc,
    RoFs,
    MLink,
    NameTooLong,
    NotEmpty,
    DQuot,
    Stale,
    BadHandle,
    NotSupp,
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        use io::ErrorKind as K;
        // A symlink where confine wanted a directory or file.
        #[cfg(unix)]
        if e.raw_os_error() == Some(libc::ELOOP) {
            return Error::Inval;
        }
        match e.kind() {
            K::NotFound => Error::NoEnt,
            K::PermissionDenied => Error::Acces,
            K::AlreadyExists => Error::Exist,
            K::NotADirectory => Error::NotDir,
            K::IsADirectory => Error::IsDir,
            K::DirectoryNotEmpty => Error::NotEmpty,
            K::ReadOnlyFilesystem => Error::RoFs,
            K::StorageFull => Error::NoSpc,
            K::QuotaExceeded => Error::DQuot,
            K::FileTooLarge => Error::FBig,
            K::CrossesDevices => Error::Xdev,
            K::TooManyLinks => Error::MLink,
            K::InvalidFilename => Error::NameTooLong,
            K::InvalidInput => Error::Inval,
            K::StaleNetworkFileHandle => Error::Stale,
            K::Unsupported => Error::NotSupp,
            _ => Error::Io,
        }
    }
}

/// An object in the share: which export, and its fileid there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle {
    pub export: u8,
    pub id: u64,
}

/// Size of an encoded handle. Fits NFSv2's fixed 32 bytes and NFSv3's 64.
pub const FH_LEN: usize = 16;
const FH_MAGIC: &[u8; 2] = b"BS";
const FH_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Reg,
    Dir,
    Lnk,
}

/// Seconds and nanoseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Time {
    pub secs: u32,
    pub nsecs: u32,
}

impl Time {
    #[cfg_attr(not(unix), allow(dead_code))] // used by the Unix confine
    pub(crate) fn from_parts(secs: i64, nsecs: i64) -> Self {
        Self { secs: secs.clamp(0, u32::MAX as i64) as u32, nsecs: nsecs.clamp(0, 999_999_999) as u32 }
    }

    #[cfg_attr(unix, allow(dead_code))] // used by the non-Unix confine
    pub(crate) fn from_system(t: Option<SystemTime>) -> Self {
        t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| Self { secs: d.as_secs().min(u32::MAX as u64) as u32, nsecs: d.subsec_nanos() })
            .unwrap_or_default()
    }

    #[cfg_attr(unix, allow(dead_code))] // used by the non-Unix confine
    pub(crate) fn to_system(self) -> SystemTime {
        UNIX_EPOCH + Duration::new(self.secs as u64, self.nsecs.min(999_999_999))
    }
}

/// Synthetic POSIX attributes. Deliberately fake: owner 0:0 and a mode by
/// heuristic, so the share looks the same whatever the host is. The NFS
/// layer replaces the owner with whoever is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub kind: Kind,
    /// Permission bits only (`0o777`); the type is in `kind`.
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub fsid: u64,
    pub fileid: u64,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
}

impl Attr {
    /// The `S_IF*` type bits for `kind`.
    pub fn type_bits(&self) -> u32 {
        match self.kind {
            Kind::Reg => 0o100000,
            Kind::Dir => 0o040000,
            Kind::Lnk => 0o120000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SetTime {
    #[default]
    DontChange,
    ServerTime,
    Client(Time),
}

/// Requested attribute changes (NFS `sattr`/`sattr3`). Only `size` and the
/// times are applied; mode and ownership are accepted and ignored.
#[derive(Debug, Clone, Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: SetTime,
    pub mtime: SetTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateMode {
    /// Create, or open an existing file.
    Unchecked,
    /// Fail with `Exist` if the name exists.
    Guarded,
}

pub struct DirEntry {
    pub name: Vec<u8>,
    pub handle: Handle,
    pub attr: Attr,
}

/// A directory snapshot. NFS readdir cookies are indexes into `entries`, so
/// a listing is cached while a client pages through it.
pub struct DirListing {
    /// Changes when the directory does (derived from its mtime).
    pub verifier: u64,
    pub entries: Vec<DirEntry>,
}

pub struct FsStat {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub avail_bytes: u64,
    pub total_files: u64,
    pub free_files: u64,
    pub avail_files: u64,
}

/// fileid ↔ export-relative path. The root is `ROOT_ID` ↔ the empty path.
struct Ids {
    next: u64,
    by_id: HashMap<u64, PathBuf>,
    by_path: HashMap<PathBuf, u64>,
}

impl Ids {
    fn new() -> Self {
        let mut ids = Self { next: ROOT_ID + 1, by_id: HashMap::new(), by_path: HashMap::new() };
        ids.by_id.insert(ROOT_ID, PathBuf::new());
        ids.by_path.insert(PathBuf::new(), ROOT_ID);
        ids
    }

    fn intern(&mut self, rel: &Path) -> u64 {
        if let Some(&id) = self.by_path.get(rel) {
            return id;
        }
        let id = self.next;
        self.next += 1;
        self.by_id.insert(id, rel.to_path_buf());
        self.by_path.insert(rel.to_path_buf(), id);
        id
    }

    /// Drop `rel` and everything below it.
    fn forget(&mut self, rel: &Path) {
        let by_id = &mut self.by_id;
        self.by_path.retain(|p, id| {
            let gone = p.starts_with(rel);
            if gone {
                by_id.remove(id);
            }
            !gone
        });
    }

    /// Re-point `from` and everything below it at `to`, keeping their ids.
    fn rename(&mut self, from: &Path, to: &Path) {
        self.forget(to);
        let moved: Vec<(PathBuf, u64)> =
            self.by_path.iter().filter(|(p, _)| p.starts_with(from)).map(|(p, &id)| (p.clone(), id)).collect();
        for (old, id) in moved {
            let rest = old.strip_prefix(from).unwrap();
            let new = if rest.as_os_str().is_empty() { to.to_path_buf() } else { to.join(rest) };
            self.by_path.remove(&old);
            self.by_path.insert(new.clone(), id);
            self.by_id.insert(id, new);
        }
    }
}

/// One shared folder.
pub struct Export {
    /// Path clients mount, e.g. `/photos`.
    pub name: String,
    /// Canonical host path.
    pub root: PathBuf,
    read_only: AtomicBool,
    fsid: u64,
    ids: Mutex<Ids>,
    /// Directory fileid → (its mtime when counted, number of subdirectories).
    subdirs: Mutex<HashMap<u64, (Time, u32)>>,
}

impl Export {
    pub fn read_only(&self) -> bool {
        self.read_only.load(Ordering::Relaxed)
    }

    fn rel(&self, id: u64) -> Result<PathBuf> {
        self.ids.lock().unwrap().by_id.get(&id).cloned().ok_or(Error::Stale)
    }

    fn intern(&self, rel: &Path) -> u64 {
        self.ids.lock().unwrap().intern(rel)
    }

    /// A directory's subdirectory count, recounted only when its mtime
    /// changes (adding or removing an entry always bumps it).
    fn subdirs(&self, id: u64, rel: &Path, md: &Meta) -> u32 {
        if let Some(&(t, n)) = self.subdirs.lock().unwrap().get(&id)
            && t == md.mtime
        {
            return n;
        }
        let n = confine::subdir_count(&self.root, rel).unwrap_or(0).min(u32::MAX as usize - 2) as u32;
        self.subdirs.lock().unwrap().insert(id, (md.mtime, n));
        n
    }

    /// Synthetic attributes for the object `id` at `rel`, whose host metadata
    /// is `md`.
    fn attr(&self, id: u64, rel: &Path, md: &Meta) -> Attr {
        // The execute bit is the one host permission passed through, so
        // scripts and binaries stay runnable.
        let (kind, mode) = match md.kind {
            Type::Dir => (Kind::Dir, 0o755),
            Type::Symlink => (Kind::Lnk, 0o777),
            Type::File | Type::Other => (Kind::Reg, if md.exec { 0o755 } else { 0o644 }),
        };
        let mode = if self.read_only() && kind != Kind::Lnk { mode & !0o222 } else { mode };
        let mtime = md.mtime;
        Attr {
            kind,
            mode,
            // A directory's own `.` and its entry in the parent, plus each
            // subdirectory's `..`. Old `find`/`fts` read nlink == 2 as "no
            // subdirectories" and stop descending, so this must be real.
            nlink: if kind == Kind::Dir { 2 + self.subdirs(id, rel, md) } else { 1 },
            uid: 0,
            gid: 0,
            size: md.size,
            fsid: self.fsid,
            fileid: id,
            atime: md.atime,
            mtime,
            ctime: mtime, // no portable ctime; reuse mtime
        }
    }
}

/// A resolved handle: its export, where it lives, and its current metadata.
struct Node {
    export: Arc<Export>,
    rel: PathBuf,
    md: Meta,
}


const LISTING_CACHE: usize = 64;

pub struct Vfs {
    /// Indexed by the handle's export byte. Removed exports leave a `None`
    /// so the remaining handles keep their meaning.
    exports: RwLock<Vec<Option<Arc<Export>>>>,
    /// Stamped into every non-root handle; changes on each start.
    generation: u32,
    listings: Mutex<HashMap<Handle, Arc<DirListing>>>,
    /// Whether clients may create symlinks. They are never followed here,
    /// but other programs on the host would follow them.
    symlinks: AtomicBool,
    /// Store client symlinks as `symlink-NAME` files (see [`LINK_PREFIX`]).
    emulate: AtomicBool,
}

/// Emulated symlinks: a client's symlink `NAME` is stored on the host as an
/// ordinary file `symlink-NAME` holding [`LINK_MAGIC`] and the target. Used
/// where the host can't hold real symlinks (Windows, FAT/exFAT disks). A file
/// only counts as one if its contents start with the magic line, so a real
/// file that happens to be called `symlink-...` stays a file.
const LINK_PREFIX: &[u8] = b"symlink-";
const LINK_MAGIC: &[u8] = b"!<bsnfs-symlink>\n";
const LINK_MAX: u64 = 4096;

impl Vfs {
    /// `exports` is `(name, host path, read_only)`.
    pub fn new(exports: Vec<(String, PathBuf, bool)>) -> io::Result<Self> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let generation = (now.subsec_nanos() ^ (now.as_secs() as u32).rotate_left(16) ^ std::process::id()) | 1;
        let vfs = Self {
            exports: RwLock::new(Vec::new()),
            generation,
            listings: Mutex::new(HashMap::new()),
            symlinks: AtomicBool::new(true),
            emulate: AtomicBool::new(cfg!(windows)),
        };
        for (name, path, read_only) in exports {
            vfs.add_export(&name, &path, read_only)?;
        }
        Ok(vfs)
    }

    /// Share another folder. Returns its index.
    pub fn add_export(&self, name: &str, path: &Path, read_only: bool) -> io::Result<usize> {
        let invalid = |msg: String| io::Error::new(io::ErrorKind::InvalidInput, msg);
        let name = normalize_export_name(name).ok_or_else(|| invalid(format!("bad export name {name:?}")))?;
        let root = fs::canonicalize(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        if !fs::metadata(&root)?.is_dir() {
            return Err(invalid(format!("{} is not a directory", root.display())));
        }
        let mut exports = self.exports.write().unwrap();
        if exports.iter().flatten().any(|e| e.name == name) {
            return Err(invalid(format!("an export named {name} already exists")));
        }
        let i = match exports.iter().position(Option::is_none) {
            Some(i) => i,
            None if exports.len() < u8::MAX as usize => {
                exports.push(None);
                exports.len() - 1
            }
            None => return Err(invalid("too many exports".into())),
        };
        // Distinct per export and stable across runs, so clients see each
        // export as its own filesystem.
        let fsid = name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        exports[i] = Some(Arc::new(Export {
            name,
            root,
            read_only: AtomicBool::new(read_only),
            fsid,
            ids: Mutex::new(Ids::new()),
            subdirs: Mutex::new(HashMap::new()),
        }));
        Ok(i)
    }

    /// Allow or refuse client requests to create symlinks.
    pub fn set_symlinks(&self, allow: bool) {
        self.symlinks.store(allow, Ordering::Relaxed);
    }

    /// Store client symlinks as `symlink-NAME` files. Always on for Windows
    /// hosts, which can't hold real ones.
    pub fn set_emulate_symlinks(&self, on: bool) {
        self.emulate.store(on || cfg!(windows), Ordering::Relaxed);
    }

    fn emulating(&self) -> bool {
        self.emulate.load(Ordering::Relaxed)
    }

    /// Host attributes of `rel`, with emulated symlinks reported as symlinks.
    fn stat(&self, export: &Export, rel: &Path) -> io::Result<Meta> {
        let md = confine::lstat(&export.root, rel)?;
        Ok(self.classify(export, rel, md))
    }

    fn classify(&self, export: &Export, rel: &Path, md: Meta) -> Meta {
        let named = rel.file_name().is_some_and(|n| {
            let n = name_bytes(n.to_os_string());
            n.len() > LINK_PREFIX.len() && n.starts_with(LINK_PREFIX)
        });
        let sized = (LINK_MAGIC.len() as u64..=LINK_MAGIC.len() as u64 + LINK_MAX).contains(&md.size);
        if md.kind == Type::File && named && sized && self.emulating()
            && let Some(target) = read_emulated(&export.root, rel)
        {
            return Meta { kind: Type::Symlink, size: target.len() as u64, ..md };
        }
        md
    }

    /// Whether the symlink at `rel` is an emulated one (a file on the host).
    fn is_emulated(export: &Export, rel: &Path) -> bool {
        confine::lstat(&export.root, rel).is_ok_and(|m| m.kind == Type::File)
    }

    /// Where the client's `name` in `dir_rel` lives on the host, and what's
    /// there: `dir/name` itself, else an emulated link `dir/symlink-name`.
    fn host_entry(&self, export: &Export, dir_rel: &Path, name: &[u8]) -> Result<(PathBuf, Option<Meta>)> {
        let rel = dir_rel.join(component(name)?);
        match self.stat(export, &rel) {
            Ok(m) => return Ok((rel, Some(m))),
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            Err(_) => {}
        }
        if self.emulating()
            && let Ok(lrel) = link_rel(dir_rel, name)
            && let Ok(m) = self.stat(export, &lrel)
            && m.kind == Type::Symlink
        {
            return Ok((lrel, Some(m)));
        }
        Ok((rel, None))
    }

    /// Stop sharing an export. Clients holding its handles get STALE.
    pub fn remove_export(&self, export: usize) -> bool {
        let mut exports = self.exports.write().unwrap();
        let Some(slot) = exports.get_mut(export) else { return false };
        if slot.take().is_none() {
            return false;
        }
        self.listings.lock().unwrap().retain(|h, _| h.export as usize != export);
        true
    }

    pub fn set_read_only(&self, export: usize, read_only: bool) -> bool {
        match self.exports.read().unwrap().get(export) {
            Some(Some(e)) => {
                e.read_only.store(read_only, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Current exports with their indexes.
    pub fn exports(&self) -> Vec<(usize, Arc<Export>)> {
        let exports = self.exports.read().unwrap();
        exports.iter().enumerate().filter_map(|(i, e)| Some((i, e.clone()?))).collect()
    }

    /// The root handle of an export; `None` if there is no such export.
    pub fn root(&self, export: usize) -> Option<Handle> {
        matches!(self.exports.read().unwrap().get(export), Some(Some(_))).then_some(Handle { export: export as u8, id: ROOT_ID })
    }

    fn export(&self, h: &Handle) -> Result<Arc<Export>> {
        match self.exports.read().unwrap().get(h.export as usize) {
            Some(Some(e)) => Ok(e.clone()),
            _ => Err(Error::Stale),
        }
    }

    pub fn is_read_only(&self, h: &Handle) -> bool {
        self.export(h).map(|e| e.read_only()).unwrap_or(true)
    }

    // ── wire handles ────────────────────────────────────────────────────────

    pub fn encode(&self, h: Handle) -> [u8; FH_LEN] {
        let generation = if h.id == ROOT_ID { 0 } else { self.generation };
        let mut b = [0u8; FH_LEN];
        b[..2].copy_from_slice(FH_MAGIC);
        b[2] = FH_VERSION;
        b[3] = h.export;
        b[4..8].copy_from_slice(&generation.to_be_bytes());
        b[8..16].copy_from_slice(&h.id.to_be_bytes());
        b
    }

    /// Decode a wire handle; longer buffers (NFSv2's zero-padded 32 bytes)
    /// are fine. A handle from an earlier run is STALE, except the roots.
    pub fn decode(&self, b: &[u8]) -> Result<Handle> {
        if b.len() < FH_LEN || &b[..2] != FH_MAGIC {
            return Err(Error::BadHandle);
        }
        if b[2] != FH_VERSION {
            return Err(Error::Stale);
        }
        let generation = u32::from_be_bytes(b[4..8].try_into().unwrap());
        let id = u64::from_be_bytes(b[8..16].try_into().unwrap());
        if id < ROOT_ID || (id != ROOT_ID && generation != self.generation) {
            return Err(Error::Stale);
        }
        Ok(Handle { export: b[3], id })
    }

    /// Resolve a client's mount path to a handle. Accepts the export name or
    /// its host path, optionally followed by a subdirectory. With a single
    /// export, `/` and bare subdirectory paths resolve into it too.
    pub fn mount(&self, path: &[u8]) -> Result<(usize, Handle)> {
        let path = String::from_utf8_lossy(path);
        let path = path.trim_end_matches('/');
        let exports = self.exports();
        for (i, e) in &exports {
            for prefix in [e.name.trim_end_matches('/'), &*e.root.to_string_lossy()] {
                if let Some(rest) = path.strip_prefix(prefix)
                    && (rest.is_empty() || rest.starts_with('/'))
                {
                    return Ok((*i, self.walk(self.root(*i).ok_or(Error::NoEnt)?, rest)?));
                }
            }
        }
        if let [(i, _)] = exports[..] {
            return Ok((i, self.walk(self.root(i).ok_or(Error::NoEnt)?, path)?));
        }
        Err(Error::NoEnt)
    }

    fn walk(&self, mut h: Handle, sub: &str) -> Result<Handle> {
        for comp in sub.split('/').filter(|c| !c.is_empty() && *c != ".") {
            if comp == ".." {
                return Err(Error::NoEnt);
            }
            h = self.lookup(&h, comp.as_bytes())?.0;
        }
        if self.getattr(&h)?.kind != Kind::Dir {
            return Err(Error::NotDir);
        }
        Ok(h)
    }

    fn resolve(&self, h: &Handle) -> Result<Node> {
        let export = self.export(h)?;
        let rel = export.rel(h.id)?;
        // Gone from the host since we handed out the handle: stale.
        let md = self.stat(&export, &rel).map_err(|_| Error::Stale)?;
        Ok(Node { export, rel, md })
    }

    /// Resolve a directory handle.
    fn resolve_dir(&self, h: &Handle) -> Result<Node> {
        let node = self.resolve(h)?;
        if node.md.kind != Type::Dir {
            return Err(Error::NotDir);
        }
        Ok(node)
    }

    // ── queries ─────────────────────────────────────────────────────────────

    pub fn getattr(&self, h: &Handle) -> Result<Attr> {
        let node = self.resolve(h)?;
        Ok(node.export.attr(h.id, &node.rel, &node.md))
    }

    /// Look up `name` in directory `dir`. `.` and `..` are answered here:
    /// SVR4-derived clients send `..` over the wire on every DNLC miss, and it
    /// is clamped at the export root so it can never leave the share.
    pub fn lookup(&self, dir: &Handle, name: &[u8]) -> Result<(Handle, Attr)> {
        let node = self.resolve_dir(dir)?;
        let (rel, md) = match name {
            b"." | b".." => {
                let rel = if name == b"." { node.rel.clone() } else { node.rel.parent().map(Path::to_path_buf).unwrap_or_default() };
                let md = self.stat(&node.export, &rel)?;
                (rel, md)
            }
            _ => match self.host_entry(&node.export, &node.rel, name)? {
                (rel, Some(md)) => (rel, md),
                (_, None) => return Err(Error::NoEnt),
            },
        };
        let id = node.export.intern(&rel);
        Ok((Handle { export: dir.export, id }, node.export.attr(id, &rel, &md)))
    }

    pub fn readlink(&self, h: &Handle) -> Result<Vec<u8>> {
        let node = self.resolve(h)?;
        if node.md.kind != Type::Symlink {
            return Err(Error::Inval);
        }
        if Self::is_emulated(&node.export, &node.rel) {
            return read_emulated(&node.export.root, &node.rel).ok_or(Error::Stale);
        }
        Ok(confine::read_link(&node.export.root, &node.rel)?)
    }

    /// List a directory, `.` and `..` first, then the entries by name. With
    /// `fresh`, always re-read it (a client starting a listing); otherwise
    /// reuse the cached snapshot so cookies stay valid.
    pub fn readdir(&self, dir: &Handle, fresh: bool) -> Result<Arc<DirListing>> {
        if !fresh && let Some(l) = self.listings.lock().unwrap().get(dir) {
            return Ok(l.clone());
        }
        let node = self.resolve_dir(dir)?;
        let export = &node.export;
        let entry = |name: Vec<u8>, rel: &Path, md: &Meta| {
            let id = export.intern(rel);
            DirEntry { name, handle: Handle { export: dir.export, id }, attr: export.attr(id, rel, md) }
        };
        let mut entries = Vec::new();
        let parent = node.rel.parent().map(Path::to_path_buf).unwrap_or_default();
        for (name, rel) in [(&b"."[..], &node.rel), (&b".."[..], &parent)] {
            if let Ok(md) = self.stat(export, rel) {
                entries.push(entry(name.to_vec(), rel, &md));
            }
        }
        let raw = confine::read_dir(&export.root, &node.rel)?;
        let host_names: std::collections::HashSet<Vec<u8>> = raw.iter().map(|(n, _)| name_bytes(n.clone())).collect();
        let mut children = Vec::new();
        for (os_name, md) in raw {
            let mut name = name_bytes(os_name.clone());
            // Skip anything that wouldn't round-trip as a safe component.
            if component(&name).is_err() {
                continue;
            }
            let md = self.classify(export, &node.rel.join(&os_name), md);
            // An emulated link shows under its own name, unless a real entry
            // already has that name.
            if md.kind == Type::Symlink && name.starts_with(LINK_PREFIX) && !host_names.contains(&name[LINK_PREFIX.len()..]) {
                name.drain(..LINK_PREFIX.len());
            }
            children.push((name, os_name, md));
        }
        // Sorted, so a re-read after cache eviction keeps cookie positions.
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, os_name, md) in children {
            entries.push(entry(name, &node.rel.join(os_name), &md));
        }
        let mtime = node.md.mtime;
        let listing = Arc::new(DirListing { verifier: ((mtime.secs as u64) << 32) | mtime.nsecs as u64, entries });
        let mut cache = self.listings.lock().unwrap();
        if cache.len() >= LISTING_CACHE {
            cache.clear();
        }
        cache.insert(*dir, listing.clone());
        Ok(listing)
    }

    /// Capacity of the host filesystem holding the object, so clients see
    /// the real free space.
    pub fn fsstat(&self, h: &Handle) -> Result<FsStat> {
        disk_space(&self.resolve(h)?.export.root)
    }

    // ── file data ───────────────────────────────────────────────────────────

    fn open_file(node: &Node, write: bool) -> Result<File> {
        match node.md.kind {
            Type::Dir => Err(Error::IsDir),
            Type::File => Ok(confine::open_file(&node.export.root, &node.rel, write, None)?),
            // Symlinks (real or emulated) are never opened through.
            _ => Err(Error::Inval),
        }
    }

    /// Read up to `count` bytes at `offset`. Returns the data and whether it
    /// reached end of file.
    pub fn read(&self, h: &Handle, offset: u64, count: u32) -> Result<(Vec<u8>, bool)> {
        let node = self.resolve(h)?;
        let mut f = Self::open_file(&node, false)?;
        let size = f.metadata()?.len();
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; count as usize];
        let mut got = 0;
        while got < buf.len() {
            match f.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        buf.truncate(got);
        Ok((buf, offset + got as u64 >= size))
    }

    /// Write `data` at `offset`; with `sync`, flush it to stable storage.
    pub fn write(&self, h: &Handle, offset: u64, data: &[u8], sync: bool) -> Result<()> {
        let node = self.resolve(h)?;
        writable(&node.export)?;
        let mut f = Self::open_file(&node, true)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(data)?;
        if sync {
            f.sync_data()?;
        }
        Ok(())
    }

    /// Flush a file's data to stable storage (NFSv3 COMMIT).
    pub fn commit(&self, h: &Handle) -> Result<()> {
        let node = self.resolve(h)?;
        // Flushing needs write access on Windows. A file we can't open for
        // writing holds no unflushed writes of ours, so there's nothing to do.
        if node.md.kind == Type::File
            && let Ok(f) = Self::open_file(&node, true)
        {
            f.sync_all()?;
        }
        Ok(())
    }

    // ── attributes ──────────────────────────────────────────────────────────

    /// Apply a size or time change. Mode and ownership changes are accepted
    /// and ignored: the attributes are synthetic.
    pub fn setattr(&self, h: &Handle, sa: &SetAttr) -> Result<()> {
        let node = self.resolve(h)?;
        if sa.size.is_some() || sa.atime != SetTime::DontChange || sa.mtime != SetTime::DontChange {
            writable(&node.export)?;
        }
        apply(&node.export.root, &node.rel, &node.md, sa)
    }

    // ── namespace changes ───────────────────────────────────────────────────

    /// Resolve a writable directory, and where `name` in it lives on the
    /// host and what's there now.
    fn child(&self, dir: &Handle, name: &[u8]) -> Result<(Arc<Export>, PathBuf, PathBuf, Option<Meta>)> {
        let node = self.resolve_dir(dir)?;
        writable(&node.export)?;
        let (rel, md) = self.host_entry(&node.export, &node.rel, name)?;
        Ok((node.export, node.rel, rel, md))
    }

    /// Intern a newly created object and return its handle and attributes.
    fn created(&self, dir: &Handle, export: &Export, rel: &Path) -> Result<(Handle, Attr)> {
        let md = self.stat(export, rel)?;
        let id = export.intern(rel);
        Ok((Handle { export: dir.export, id }, export.attr(id, rel, &md)))
    }

    pub fn create(&self, dir: &Handle, name: &[u8], how: CreateMode, sa: &SetAttr) -> Result<(Handle, Attr)> {
        let (export, _, rel, existing) = self.child(dir, name)?;
        // Never open through a symlink, or onto a directory.
        if let Some(md) = existing
            && md.kind != Type::File
        {
            return Err(if how == CreateMode::Guarded || md.kind != Type::Dir { Error::Exist } else { Error::IsDir });
        }
        confine::open_file(&export.root, &rel, true, Some(how == CreateMode::Guarded))?;
        apply(&export.root, &rel, &confine::lstat(&export.root, &rel)?, sa)?;
        self.created(dir, &export, &rel)
    }

    pub fn mkdir(&self, dir: &Handle, name: &[u8], sa: &SetAttr) -> Result<(Handle, Attr)> {
        let (export, _, rel, existing) = self.child(dir, name)?;
        if existing.is_some() {
            return Err(Error::Exist);
        }
        confine::mkdir(&export.root, &rel)?;
        apply(&export.root, &rel, &confine::lstat(&export.root, &rel)?, &SetAttr { size: None, ..sa.clone() })?;
        self.created(dir, &export, &rel)
    }

    pub fn symlink(&self, dir: &Handle, name: &[u8], target: &[u8]) -> Result<(Handle, Attr)> {
        let (export, dir_rel, rel, existing) = self.child(dir, name)?;
        if !self.symlinks.load(Ordering::Relaxed) {
            return Err(Error::Acces);
        }
        if target.is_empty() || target.contains(&0) || target.len() as u64 > LINK_MAX {
            return Err(Error::Inval);
        }
        if existing.is_some() {
            return Err(Error::Exist);
        }
        if !self.emulating() {
            confine::symlink(&export.root, &rel, &os_string_from_bytes(target)?)?;
            return self.created(dir, &export, &rel);
        }
        let lrel = link_rel(&dir_rel, name)?;
        if confine::lstat(&export.root, &lrel).is_ok() {
            return Err(Error::Exist); // a real file is in the way
        }
        let mut f = confine::open_file(&export.root, &lrel, true, Some(true))?;
        let written = f.write_all(LINK_MAGIC).and_then(|()| f.write_all(target));
        drop(f);
        if let Err(e) = written {
            let _ = confine::remove(&export.root, &lrel, false);
            return Err(e.into());
        }
        self.created(dir, &export, &lrel)
    }

    /// Hard-link `file` as `name` in `dir`.
    pub fn link(&self, file: &Handle, dir: &Handle, name: &[u8]) -> Result<()> {
        if file.export != dir.export {
            return Err(Error::Xdev);
        }
        let src = self.resolve(file)?;
        if src.md.kind == Type::Dir {
            return Err(Error::IsDir);
        }
        let (export, dir_rel, rel, existing) = self.child(dir, name)?;
        if existing.is_some() {
            return Err(Error::Exist);
        }
        // A link to an emulated symlink is an emulated symlink too.
        let rel = if src.md.kind == Type::Symlink && Self::is_emulated(&export, &src.rel) { link_rel(&dir_rel, name)? } else { rel };
        confine::hard_link(&export.root, &src.rel, &rel)?;
        export.intern(&rel);
        Ok(())
    }

    pub fn remove(&self, dir: &Handle, name: &[u8]) -> Result<()> {
        let (export, _, rel, existing) = self.child(dir, name)?;
        match existing {
            None => return Err(Error::NoEnt),
            Some(m) if m.kind == Type::Dir => return Err(Error::IsDir),
            Some(_) => {}
        }
        confine::remove(&export.root, &rel, false)?;
        export.ids.lock().unwrap().forget(&rel);
        Ok(())
    }

    pub fn rmdir(&self, dir: &Handle, name: &[u8]) -> Result<()> {
        let (export, _, rel, existing) = self.child(dir, name)?;
        match existing {
            None => return Err(Error::NoEnt),
            Some(m) if m.kind != Type::Dir => return Err(Error::NotDir),
            Some(_) => {}
        }
        confine::remove(&export.root, &rel, true)?;
        export.ids.lock().unwrap().forget(&rel);
        Ok(())
    }

    /// Rename, keeping the moved object's fileid (and those of everything
    /// under it) so clients' handles stay valid.
    pub fn rename(&self, from_dir: &Handle, from: &[u8], to_dir: &Handle, to: &[u8]) -> Result<()> {
        if from_dir.export != to_dir.export {
            return Err(Error::Xdev);
        }
        let (export, _, from_rel, from_md) = self.child(from_dir, from)?;
        let Some(from_md) = from_md else { return Err(Error::NoEnt) };
        let (_, to_dir_rel, to_plain, to_md) = self.child(to_dir, to)?;
        // An emulated link keeps its `symlink-` host name under the new name.
        let emulated = from_md.kind == Type::Symlink && Self::is_emulated(&export, &from_rel);
        let to_rel = if emulated { link_rel(&to_dir_rel, to)? } else { to_dir_rel.join(component(to)?) };
        if to_rel.starts_with(&from_rel) && to_rel != from_rel {
            return Err(Error::Inval); // a directory into its own subtree
        }
        // The target name may exist in the other form (a file where a link
        // goes, or the reverse). Replace it, as rename does.
        if let Some(m) = to_md
            && to_plain != to_rel
            && to_plain != from_rel
        {
            if m.kind == Type::Dir {
                return Err(Error::Exist);
            }
            confine::remove(&export.root, &to_plain, false)?;
            export.ids.lock().unwrap().forget(&to_plain);
        }
        confine::rename(&export.root, &from_rel, &to_rel)?;
        export.ids.lock().unwrap().rename(&from_rel, &to_rel);
        Ok(())
    }
}

#[cfg(unix)]
fn disk_space(path: &Path) -> Result<FsStat> {
    use std::os::unix::ffi::OsStrExt;
    let p = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| Error::Inval)?;
    // SAFETY: statvfs reads the path and writes only into the struct we pass.
    let s = unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(p.as_ptr(), &mut s) != 0 {
            return Err(io::Error::last_os_error().into());
        }
        s
    };
    let frsize = if s.f_frsize > 0 { s.f_frsize as u64 } else { s.f_bsize as u64 };
    Ok(FsStat {
        total_bytes: s.f_blocks as u64 * frsize,
        free_bytes: s.f_bfree as u64 * frsize,
        avail_bytes: s.f_bavail as u64 * frsize,
        total_files: s.f_files as u64,
        free_files: s.f_ffree as u64,
        avail_files: s.f_favail as u64,
    })
}

/// No portable free-space query outside Unix yet: report a roomy disk.
#[cfg(not(unix))]
fn disk_space(_path: &Path) -> Result<FsStat> {
    let (bytes, files) = (1u64 << 40, 1u64 << 24);
    Ok(FsStat {
        total_bytes: bytes,
        free_bytes: bytes / 2,
        avail_bytes: bytes / 2,
        total_files: files,
        free_files: files / 2,
        avail_files: files / 2,
    })
}

/// Host path of the emulated link for `name` in `dir_rel`.
fn link_rel(dir_rel: &Path, name: &[u8]) -> Result<PathBuf> {
    let mut n = LINK_PREFIX.to_vec();
    n.extend_from_slice(name);
    Ok(dir_rel.join(component(&n)?))
}

/// The target stored in an emulated link, if `rel` is one.
fn read_emulated(root: &Path, rel: &Path) -> Option<Vec<u8>> {
    let f = confine::open_file(root, rel, false, None).ok()?;
    let mut buf = Vec::new();
    f.take(LINK_MAGIC.len() as u64 + LINK_MAX + 1).read_to_end(&mut buf).ok()?;
    let target = buf.strip_prefix(LINK_MAGIC)?;
    (!target.is_empty() && target.len() as u64 <= LINK_MAX && !target.contains(&0)).then(|| target.to_vec())
}

fn writable(export: &Export) -> Result<()> {
    if export.read_only() { Err(Error::RoFs) } else { Ok(()) }
}

/// Apply size and time changes to `rel` (`md` is its current attributes).
fn apply(root: &Path, rel: &Path, md: &Meta, sa: &SetAttr) -> Result<()> {
    if md.kind == Type::Symlink {
        return Ok(()); // nothing to change on the link itself, portably
    }
    if let Some(size) = sa.size {
        if md.kind == Type::Dir {
            return Err(Error::IsDir);
        }
        confine::truncate(root, rel, size)?;
    }
    if sa.atime != SetTime::DontChange || sa.mtime != SetTime::DontChange {
        confine::set_times(root, rel, sa.atime, sa.mtime)?;
    }
    Ok(())
}

/// Export names are absolute, slash-separated, without `.`/`..` parts.
fn normalize_export_name(name: &str) -> Option<String> {
    let parts: Vec<&str> = name.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| *p == "." || *p == "..") {
        return None;
    }
    Some(format!("/{}", parts.join("/")))
}

/// Validate one name from the wire as a single, normal path component.
fn component(name: &[u8]) -> Result<PathBuf> {
    if name.len() > NAME_MAX {
        return Err(Error::NameTooLong);
    }
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0) {
        return Err(Error::Inval);
    }
    if cfg!(windows) && !windows_safe(name) {
        return Err(Error::Inval);
    }
    let p = PathBuf::from(os_string_from_bytes(name)?);
    let mut comps = p.components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => Ok(p),
        _ => Err(Error::Inval),
    }
}

/// Names Windows would read as something other than a file in this folder:
/// path separators, `:` (drive letters, alternate data streams), trailing
/// dots and spaces (silently stripped), and the reserved device names, which
/// open a device whatever folder or extension they're given (`NUL.txt`).
fn windows_safe(name: &[u8]) -> bool {
    if name.iter().any(|&b| matches!(b, b'\\' | b':' | b'*' | b'?' | b'"' | b'<' | b'>' | b'|') || b < 0x20) {
        return false;
    }
    if matches!(name.last(), Some(b'.' | b' ')) {
        return false;
    }
    let stem = name.split(|&b| b == b'.').next().unwrap_or(name);
    let stem = String::from_utf8_lossy(stem).trim_end().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'0'..=b'9' | 0xB9 | 0xB2 | 0xB3));
    !reserved
}

/// Bytes → OS string. On Unix names are arbitrary bytes; elsewhere they must
/// be UTF-8.
fn os_string_from_bytes(b: &[u8]) -> Result<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(std::ffi::OsStr::from_bytes(b).to_os_string())
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(b).map(OsString::from).map_err(|_| Error::Inval)
    }
}

/// OS string → bytes (inverse of `os_string_from_bytes`).
fn name_bytes(name: OsString) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        name.into_vec()
    }
    #[cfg(not(unix))]
    {
        name.to_string_lossy().into_owned().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn temp_dir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!("bsnfs_vfs_{}_{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn vfs(root: &Path) -> Vfs {
        Vfs::new(vec![("/share".into(), root.to_path_buf(), false)]).unwrap()
    }

    #[test]
    fn root_is_inode_2_and_ids_never_0_or_1() {
        let root = temp_dir();
        fs::write(root.join("f"), b"x").unwrap();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        assert_eq!(r.id, ROOT_ID);
        let a = v.getattr(&r).unwrap();
        assert_eq!((a.kind, a.fileid, a.mode, a.uid, a.nlink), (Kind::Dir, 2, 0o755, 0, 2), "no subdirectories yet");
        let (f, fa) = v.lookup(&r, b"f").unwrap();
        assert_eq!(f.id, 3, "ids are handed out from 3");
        assert_eq!((fa.kind, fa.mode, fa.nlink, fa.ctime), (Kind::Reg, 0o644, 1, fa.mtime));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn directory_nlink_counts_subdirectories() {
        let root = temp_dir();
        fs::create_dir_all(root.join("a/x")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        fs::write(root.join("file"), b"").unwrap();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        assert_eq!(v.getattr(&r).unwrap().nlink, 4, "2 + subdirs a and b; files don't count");
        let a = v.lookup(&r, b"a").unwrap();
        assert_eq!(a.1.nlink, 3);
        // A new subdirectory is counted (the parent's mtime moves on).
        std::thread::sleep(Duration::from_millis(20));
        v.mkdir(&r, b"c", &SetAttr::default()).unwrap();
        assert_eq!(v.getattr(&r).unwrap().nlink, 5);
        // Listings agree with GETATTR.
        let l = v.readdir(&r, true).unwrap();
        assert_eq!(l.entries.iter().find(|e| e.name == b"a").unwrap().attr.nlink, 3);
        assert_eq!(l.entries[0].attr.nlink, 5, "`.`");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn free_space_is_the_hosts() {
        let root = temp_dir();
        let v = vfs(&root);
        let s = v.fsstat(&v.root(0).unwrap()).unwrap();
        assert!(s.total_bytes > 0 && s.avail_bytes <= s.total_bytes && s.free_bytes <= s.total_bytes);
        #[cfg(unix)]
        assert_ne!(s.total_bytes, 1 << 40, "not the made-up figure");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn handles_encode_and_go_stale_across_runs() {
        let root = temp_dir();
        fs::write(root.join("f"), b"x").unwrap();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        let f = v.lookup(&r, b"f").unwrap().0;
        assert_eq!(v.decode(&v.encode(f)).unwrap(), f);
        let mut v2 = [0u8; 32];
        v2[..FH_LEN].copy_from_slice(&v.encode(f));
        assert_eq!(v.decode(&v2).unwrap(), f, "NFSv2's padded handle");
        assert_eq!(v.decode(&[0u8; 16]), Err(Error::BadHandle));
        assert_eq!(v.decode(&[1, 2, 3]), Err(Error::BadHandle));

        // A restarted server: the root handle still works; other ids are
        // stale rather than silently naming some other file.
        let restarted = Vfs { generation: v.generation ^ 0x55, ..vfs(&root) };
        assert_eq!(restarted.decode(&v.encode(r)).unwrap(), r);
        assert_eq!(restarted.decode(&v.encode(f)), Err(Error::Stale));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn create_write_read() {
        let root = temp_dir();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        let (f, a) = v.create(&r, b"hello.txt", CreateMode::Guarded, &SetAttr::default()).unwrap();
        assert_eq!(a.kind, Kind::Reg);
        v.write(&f, 0, b"hello world", false).unwrap();
        assert_eq!(v.read(&f, 6, 100).unwrap(), (b"world".to_vec(), true));
        assert_eq!(v.read(&f, 0, 5).unwrap(), (b"hello".to_vec(), false));
        assert_eq!(v.lookup(&r, b"hello.txt").unwrap().0, f, "stable id");
        assert_eq!(v.create(&r, b"hello.txt", CreateMode::Guarded, &SetAttr::default()).unwrap_err(), Error::Exist);
        assert_eq!(v.lookup(&r, b"nope").unwrap_err(), Error::NoEnt);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn setattr_applies_size_and_times_and_ignores_mode() {
        let root = temp_dir();
        fs::write(root.join("f"), b"0123456789").unwrap();
        let v = vfs(&root);
        let f = v.lookup(&v.root(0).unwrap(), b"f").unwrap().0;
        let t = Time { secs: 1_000_000_000, nsecs: 0 };
        let sa = SetAttr { mode: Some(0o600), uid: Some(42), size: Some(4), mtime: SetTime::Client(t), ..Default::default() };
        v.setattr(&f, &sa).unwrap();
        let a = v.getattr(&f).unwrap();
        assert_eq!((a.size, a.mtime, a.mode, a.uid), (4, t, 0o644, 0));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn dot_and_dotdot_stay_inside_the_export() {
        let root = temp_dir();
        fs::create_dir_all(root.join("a/b")).unwrap();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        let a = v.lookup(&r, b"a").unwrap().0;
        let b = v.lookup(&a, b"b").unwrap().0;
        assert_eq!(v.lookup(&b, b".").unwrap().0, b);
        assert_eq!(v.lookup(&b, b"..").unwrap().0, a);
        assert_eq!(v.lookup(&r, b"..").unwrap().0, r);
        assert!(v.lookup(&r, b"../etc").is_err());
        assert_eq!(v.mkdir(&r, b"..", &SetAttr::default()).unwrap_err(), Error::Inval);
        // `..` in a listing carries the same id LOOKUP gives; getcwd relies on it.
        let l = v.readdir(&b, true).unwrap();
        let names: Vec<&[u8]> = l.entries.iter().map(|e| &e.name[..]).collect();
        assert_eq!(names, [&b"."[..], b".."]);
        assert_eq!(l.entries[1].handle, a);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rename_keeps_ids_and_removal_makes_them_stale() {
        let root = temp_dir();
        fs::create_dir_all(root.join("d/sub")).unwrap();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        let d = v.lookup(&r, b"d").unwrap().0;
        let sub = v.lookup(&d, b"sub").unwrap().0;
        v.rename(&r, b"d", &r, b"e").unwrap();
        assert_eq!(v.lookup(&r, b"e").unwrap().0, d);
        assert_eq!(v.lookup(&d, b"sub").unwrap().0, sub, "descendants keep their ids");
        assert_eq!(v.rename(&r, b"e", &sub, b"loop").unwrap_err(), Error::Inval);
        v.rmdir(&d, b"sub").unwrap();
        assert_eq!(v.getattr(&sub).unwrap_err(), Error::Stale);
        // Deleted behind our back on the host: also stale.
        fs::remove_dir(root.join("e")).unwrap();
        assert_eq!(v.getattr(&d).unwrap_err(), Error::Stale);
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_served_but_not_followed() {
        let root = temp_dir();
        let outside = temp_dir();
        fs::write(outside.join("secret"), b"nope").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("filelink")).unwrap();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        let (esc, a) = v.lookup(&r, b"escape").unwrap();
        assert_eq!(a.kind, Kind::Lnk);
        assert_eq!(v.lookup(&esc, b"secret").unwrap_err(), Error::NotDir);
        let fl = v.lookup(&r, b"filelink").unwrap().0;
        assert!(v.read(&fl, 0, 10).is_err());
        assert!(v.write(&fl, 0, b"x", false).is_err());
        assert_eq!(v.create(&r, b"filelink", CreateMode::Unchecked, &SetAttr::default()).unwrap_err(), Error::Exist);
        assert_eq!(v.readlink(&fl).unwrap(), name_bytes(outside.join("secret").into_os_string()));
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"nope");
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    /// The race a client can stage: get a handle to `d/f`, then swap `d` for
    /// a symlink to somewhere outside. The handle must not follow it.
    #[cfg(unix)]
    #[test]
    fn swapped_in_symlink_does_not_redirect_a_handle() {
        let root = temp_dir();
        let outside = temp_dir();
        fs::create_dir(root.join("d")).unwrap();
        fs::write(root.join("d/f"), b"inside").unwrap();
        fs::write(outside.join("f"), b"outside").unwrap();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        let d = v.lookup(&r, b"d").unwrap().0;
        let f = v.lookup(&d, b"f").unwrap().0;
        fs::rename(root.join("d"), root.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("d")).unwrap();
        assert!(v.read(&f, 0, 100).is_err());
        assert!(v.write(&f, 0, b"pwned", false).is_err());
        assert!(v.setattr(&f, &SetAttr { size: Some(0), ..Default::default() }).is_err());
        assert!(v.create(&d, b"new", CreateMode::Unchecked, &SetAttr::default()).is_err());
        assert!(v.remove(&d, b"f").is_err());
        assert!(v.readdir(&d, true).is_err());
        assert_eq!(fs::read(outside.join("f")).unwrap(), b"outside");
        assert!(!outside.join("new").exists());
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn windows_device_names_are_refused() {
        for bad in [&b"CON"[..], b"nul.txt", b"Com1", b"LPT9.log", b"a:b", b"x\\y", b"trailing.", b"space ", b"q?"] {
            assert!(!windows_safe(bad), "{}", String::from_utf8_lossy(bad));
        }
        for ok in [&b"console"[..], b"COM10", b"nullable.txt", b"readme.txt", b".profile"] {
            assert!(windows_safe(ok), "{}", String::from_utf8_lossy(ok));
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_creation_can_be_refused() {
        let root = temp_dir();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        v.set_symlinks(false);
        assert_eq!(v.symlink(&r, b"l", b"/etc").unwrap_err(), Error::Acces);
        v.set_symlinks(true);
        v.symlink(&r, b"l", b"/etc").unwrap();
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn emulated_symlinks() {
        let root = temp_dir();
        fs::create_dir(root.join("d")).unwrap();
        // A real file that merely has the prefix stays a file.
        fs::write(root.join("symlink-notes.txt"), b"just notes").unwrap();
        let v = vfs(&root);
        v.set_emulate_symlinks(true);
        let r = v.root(0).unwrap();
        let d = v.lookup(&r, b"d").unwrap().0;

        let (l, a) = v.symlink(&d, b"lib", b"../usr/lib/libc.so").unwrap();
        assert_eq!((a.kind, a.size), (Kind::Lnk, 18));
        assert_eq!(fs::read(root.join("d/symlink-lib")).unwrap(), b"!<bsnfs-symlink>\n../usr/lib/libc.so");
        assert_eq!(v.readlink(&l).unwrap(), b"../usr/lib/libc.so");
        assert_eq!(v.lookup(&d, b"lib").unwrap(), (l, v.getattr(&l).unwrap()));
        assert!(v.read(&l, 0, 10).is_err(), "never opened through");
        assert!(v.write(&l, 0, b"x", false).is_err());
        assert_eq!(v.symlink(&d, b"lib", b"x").unwrap_err(), Error::Exist);
        assert_eq!(v.create(&d, b"lib", CreateMode::Guarded, &SetAttr::default()).unwrap_err(), Error::Exist);
        assert_eq!(v.mkdir(&d, b"lib", &SetAttr::default()).unwrap_err(), Error::Exist);

        let names = |h: &Handle| -> Vec<(Vec<u8>, Kind)> {
            v.readdir(h, true).unwrap().entries.iter().skip(2).map(|e| (e.name.clone(), e.attr.kind)).collect()
        };
        assert_eq!(names(&d), [(b"lib".to_vec(), Kind::Lnk)]);
        assert_eq!(names(&r), [(b"d".to_vec(), Kind::Dir), (b"symlink-notes.txt".to_vec(), Kind::Reg)]);

        // Rename keeps it a link under the new name, and the same handle.
        v.rename(&d, b"lib", &r, b"lib2").unwrap();
        assert!(root.join("symlink-lib2").exists() && !root.join("d/symlink-lib").exists());
        assert_eq!(v.lookup(&r, b"lib2").unwrap().0, l);
        assert_eq!(v.readlink(&l).unwrap(), b"../usr/lib/libc.so");
        // A hard link to it is a link too.
        v.link(&l, &d, b"again").unwrap();
        assert_eq!(v.lookup(&d, b"again").unwrap().1.kind, Kind::Lnk);
        // Renaming a file onto the link's name replaces the link.
        v.create(&r, b"plain", CreateMode::Guarded, &SetAttr::default()).unwrap();
        v.rename(&r, b"plain", &r, b"lib2").unwrap();
        assert_eq!(v.lookup(&r, b"lib2").unwrap().1.kind, Kind::Reg);
        assert!(!root.join("symlink-lib2").exists());
        v.remove(&d, b"again").unwrap();
        assert!(!root.join("d/symlink-again").exists());
        assert_eq!(v.lookup(&d, b"again").unwrap_err(), Error::NoEnt);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mount_paths() {
        let root = temp_dir();
        fs::create_dir_all(root.join("sub/deeper")).unwrap();
        let v = vfs(&root);
        let r = v.root(0).unwrap();
        assert_eq!(v.mount(b"/share").unwrap().1, r);
        assert_eq!(v.mount(b"/share/").unwrap().1, r);
        assert_eq!(v.mount(b"/").unwrap().1, r);
        assert_eq!(v.mount(fs::canonicalize(&root).unwrap().to_string_lossy().as_bytes()).unwrap().1, r);
        assert_ne!(v.mount(b"/share/sub/deeper").unwrap().1, r);
        assert_eq!(v.mount(b"/sub").unwrap().1, v.lookup(&r, b"sub").unwrap().0);
        assert_eq!(v.mount(b"/share/../etc").unwrap_err(), Error::NoEnt);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn read_only_export_refuses_changes_and_shows_no_write_bits() {
        let root = temp_dir();
        fs::write(root.join("f"), b"x").unwrap();
        let v = Vfs::new(vec![("/ro".into(), root.clone(), true)]).unwrap();
        let r = v.root(0).unwrap();
        let (f, a) = v.lookup(&r, b"f").unwrap();
        assert_eq!(a.mode, 0o444);
        assert_eq!(v.getattr(&r).unwrap().mode, 0o555);
        assert_eq!(v.write(&f, 0, b"y", false).unwrap_err(), Error::RoFs);
        assert_eq!(v.remove(&r, b"f").unwrap_err(), Error::RoFs);
        assert_eq!(v.mkdir(&r, b"d", &SetAttr::default()).unwrap_err(), Error::RoFs);
        assert_eq!(v.read(&f, 0, 1).unwrap().0, b"x");
        assert!(v.set_read_only(0, false));
        v.write(&f, 0, b"y", false).unwrap();
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn exports_can_be_added_and_removed_at_runtime() {
        let a = temp_dir();
        let b = temp_dir();
        let v = vfs(&a);
        assert!(v.add_export("/share", &b, false).is_err(), "duplicate name");
        let i = v.add_export("other//", &b, false).unwrap();
        assert_eq!(v.exports()[1].1.name, "/other");
        let r = v.root(i).unwrap();
        assert_eq!(r.id, ROOT_ID, "each export is its own filesystem");
        assert_ne!(v.getattr(&r).unwrap().fsid, v.getattr(&v.root(0).unwrap()).unwrap().fsid);
        assert_eq!(v.mount(b"/other").unwrap().1, r);
        assert!(v.remove_export(i));
        assert_eq!(v.getattr(&r).unwrap_err(), Error::Stale);
        assert!(v.root(i).is_none());
        fs::remove_dir_all(&a).ok();
        fs::remove_dir_all(&b).ok();
    }
}
