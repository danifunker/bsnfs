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
//! clamped at the export root, and symlinks inside the share are served as
//! symlinks (the client resolves them) and never followed on the host.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    fn from_system(t: Option<SystemTime>) -> Self {
        t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| Self { secs: d.as_secs().min(u32::MAX as u64) as u32, nsecs: d.subsec_nanos() })
            .unwrap_or_default()
    }

    fn to_system(self) -> SystemTime {
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
    subdirs: Mutex<HashMap<u64, (SystemTime, u32)>>,
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
    fn subdirs(&self, id: u64, rel: &Path, md: &Metadata) -> u32 {
        let mtime = md.modified().ok();
        if let Some(&(t, n)) = self.subdirs.lock().unwrap().get(&id)
            && Some(t) == mtime
        {
            return n;
        }
        let n = fs::read_dir(self.root.join(rel))
            .map(|rd| rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).count())
            .unwrap_or(0)
            .min(u32::MAX as usize - 2) as u32;
        if let Some(t) = mtime {
            self.subdirs.lock().unwrap().insert(id, (t, n));
        }
        n
    }

    /// Synthetic attributes for the object `id` at `rel`, whose host metadata
    /// is `md`.
    fn attr(&self, id: u64, rel: &Path, md: &Metadata) -> Attr {
        let ft = md.file_type();
        let (kind, mode) = if ft.is_dir() {
            (Kind::Dir, 0o755)
        } else if ft.is_symlink() {
            (Kind::Lnk, 0o777)
        } else {
            (Kind::Reg, if host_executable(md) { 0o755 } else { 0o644 })
        };
        let mode = if self.read_only() && kind != Kind::Lnk { mode & !0o222 } else { mode };
        let mtime = Time::from_system(md.modified().ok());
        Attr {
            kind,
            mode,
            // A directory's own `.` and its entry in the parent, plus each
            // subdirectory's `..`. Old `find`/`fts` read nlink == 2 as "no
            // subdirectories" and stop descending, so this must be real.
            nlink: if kind == Kind::Dir { 2 + self.subdirs(id, rel, md) } else { 1 },
            uid: 0,
            gid: 0,
            size: md.len(),
            fsid: self.fsid,
            fileid: id,
            atime: Time::from_system(md.accessed().ok()),
            mtime,
            ctime: mtime, // no portable ctime; reuse mtime
        }
    }
}

/// A resolved handle: its export, where it lives, and its current metadata.
struct Node {
    export: Arc<Export>,
    rel: PathBuf,
    abs: PathBuf,
    md: Metadata,
}

const LISTING_CACHE: usize = 64;

pub struct Vfs {
    /// Indexed by the handle's export byte. Removed exports leave a `None`
    /// so the remaining handles keep their meaning.
    exports: RwLock<Vec<Option<Arc<Export>>>>,
    /// Stamped into every non-root handle; changes on each start.
    generation: u32,
    listings: Mutex<HashMap<Handle, Arc<DirListing>>>,
}

