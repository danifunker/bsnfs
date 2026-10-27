//! NFSv3 (RFC 1813) procedures.

use super::{status3, Nfs};
use crate::rpc::{Ctx, Reject};
use crate::vfs::{self, Attr, CreateMode, Handle, Kind, SetAttr, SetTime, Time, FH_LEN};
use crate::xdr::{opaque_size, Cur, Xdr};

const NAMES: [&str; 22] = [
    "NULL", "GETATTR", "SETATTR", "LOOKUP", "ACCESS", "READLINK", "READ", "WRITE", "CREATE", "MKDIR", "SYMLINK",
    "MKNOD", "REMOVE", "RMDIR", "RENAME", "LINK", "READDIR", "READDIRPLUS", "FSSTAT", "FSINFO", "PATHCONF", "COMMIT",
];

pub fn proc_name(p: u32) -> &'static str {
    NAMES.get(p as usize).copied().unwrap_or("?")
}

/// Everything that changes the namespace or attributes. WRITE is left out:
/// re-applying the same bytes at the same offset is harmless, and caching
/// its replies would only evict the ones that matter.
pub fn idempotent(p: u32) -> bool {
    !matches!(p, 2 | 8..=15)
}

const NFS3_OK: u32 = 0;
const NFS3ERR_TOOSMALL: u32 = 10005;
const NFS3ERR_BADTYPE: u32 = 10007;

/// Encoded size of `fattr3`.
const FATTR3_SIZE: usize = 84;
/// RPC accepted-reply header that precedes our result body.
const REPLY_HEADER: usize = 24;

pub fn call(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Result<(), Reject> {
    let r = match ctx.proc_num {
        0 => Some(()),
        1 => getattr(n, ctx, a, o),
        2 => setattr(n, ctx, a, o),
        3 => lookup(n, ctx, a, o),
        4 => access(n, ctx, a, o),
        5 => readlink(n, ctx, a, o),
        6 => read(n, ctx, a, o),
        7 => write(n, ctx, a, o),
        8 => create(n, ctx, a, o),
        9 => mkdir(n, ctx, a, o),
        10 => symlink(n, ctx, a, o),
        11 => mknod(n, ctx, a, o),
        12 => remove(n, ctx, a, o, false),
        13 => remove(n, ctx, a, o, true),
        14 => rename(n, ctx, a, o),
        15 => link(n, ctx, a, o),
        16 => readdir(n, ctx, a, o, false),
        17 => readdir(n, ctx, a, o, true),
        18 => fsstat(n, ctx, a, o),
        19 => fsinfo(n, ctx, a, o),
        20 => pathconf(n, ctx, a, o),
        21 => commit(n, ctx, a, o),
        _ => return Err(Reject::ProcUnavail),
    };
    r.ok_or(Reject::GarbageArgs)
}

// ── encoding helpers ────────────────────────────────────────────────────────

fn fh(n: &Nfs, a: &mut Cur) -> Option<vfs::Result<Handle>> {
    Some(n.vfs.decode(a.opaque(64)?))
}

fn name<'a>(a: &mut Cur<'a>) -> Option<&'a [u8]> {
    a.opaque(1024)
}

fn put_fh(n: &Nfs, o: &mut Xdr, h: Handle) {
    o.opaque(&n.vfs.encode(h));
}

fn put_time(o: &mut Xdr, t: Time) {
    o.u32(t.secs);
    o.u32(t.nsecs);
}

fn ftype3(k: Kind) -> u32 {
    match k {
        Kind::Reg => 1,
        Kind::Dir => 2,
        Kind::Lnk => 5,
    }
}

fn put_fattr(o: &mut Xdr, a: &Attr) {
    o.u32(ftype3(a.kind));
    o.u32(a.mode);
    o.u32(a.nlink);
    o.u32(a.uid);
    o.u32(a.gid);
    o.u64(a.size);
    o.u64(a.size); // used
    o.u64(0); // rdev
    o.u64(a.fsid);
    o.u64(a.fileid);
    put_time(o, a.atime);
    put_time(o, a.mtime);
    put_time(o, a.ctime);
}

/// `post_op_attr` for `h`, fetched now.
fn post_op(n: &Nfs, ctx: &Ctx, o: &mut Xdr, h: Option<&Handle>) {
    match h.and_then(|h| n.vfs.getattr(h).ok()) {
        Some(a) => {
            o.bool(true);
            put_fattr(o, &n.present(ctx, a));
        }
        None => o.bool(false),
    }
}

/// Attributes before a change, for `wcc_data`.
fn pre(n: &Nfs, h: &vfs::Result<Handle>) -> Option<Attr> {
    h.as_ref().ok().and_then(|h| n.vfs.getattr(h).ok())
}

