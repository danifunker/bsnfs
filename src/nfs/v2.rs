//! NFSv2 (RFC 1094) procedures: 32-bit sizes and offsets, fixed 32-byte
//! handles, microsecond times.

use super::{status2, Nfs};
use crate::rpc::{Ctx, Reject};
use crate::vfs::{self, Attr, CreateMode, Handle, Kind, SetAttr, SetTime, Time};
use crate::xdr::{opaque_size, Cur, Xdr};

const NAMES: [&str; 18] = [
    "NULL", "GETATTR", "SETATTR", "ROOT", "LOOKUP", "READLINK", "READ", "WRITECACHE", "WRITE", "CREATE", "REMOVE",
    "RENAME", "LINK", "SYMLINK", "MKDIR", "RMDIR", "READDIR", "STATFS",
];

pub fn proc_name(p: u32) -> &'static str {
    NAMES.get(p as usize).copied().unwrap_or("?")
}

pub fn idempotent(p: u32) -> bool {
    !matches!(p, 2 | 9..=15)
}

const NFS_OK: u32 = 0;
/// NFSv2 transfers are capped at 8 KiB by the protocol.
const MAX_DATA: u32 = 8192;
const FHSIZE: usize = 32;

pub fn call(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Result<(), Reject> {
    let r = match ctx.proc_num {
        0 | 3 | 7 => Some(()), // NULL, and the obsolete ROOT and WRITECACHE
        1 => getattr(n, ctx, a, o),
        2 => setattr(n, ctx, a, o),
        4 => lookup(n, ctx, a, o),
        5 => readlink(n, ctx, a, o),
        6 => read(n, ctx, a, o),
        8 => write(n, ctx, a, o),
        9 => create(n, ctx, a, o, false),
        10 => remove(n, ctx, a, o, false),
        11 => rename(n, ctx, a, o),
        12 => link(n, ctx, a, o),
        13 => symlink(n, ctx, a, o),
        14 => create(n, ctx, a, o, true),
        15 => remove(n, ctx, a, o, true),
        16 => readdir(n, ctx, a, o),
        17 => statfs(n, ctx, a, o),
        _ => return Err(Reject::ProcUnavail),
    };
    r.ok_or(Reject::GarbageArgs)
}

fn fh(n: &Nfs, a: &mut Cur) -> Option<vfs::Result<Handle>> {
    Some(n.vfs.decode(a.fixed(FHSIZE)?))
}

fn name<'a>(a: &mut Cur<'a>) -> Option<&'a [u8]> {
    a.opaque(1024)
}

fn put_fh(n: &Nfs, o: &mut Xdr, h: Handle) {
    let mut b = [0u8; FHSIZE];
    b[..vfs::FH_LEN].copy_from_slice(&n.vfs.encode(h));
    o.fixed(&b);
}

fn put_time(o: &mut Xdr, t: Time) {
    o.u32(t.secs);
    o.u32(t.nsecs / 1000);
}

/// Fold a 64-bit fileid into v2's 32 bits, never producing 0 or 1.
fn fileid32(id: u64) -> u32 {
    let f = (id ^ (id >> 32)) as u32;
    if f < 2 { f | 0x8000_0000 } else { f }
}

fn put_fattr(o: &mut Xdr, a: &Attr) {
    o.u32(match a.kind {
        Kind::Reg => 1,
        Kind::Dir => 2,
        Kind::Lnk => 5,
    });
    o.u32(a.type_bits() | a.mode);
    o.u32(a.nlink);
    o.u32(a.uid);
    o.u32(a.gid);
    o.u32(a.size.min(u32::MAX as u64) as u32);
    o.u32(4096); // blocksize
    o.u32(0); // rdev
    o.u32(a.size.div_ceil(512).min(u32::MAX as u64) as u32); // blocks
    o.u32(a.fsid as u32);
    o.u32(fileid32(a.fileid));
    put_time(o, a.atime);
    put_time(o, a.mtime);
    put_time(o, a.ctime);
}

fn status(ctx: &Ctx, o: &mut Xdr, e: vfs::Error) {
    trace!("  nfs2 {} -> {e:?}", proc_name(ctx.proc_num));
    o.u32(status2(e));
}

/// `attrstat`: status, then attributes on success.
fn attrstat(n: &Nfs, ctx: &Ctx, o: &mut Xdr, r: vfs::Result<Attr>) {
    match r {
        Ok(attr) => {
            o.u32(NFS_OK);
            put_fattr(o, &n.present(ctx, attr));
        }
        Err(e) => status(ctx, o, e),
    }
}

/// `diropres`: status, then handle and attributes on success.
fn diropres(n: &Nfs, ctx: &Ctx, o: &mut Xdr, r: vfs::Result<(Handle, Attr)>) {
    match r {
        Ok((h, attr)) => {
            o.u32(NFS_OK);
            put_fh(n, o, h);
            put_fattr(o, &n.present(ctx, attr));
        }
        Err(e) => status(ctx, o, e),
    }
}

fn stat_only(ctx: &Ctx, o: &mut Xdr, r: vfs::Result<()>) {
    match r {
        Ok(()) => o.u32(NFS_OK),
        Err(e) => status(ctx, o, e),
    }
}