impl Vfs {
    /// `exports` is `(name, host path, read_only)`.
    pub fn new(exports: Vec<(String, PathBuf, bool)>) -> io::Result<Self> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let generation = (now.subsec_nanos() ^ (now.as_secs() as u32).rotate_left(16) ^ std::process::id()) | 1;
        let vfs = Self { exports: RwLock::new(Vec::new()), generation, listings: Mutex::new(HashMap::new()) };
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
        let abs = export.root.join(&rel);
        // Gone from the host since we handed out the handle: stale.
        let md = fs::symlink_metadata(&abs).map_err(|_| Error::Stale)?;
        Ok(Node { export, rel, abs, md })
    }

    /// Resolve a directory handle.
    fn resolve_dir(&self, h: &Handle) -> Result<Node> {
        let node = self.resolve(h)?;
        if !node.md.is_dir() {
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
        let rel = match name {
            b"." => node.rel,
            b".." => node.rel.parent().map(Path::to_path_buf).unwrap_or_default(),
            _ => node.rel.join(component(name)?),
        };
        let md = fs::symlink_metadata(node.export.root.join(&rel))?;
        let id = node.export.intern(&rel);
        Ok((Handle { export: dir.export, id }, node.export.attr(id, &rel, &md)))
    }

    pub fn readlink(&self, h: &Handle) -> Result<Vec<u8>> {
        let node = self.resolve(h)?;
        if !node.md.file_type().is_symlink() {
            return Err(Error::Inval);
        }
        Ok(name_bytes(fs::read_link(&node.abs)?.into_os_string()))
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
        let entry = |name: Vec<u8>, rel: &Path, md: &Metadata| {
            let id = export.intern(rel);
            DirEntry { name, handle: Handle { export: dir.export, id }, attr: export.attr(id, rel, md) }
        };
        let mut entries = Vec::new();
        let parent = node.rel.parent().map(Path::to_path_buf).unwrap_or_default();
        for (name, rel) in [(&b"."[..], &node.rel), (&b".."[..], &parent)] {
            if let Ok(md) = fs::symlink_metadata(export.root.join(rel)) {
                entries.push(entry(name.to_vec(), rel, &md));
            }
        }
        let mut children = Vec::new();
        for ent in fs::read_dir(&node.abs)? {
            let ent = ent?;
            let name = name_bytes(ent.file_name());
            // Skip anything that wouldn't round-trip as a safe component.
            if component(&name).is_err() {
                continue;
            }
            let Ok(md) = fs::symlink_metadata(ent.path()) else { continue }; // vanished meanwhile
            children.push((name, ent.file_name(), md));
        }
        // Sorted, so a re-read after cache eviction keeps cookie positions.
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, os_name, md) in children {
            entries.push(entry(name, &node.rel.join(os_name), &md));
        }
        let mtime = Time::from_system(node.md.modified().ok());
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
        disk_space(&self.resolve(h)?.abs)
    }

    // ── file data ───────────────────────────────────────────────────────────

    fn open_file(node: &Node, write: bool) -> Result<File> {
        if node.md.is_dir() {
            return Err(Error::IsDir);
        }
        if !node.md.is_file() {
            return Err(Error::Inval); // a symlink: never followed on the host
        }
        Ok(OpenOptions::new().read(!write).write(write).open(&node.abs)?)
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
        if node.md.is_file() && !node.export.read_only() {
            Self::open_file(&node, true)?.sync_all()?;
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
        apply(&node.abs, &node.md, sa)
    }

    // ── namespace changes ───────────────────────────────────────────────────

    /// Resolve a writable directory and the export-relative path of `name`.
    fn child(&self, dir: &Handle, name: &[u8]) -> Result<(Arc<Export>, PathBuf)> {
        let node = self.resolve_dir(dir)?;
        writable(&node.export)?;
        let rel = node.rel.join(component(name)?);
        Ok((node.export, rel))
    }

    /// Intern a newly created object and return its handle and attributes.
    fn created(&self, dir: &Handle, export: &Export, rel: &Path) -> Result<(Handle, Attr)> {
        let md = fs::symlink_metadata(export.root.join(rel))?;
        let id = export.intern(rel);
        Ok((Handle { export: dir.export, id }, export.attr(id, rel, &md)))
    }

    pub fn create(&self, dir: &Handle, name: &[u8], how: CreateMode, sa: &SetAttr) -> Result<(Handle, Attr)> {
        let (export, rel) = self.child(dir, name)?;
        let abs = export.root.join(&rel);
        // Never open through a symlink, or onto a directory.
        if let Ok(md) = fs::symlink_metadata(&abs)
            && !md.is_file()
        {
            return Err(if how == CreateMode::Guarded || !md.is_dir() { Error::Exist } else { Error::IsDir });
        }
        let mut o = OpenOptions::new();
        o.write(true);
        match how {
            CreateMode::Unchecked => o.create(true),
            CreateMode::Guarded => o.create_new(true),
        };
        let f = o.open(&abs)?;
        apply(&abs, &f.metadata()?, sa)?;
        self.created(dir, &export, &rel)
    }

    pub fn mkdir(&self, dir: &Handle, name: &[u8], sa: &SetAttr) -> Result<(Handle, Attr)> {
        let (export, rel) = self.child(dir, name)?;
        let abs = export.root.join(&rel);
        fs::create_dir(&abs)?;
        apply(&abs, &fs::symlink_metadata(&abs)?, &SetAttr { size: None, ..sa.clone() })?;
        self.created(dir, &export, &rel)
    }

    pub fn symlink(&self, dir: &Handle, name: &[u8], target: &[u8]) -> Result<(Handle, Attr)> {
        let (export, rel) = self.child(dir, name)?;
        if target.is_empty() || target.contains(&0) {
            return Err(Error::Inval);
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(os_string_from_bytes(target)?, export.root.join(&rel))?;
            self.created(dir, &export, &rel)
        }
        #[cfg(not(unix))]
        {
            let _ = (export, rel);
            Err(Error::NotSupp)
        }
    }

    /// Hard-link `file` as `name` in `dir`.
    pub fn link(&self, file: &Handle, dir: &Handle, name: &[u8]) -> Result<()> {
        if file.export != dir.export {
            return Err(Error::Xdev);
        }
        let src = self.resolve(file)?;
        if src.md.is_dir() {
            return Err(Error::IsDir);
        }
        let (export, rel) = self.child(dir, name)?;
        fs::hard_link(&src.abs, export.root.join(&rel))?;
        export.intern(&rel);
        Ok(())
    }

    pub fn remove(&self, dir: &Handle, name: &[u8]) -> Result<()> {
        let (export, rel) = self.child(dir, name)?;
        let abs = export.root.join(&rel);
        if fs::symlink_metadata(&abs)?.is_dir() {
            return Err(Error::IsDir);
        }
        fs::remove_file(&abs)?;
        export.ids.lock().unwrap().forget(&rel);
        Ok(())
    }

    pub fn rmdir(&self, dir: &Handle, name: &[u8]) -> Result<()> {
        let (export, rel) = self.child(dir, name)?;
        let abs = export.root.join(&rel);
        if !fs::symlink_metadata(&abs)?.is_dir() {
            return Err(Error::NotDir);
        }
        fs::remove_dir(&abs)?;
        export.ids.lock().unwrap().forget(&rel);
        Ok(())
    }

    /// Rename, keeping the moved object's fileid (and those of everything
    /// under it) so clients' handles stay valid.
    pub fn rename(&self, from_dir: &Handle, from: &[u8], to_dir: &Handle, to: &[u8]) -> Result<()> {
        if from_dir.export != to_dir.export {
            return Err(Error::Xdev);
        }
        let (export, from_rel) = self.child(from_dir, from)?;
        let (_, to_rel) = self.child(to_dir, to)?;
        if to_rel.starts_with(&from_rel) && to_rel != from_rel {
            return Err(Error::Inval); // a directory into its own subtree
        }
        fs::rename(export.root.join(&from_rel), export.root.join(&to_rel))?;
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

fn writable(export: &Export) -> Result<()> {
    if export.read_only() { Err(Error::RoFs) } else { Ok(()) }
}

/// Apply size and time changes to the object at `abs` (`md` is its lstat).
fn apply(abs: &Path, md: &Metadata, sa: &SetAttr) -> Result<()> {
    if md.file_type().is_symlink() {
        return Ok(()); // nothing to change on the link itself, portably
    }
    if let Some(size) = sa.size {
        if md.is_dir() {
            return Err(Error::IsDir);
        }
        OpenOptions::new().write(true).open(abs)?.set_len(size)?;
    }
    let pick = |t: SetTime| match t {
        SetTime::DontChange => None,
        SetTime::ServerTime => Some(SystemTime::now()),
        SetTime::Client(t) => Some(t.to_system()),
    };
    let (atime, mtime) = (pick(sa.atime), pick(sa.mtime));
    if atime.is_none() && mtime.is_none() {
        return Ok(());
    }
    let mut times = FileTimes::new();
    if let Some(t) = atime {
        times = times.set_accessed(t);
    }
    if let Some(t) = mtime {
        times = times.set_modified(t);
    }
    let f = if md.is_dir() {
        // Opening a directory as a File works on Unix; elsewhere skip it.
        if cfg!(unix) { File::open(abs)? } else { return Ok(()) }
    } else {
        OpenOptions::new().write(true).open(abs)?
    };
    f.set_times(times)?;
    Ok(())
}

/// The execute bit is the one host permission passed through, so scripts and
/// binaries stay runnable. Elsewhere files are 0644.
fn host_executable(md: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        false
    }
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
    // Backslash and colon are separators (or worse) on Windows.
    if cfg!(windows) && name.iter().any(|&b| b == b'\\' || b == b':') {
        return Err(Error::Inval);
    }
    let p = PathBuf::from(os_string_from_bytes(name)?);
    let mut comps = p.components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => Ok(p),
        _ => Err(Error::Inval),
    }
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