/// `wcc_data`: the pre-change size and times, then current attributes.
fn wcc(n: &Nfs, ctx: &Ctx, o: &mut Xdr, pre: Option<Attr>, h: &vfs::Result<Handle>) {
    match pre {
        Some(p) => {
            o.bool(true);
            o.u64(p.size);
            put_time(o, p.mtime);
            put_time(o, p.ctime);
        }
        None => o.bool(false),
    }
    post_op(n, ctx, o, h.as_ref().ok());
}

fn status(ctx: &Ctx, o: &mut Xdr, e: vfs::Error) {
    trace!("  nfs3 {} -> {e:?}", proc_name(ctx.proc_num));
    o.u32(status3(e));
}

fn set_time(a: &mut Cur) -> Option<SetTime> {
    Some(match a.u32()? {
        0 => SetTime::DontChange,
        1 => SetTime::ServerTime,
        2 => SetTime::Client(Time { secs: a.u32()?, nsecs: a.u32()? }),
        _ => return None,
    })
}

fn sattr(a: &mut Cur) -> Option<SetAttr> {
    let opt = |a: &mut Cur| -> Option<Option<u32>> { if a.bool()? { Some(Some(a.u32()?)) } else { Some(None) } };
    let mode = opt(a)?;
    let uid = opt(a)?;
    let gid = opt(a)?;
    let size = if a.bool()? { Some(a.u64()?) } else { None };
    Some(SetAttr { mode, uid, gid, size, atime: set_time(a)?, mtime: set_time(a)? })
}

/// Result of CREATE/MKDIR/SYMLINK/MKNOD: the new handle and attributes, then
/// the directory's wcc_data.
fn new_object(n: &Nfs, ctx: &Ctx, o: &mut Xdr, r: vfs::Result<(Handle, Attr)>, pre: Option<Attr>, dir: &vfs::Result<Handle>) {
    match r {
        Ok((h, attr)) => {
            o.u32(NFS3_OK);
            o.bool(true);
            put_fh(n, o, h);
            o.bool(true);
            put_fattr(o, &n.present(ctx, attr));
        }
        Err(e) => status(ctx, o, e),
    }
    wcc(n, ctx, o, pre, dir);
}

// ── procedures ──────────────────────────────────────────────────────────────

fn getattr(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    match h.and_then(|h| n.vfs.getattr(&h)) {
        Ok(attr) => {
            o.u32(NFS3_OK);
            put_fattr(o, &n.present(ctx, attr));
        }
        Err(e) => status(ctx, o, e),
    }
    Some(())
}

fn setattr(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    let sa = sattr(a)?;
    if a.bool()? {
        a.u64()?; // guard ctime; we don't check it
    }
    let pre = pre(n, &h);
    match h.and_then(|h| n.vfs.setattr(&h, &sa)) {
        Ok(()) => o.u32(NFS3_OK),
        Err(e) => status(ctx, o, e),
    }
    wcc(n, ctx, o, pre, &h);
    Some(())
}

fn lookup(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    match d.and_then(|d| n.vfs.lookup(&d, name)) {
        Ok((h, attr)) => {
            o.u32(NFS3_OK);
            put_fh(n, o, h);
            o.bool(true);
            put_fattr(o, &n.present(ctx, attr));
        }
        Err(e) => status(ctx, o, e),
    }
    post_op(n, ctx, o, d.as_ref().ok());
    Some(())
}

fn access(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    let want = a.u32()?;
    match h.and_then(|h| Ok((h, n.vfs.getattr(&h)?))) {
        Ok((h, attr)) => {
            let attr = n.present(ctx, attr);
            o.u32(NFS3_OK);
            o.bool(true);
            put_fattr(o, &attr);
            o.u32(n.access(want, n.vfs.is_read_only(&h)));
        }
        Err(e) => {
            status(ctx, o, e);
            o.bool(false);
        }
    }
    Some(())
}

fn readlink(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    match h.and_then(|h| n.vfs.readlink(&h)) {
        Ok(target) => {
            o.u32(NFS3_OK);
            post_op(n, ctx, o, h.as_ref().ok());
            o.opaque(&target);
        }
        Err(e) => {
            status(ctx, o, e);
            post_op(n, ctx, o, h.as_ref().ok());
        }
    }
    Some(())
}

fn read(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    let offset = a.u64()?;
    let count = a.u32()?.min(n.max_io(ctx));
    match h.and_then(|h| n.vfs.read(&h, offset, count)) {
        Ok((data, eof)) => {
            o.u32(NFS3_OK);
            post_op(n, ctx, o, h.as_ref().ok());
            o.u32(data.len() as u32);
            o.bool(eof);
            o.opaque(&data);
        }
        Err(e) => {
            status(ctx, o, e);
            post_op(n, ctx, o, h.as_ref().ok());
        }
    }
    Some(())
}

