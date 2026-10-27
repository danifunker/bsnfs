//! MOUNT protocol (program 100005): v1/v2 hand out NFSv2 handles, v3 hands
//! out NFSv3 handles. Also tracks who has what mounted, for `showmount -a`
//! and the admin UI.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::rpc::{Ctx, Program, Reject, AUTH_NULL, AUTH_UNIX};
use crate::vfs::{Error, Vfs};
use crate::xdr::{Cur, Xdr};

pub const PROG: u32 = 100005;

#[derive(Debug, Clone)]
pub struct MountEntry {
    pub client: IpAddr,
    /// Client machine name from its credentials, or its address.
    pub host: String,
    pub path: String,
    pub export: usize,
    pub since: SystemTime,
}

pub struct Mount {
    vfs: Arc<Vfs>,
    mounts: Mutex<Vec<MountEntry>>,
}

impl Mount {
    pub fn new(vfs: Arc<Vfs>) -> Self {
        Self { vfs, mounts: Mutex::new(Vec::new()) }
    }

    pub fn mounts(&self) -> Vec<MountEntry> {
        self.mounts.lock().unwrap().clone()
    }

    fn mnt(&self, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Option<()> {
        let raw = a.opaque(1024)?;
        let path = String::from_utf8_lossy(raw).into_owned();
        match self.vfs.mount(raw) {
            Ok((export, h)) => {
                info!("mount: {} mounted {path} (MOUNT v{})", ctx.peer.ip(), ctx.vers);
                let host = if ctx.cred.machine.is_empty() { ctx.peer.ip().to_string() } else { ctx.cred.machine.clone() };
                let mut mounts = self.mounts.lock().unwrap();
                mounts.retain(|m| !(m.client == ctx.peer.ip() && m.path == path));
                mounts.push(MountEntry { client: ctx.peer.ip(), host, path, export, since: SystemTime::now() });
                o.u32(0);
                if ctx.vers == 3 {
                    o.opaque(&self.vfs.encode(h));
                    o.u32(2);
                    o.u32(AUTH_UNIX);
                    o.u32(AUTH_NULL);
                } else {
                    let mut fh = [0u8; 32];
                    fh[..16].copy_from_slice(&self.vfs.encode(h));
                    o.fixed(&fh);
                }
            }
            Err(e) => {
                info!("mount: {} asked for {path}: {e:?}", ctx.peer.ip());
                o.u32(match e {
                    Error::NoEnt | Error::Stale => 2,
                    Error::Acces | Error::Perm => 13,
                    Error::NotDir => 20,
                    Error::NameTooLong => 63,
                    _ => 5,
                });
            }
        }
        Some(())
    }
}

impl Program for Mount {
    fn prog(&self) -> u32 {
        PROG
    }

    fn name(&self) -> &'static str {
        "mount"
    }

    fn versions(&self) -> &[u32] {
        &[1, 2, 3]
    }

    fn proc_name(&self, _vers: u32, proc_num: u32) -> &'static str {
        ["NULL", "MNT", "DUMP", "UMNT", "UMNTALL", "EXPORT", "EXPORTALL", "PATHCONF"]
            .get(proc_num as usize)
            .copied()
            .unwrap_or("?")
    }

    fn call(&self, ctx: &Ctx, a: &mut Cur, o: &mut Xdr) -> Result<(), Reject> {
        let r = match ctx.proc_num {
            0 => Some(()),
            1 => self.mnt(ctx, a, o),
            2 => {
                for m in self.mounts.lock().unwrap().iter() {
                    o.bool(true);
                    o.opaque(m.host.as_bytes());
                    o.opaque(m.path.as_bytes());
                }
                o.bool(false);
                Some(())
            }
            3 => a.opaque(1024).map(|raw| {
                let path = String::from_utf8_lossy(raw);
                info!("mount: {} unmounted {path}", ctx.peer.ip());
                self.mounts.lock().unwrap().retain(|m| !(m.client == ctx.peer.ip() && m.path == path));
            }),
            4 => {
                self.mounts.lock().unwrap().retain(|m| m.client != ctx.peer.ip());
                Some(())
            }
            5 | 6 if ctx.vers < 3 || ctx.proc_num == 5 => {
                for (_, e) in self.vfs.exports() {
                    o.bool(true);
                    o.opaque(e.name.as_bytes());
                    o.bool(false); // no group restrictions
                }
                o.bool(false);
                Some(())
            }
            _ => return Err(Reject::ProcUnavail),
        };
        r.ok_or(Reject::GarbageArgs)
    }
}