fn sattr(a: &mut Cur) -> Option<SetAttr> {
    const UNSET: u32 = u32::MAX;
    let field = |v: u32| (v != UNSET).then_some(v);
    let mode = field(a.u32()?).map(|m| m & 0o7777);
    let uid = field(a.u32()?);
    let gid = field(a.u32()?);
    let size = field(a.u32()?).map(u64::from);
    let time = |a: &mut Cur| -> Option<SetTime> {
        let (secs, usecs) = (a.u32()?, a.u32()?);
        Some(match (secs, usecs) {
            (UNSET, _) => SetTime::DontChange,
            // Sun's convention for "use the server's clock".
            (_, 1_000_000) => SetTime::ServerTime,
            _ => SetTime::Client(Time { secs, nsecs: usecs.min(999_999) * 1000 }),
        })
    };
    let atime = time(a)?;
    let mtime = time(a)?;
    Some(SetAttr { mode, uid, gid, size, atime, mtime })
}

fn getattr(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    attrstat(n, ctx, o, h.and_then(|h| n.vfs.getattr(&h)));
    Some(())
}

fn setattr(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    let sa = sattr(a)?;
    attrstat(n, ctx, o, h.and_then(|h| n.vfs.setattr(&h, &sa).and_then(|()| n.vfs.getattr(&h))));
    Some(())
}

fn lookup(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    diropres(n, ctx, o, d.and_then(|d| n.vfs.lookup(&d, name)));
    Some(())
}

fn readlink(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    match h.and_then(|h| n.vfs.readlink(&h)) {
        Ok(target) => {
            o.u32(NFS_OK);
            o.opaque(&target);
        }
        Err(e) => status(ctx, o, e),
    }
    Some(())
}

fn read(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    let offset = a.u32()?;
    let count = a.u32()?.min(MAX_DATA);
    a.u32()?; // totalcount (unused by the protocol)
    let r = h.and_then(|h| {
        let (data, _eof) = n.vfs.read(&h, offset as u64, count)?;
        Ok((data, n.vfs.getattr(&h)?))
    });
    match r {
        Ok((data, attr)) => {
            o.u32(NFS_OK);
            put_fattr(o, &n.present(ctx, attr));
            o.opaque(&data);
        }
        Err(e) => status(ctx, o, e),
    }
    Some(())
}

fn write(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    a.u32()?; // beginoffset (unused)
    let offset = a.u32()?;
    a.u32()?; // totalcount (unused)
    let data = a.opaque(64 * 1024)?;
    // v2 writes are meant to be synchronous; we hand them to the host's page
    // cache like everything else and let it flush.
    attrstat(n, ctx, o, h.and_then(|h| n.vfs.write(&h, offset as u64, data, false).and_then(|()| n.vfs.getattr(&h))));
    Some(())
}

/// CREATE and MKDIR.
fn create(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr, dir: bool) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    let sa = sattr(a)?;
    let r = d.and_then(|d| if dir { n.vfs.mkdir(&d, name, &sa) } else { n.vfs.create(&d, name, CreateMode::Unchecked, &sa) });
    diropres(n, ctx, o, r);
    Some(())
}

fn remove(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr, dir: bool) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    stat_only(ctx, o, d.and_then(|d| if dir { n.vfs.rmdir(&d, name) } else { n.vfs.remove(&d, name) }));
    Some(())
}

fn rename(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let fd = fh(n, a)?;
    let fname = name(a)?;
    let td = fh(n, a)?;
    let tname = name(a)?;
    stat_only(ctx, o, fd.and_then(|fd| n.vfs.rename(&fd, fname, &td?, tname)));
    Some(())
}

fn link(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    let d = fh(n, a)?;
    let name = name(a)?;
    stat_only(ctx, o, h.and_then(|h| n.vfs.link(&h, &d?, name)));
    Some(())
}

fn symlink(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    let target = a.opaque(1024)?;
    sattr(a)?;
    stat_only(ctx, o, d.and_then(|d| n.vfs.symlink(&d, name, target).map(|_| ())));
    Some(())
}

/// READDIR. `count` bounds the whole result; at least one entry is always
/// returned so a tiny `count` still makes progress.
fn readdir(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let d = fh(n, a)?;
    let cookie = u32::from_be_bytes(a.fixed(4)?.try_into().ok()?) as usize;
    let count = a.u32()? as usize;
    let listing = match d.and_then(|d| n.vfs.readdir(&d, cookie == 0)) {
        Ok(l) => l,
        Err(e) => {
            status(ctx, o, e);
            return Some(());
        }
    };
    o.u32(NFS_OK);
    let mut size = 4 + 8;
    let mut i = cookie;
    while let Some(e) = listing.entries.get(i) {
        let len = 4 + 4 + opaque_size(e.name.len()) + 4;
        if i > cookie && size + len > count {
            break;
        }
        o.bool(true);
        o.u32(fileid32(e.attr.fileid));
        o.opaque(&e.name);
        o.u32(i as u32 + 1);
        size += len;
        i += 1;
    }
    o.bool(false);
    o.bool(i >= listing.entries.len());
    Some(())
}

fn statfs(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    match h.and_then(|h| n.vfs.fsstat(&h)) {
        Ok(s) => {
            // Grow the reported block size until the counts fit in 32 bits.
            let mut bsize = 4096u64;
            while s.total_bytes / bsize > u32::MAX as u64 {
                bsize *= 2;
            }
            o.u32(NFS_OK);
            o.u32(MAX_DATA); // tsize
            o.u32(bsize as u32);
            o.u32((s.total_bytes / bsize) as u32);
            o.u32((s.free_bytes / bsize) as u32);
            o.u32((s.avail_bytes / bsize) as u32);
        }
        Err(e) => status(ctx, o, e),
    }
    Some(())
}