fn write(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    let offset = a.u64()?;
    let _count = a.u32()?;
    let stable = a.u32()?;
    let data = a.opaque(2 << 20)?;
    let pre = pre(n, &h);
    // UNSTABLE (0) writes go to the page cache and are flushed by COMMIT;
    // DATA_SYNC and FILE_SYNC are flushed before we reply.
    match h.and_then(|h| n.vfs.write(&h, offset, data, stable != 0)) {
        Ok(()) => {
            o.u32(NFS3_OK);
            wcc(n, ctx, o, pre, &h);
            o.u32(data.len() as u32);
            o.u32(if stable == 0 { 0 } else { 2 });
            o.fixed(&n.verifier);
        }
        Err(e) => {
            status(ctx, o, e);
            wcc(n, ctx, o, pre, &h);
        }
    }
    Some(())
}

fn create(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    let (how, sa) = match a.u32()? {
        0 => (CreateMode::Unchecked, sattr(a)?),
        1 => (CreateMode::Guarded, sattr(a)?),
        2 => {
            // EXCLUSIVE: a retransmission is answered from the duplicate
            // request cache, so a plain exclusive create is enough.
            a.fixed(8)?;
            (CreateMode::Guarded, SetAttr::default())
        }
        _ => return None,
    };
    let pre = pre(n, &d);
    let r = d.and_then(|d| n.vfs.create(&d, name, how, &sa));
    new_object(n, ctx, o, r, pre, &d);
    Some(())
}

fn mkdir(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    let sa = sattr(a)?;
    let pre = pre(n, &d);
    let r = d.and_then(|d| n.vfs.mkdir(&d, name, &sa));
    new_object(n, ctx, o, r, pre, &d);
    Some(())
}

fn symlink(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    let _sa = sattr(a)?;
    let target = a.opaque(4096)?;
    let pre = pre(n, &d);
    let r = d.and_then(|d| n.vfs.symlink(&d, name, target));
    new_object(n, ctx, o, r, pre, &d);
    Some(())
}

fn mknod(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let d = fh(n, a)?;
    name(a)?;
    let pre = pre(n, &d);
    let r = match a.u32()? {
        // Devices, sockets and FIFOs have no portable host equivalent.
        3 | 4 => {
            sattr(a)?;
            a.u64()?; // specdata3
            Err(vfs::Error::NotSupp)
        }
        6 | 7 => {
            sattr(a)?;
            Err(vfs::Error::NotSupp)
        }
        _ => {
            o.u32(NFS3ERR_BADTYPE);
            wcc(n, ctx, o, pre, &d);
            return Some(());
        }
    };
    new_object(n, ctx, o, r, pre, &d);
    Some(())
}

fn remove(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr, dir: bool) -> Option<()> {
    let d = fh(n, a)?;
    let name = name(a)?;
    let pre = pre(n, &d);
    let r = d.and_then(|d| if dir { n.vfs.rmdir(&d, name) } else { n.vfs.remove(&d, name) });
    match r {
        Ok(()) => o.u32(NFS3_OK),
        Err(e) => status(ctx, o, e),
    }
    wcc(n, ctx, o, pre, &d);
    Some(())
}

fn rename(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let fd = fh(n, a)?;
    let fname = name(a)?;
    let td = fh(n, a)?;
    let tname = name(a)?;
    let (fpre, tpre) = (pre(n, &fd), pre(n, &td));
    match fd.and_then(|fd| n.vfs.rename(&fd, fname, &td?, tname)) {
        Ok(()) => o.u32(NFS3_OK),
        Err(e) => status(ctx, o, e),
    }
    wcc(n, ctx, o, fpre, &fd);
    wcc(n, ctx, o, tpre, &td);
    Some(())
}

fn link(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    let d = fh(n, a)?;
    let name = name(a)?;
    let pre = pre(n, &d);
    match h.and_then(|h| n.vfs.link(&h, &d?, name)) {
        Ok(()) => o.u32(NFS3_OK),
        Err(e) => status(ctx, o, e),
    }
    post_op(n, ctx, o, h.as_ref().ok());
    wcc(n, ctx, o, pre, &d);
    Some(())
}

