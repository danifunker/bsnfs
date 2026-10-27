//! The NFS program (100003) and its companions MOUNT (100005) and NLM
//! (100021). Versions 2 and 3 share the [`Vfs`] backend; only the wire
//! encoding differs.

pub mod mount;
pub mod nlm;
mod v2;
mod v3;

use std::sync::Arc;

use crate::rpc::{Ctx, Program, Reject, Transport};
use crate::vfs::{Attr, Error, Vfs};
use crate::xdr::{Cur, Xdr};

pub const PROG: u32 = 100003;

pub struct Nfs {
    vfs: Arc<Vfs>,
    versions: Vec<u32>,
    /// NFSv3 write verifier: changes on every server start, which tells
    /// clients that unstable writes they haven't committed must be resent.
    verifier: [u8; 8],
}

impl Nfs {
    pub fn new(vfs: Arc<Vfs>, versions: Vec<u32>) -> Self {
        let boot = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let verifier = (boot.as_nanos() as u64 ^ std::process::id() as u64).to_be_bytes();
        Self { vfs, versions, verifier }
    }

    /// uid/gid are a shim: every object is shown as owned by whoever is
    /// asking, so the client's own permission checks always pass.
    fn present(&self, ctx: &Ctx, mut a: Attr) -> Attr {
        a.uid = ctx.cred.uid;
        a.gid = ctx.cred.gid;
        a
    }

    /// NFSv3 ACCESS: grant whatever was asked for. Only a read-only export
    /// holds anything back, since the write would fail anyway.
    fn access(&self, want: u32, read_only: bool) -> u32 {
        const MODIFY: u32 = 0x04;
        const EXTEND: u32 = 0x08;
        const DELETE: u32 = 0x10;
        if read_only { want & !(MODIFY | EXTEND | DELETE) } else { want }
    }

    /// Largest READ/WRITE we advertise and honor.
    fn max_io(&self, ctx: &Ctx) -> u32 {
        match ctx.transport {
            Transport::Udp => 32 * 1024,
            Transport::Tcp => 1024 * 1024,
        }
    }
}

/// NFSv3 status for a backend error (RFC 1813 `nfsstat3`).
fn status3(e: Error) -> u32 {
    match e {
        Error::Perm => 1,
        Error::NoEnt => 2,
        Error::Io => 5,
        Error::Nxio => 6,
        Error::Acces => 13,
        Error::Exist => 17,
        Error::Xdev => 18,
        Error::NotDir => 20,
        Error::IsDir => 21,
        Error::Inval => 22,
        Error::FBig => 27,
        Error::NoSpc => 28,
        Error::RoFs => 30,
        Error::MLink => 31,
        Error::NameTooLong => 63,
        Error::NotEmpty => 66,
        Error::DQuot => 69,
        Error::Stale => 70,
        Error::BadHandle => 10001,
        Error::NotSupp => 10004,
    }
}

/// NFSv2 status (RFC 1094 `stat`), which lacks the v3-only codes.
fn status2(e: Error) -> u32 {
    match e {
        Error::BadHandle => 70,
        Error::NotSupp => 5,
        e => status3(e),
    }
}

impl Program for Nfs {
    fn prog(&self) -> u32 {
        PROG
    }

    fn name(&self) -> &'static str {
        "nfs"
    }

    fn versions(&self) -> &[u32] {
        &self.versions
    }

    fn proc_name(&self, vers: u32, proc_num: u32) -> &'static str {
        if vers == 2 { v2::proc_name(proc_num) } else { v3::proc_name(proc_num) }
    }

    fn idempotent(&self, vers: u32, proc_num: u32) -> bool {
        if vers == 2 { v2::idempotent(proc_num) } else { v3::idempotent(proc_num) }
    }

    fn call(&self, ctx: &Ctx, args: &mut Cur, out: &mut Xdr) -> Result<(), Reject> {
        if ctx.vers == 2 { v2::call(self, ctx, args, out) } else { v3::call(self, ctx, args, out) }
    }
}