/// READDIR and READDIRPLUS. The cookie is the index of the next entry in
/// the directory snapshot; cookie 0 starts a fresh snapshot.
fn readdir(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr, plus: bool) -> Option<()> {
    let d = fh(n, a)?;
    let cookie = a.u64()?;
    a.fixed(8)?; // cookieverf: we tolerate changed directories
    let (dircount, maxcount) = if plus {
        (a.u32()? as usize, a.u32()? as usize)
    } else {
        let c = a.u32()? as usize;
        (usize::MAX, c)
    };
    let listing = match d.and_then(|d| n.vfs.readdir(&d, cookie == 0)) {
        Ok(l) => l,
        Err(e) => {
            status(ctx, o, e);
            post_op(n, ctx, o, d.as_ref().ok());
            return Some(());
        }
    };

    let mut head = Xdr::new();
    post_op(n, ctx, &mut head, d.as_ref().ok());
    head.fixed(&listing.verifier.to_be_bytes());

    // Never exceed the client's maxcount: old clients size their receive
    // buffer from it and fail to decode anything longer.
    let limit = maxcount.min(n.max_io(ctx) as usize);
    let mut size = REPLY_HEADER + 4 + head.len() + 8;
    let mut dir_bytes = 0;
    let mut entries = Xdr::new();
    let mut i = cookie as usize;
    while let Some(e) = listing.entries.get(i) {
        let base = 8 + opaque_size(e.name.len()) + 8;
        let len = 4 + base + if plus { 4 + FATTR3_SIZE + 4 + opaque_size(FH_LEN) } else { 0 };
        if size + len > limit || (i > cookie as usize && dir_bytes + base > dircount) {
            break;
        }
        entries.bool(true);
        entries.u64(e.attr.fileid);
        entries.opaque(&e.name);
        entries.u64(i as u64 + 1);
        if plus {
            entries.bool(true);
            put_fattr(&mut entries, &n.present(ctx, e.attr.clone()));
            entries.bool(true);
            put_fh(n, &mut entries, e.handle);
        }
        size += len;
        dir_bytes += base;
        i += 1;
    }
    if i == cookie as usize && i < listing.entries.len() {
        o.u32(NFS3ERR_TOOSMALL);
        post_op(n, ctx, o, d.as_ref().ok());
        return Some(());
    }
    o.u32(NFS3_OK);
    o.raw(head.as_bytes());
    o.raw(entries.as_bytes());
    o.bool(false);
    o.bool(i >= listing.entries.len());
    Some(())
}

fn fsstat(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    match h.and_then(|h| n.vfs.fsstat(&h)) {
        Ok(s) => {
            o.u32(NFS3_OK);
            post_op(n, ctx, o, h.as_ref().ok());
            o.u64(s.total_bytes);
            o.u64(s.free_bytes);
            o.u64(s.avail_bytes);
            o.u64(s.total_files);
            o.u64(s.free_files);
            o.u64(s.avail_files);
            o.u32(0); // invarsec
        }
        Err(e) => {
            status(ctx, o, e);
            post_op(n, ctx, o, h.as_ref().ok());
        }
    }
    Some(())
}

fn fsinfo(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    if let Err(e) = h.and_then(|h| n.vfs.getattr(&h)) {
        status(ctx, o, e);
        o.bool(false);
        return Some(());
    }
    let max = n.max_io(ctx);
    o.u32(NFS3_OK);
    post_op(n, ctx, o, h.as_ref().ok());
    o.u32(max); // rtmax
    o.u32(max); // rtpref
    o.u32(4096); // rtmult
    o.u32(max); // wtmax
    o.u32(max); // wtpref
    o.u32(4096); // wtmult
    o.u32(32 * 1024); // dtpref
    o.u64(i64::MAX as u64); // maxfilesize
    put_time(o, Time { secs: 0, nsecs: 1 }); // time_delta
    o.u32(0x1 | 0x2 | 0x8 | 0x10); // LINK | SYMLINK | HOMOGENEOUS | CANSETTIME
    Some(())
}

fn pathconf(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    if let Err(e) = h.and_then(|h| n.vfs.getattr(&h)) {
        status(ctx, o, e);
        o.bool(false);
        return Some(());
    }
    let h = h.ok()?;
    o.u32(NFS3_OK);
    post_op(n, ctx, o, Some(&h));
    o.u32(32767); // linkmax
    o.u32(vfs::NAME_MAX as u32);
    o.bool(true); // no_trunc
    o.bool(true); // chown_restricted
    o.bool(false); // case_insensitive
    o.bool(true); // case_preserving
    Some(())
}

fn commit(n: &Nfs, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
    let h = fh(n, a)?;
    a.u64()?; // offset
    a.u32()?; // count
    let pre = pre(n, &h);
    match h.and_then(|h| n.vfs.commit(&h)) {
        Ok(()) => {
            o.u32(NFS3_OK);
            wcc(n, ctx, o, pre, &h);
            o.fixed(&n.verifier);
        }
        Err(e) => {
            status(ctx, o, e);
            wcc(n, ctx, o, pre, &h);
        }
    }
    Some(())
}
