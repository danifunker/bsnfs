//! The portmapper (program 100000): portmap v2 and the rpcbind v3/v4 calls
//! clients actually use. Clients ask it which port MOUNT, NFS and NLM are on.
//! Also a client side, for registering with the host's own portmapper when it
//! already owns port 111.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::Duration;

use crate::rpc::{self, Ctx, Program, Reject, Transport};
use crate::xdr::{Cur, Xdr};

pub const PROG: u32 = 100000;
pub const IPPROTO_TCP: u32 = 6;
pub const IPPROTO_UDP: u32 = 17;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub prog: u32,
    pub vers: u32,
    pub prot: u32,
    pub port: u16,
}

pub struct Portmap {
    maps: Mutex<Vec<Mapping>>,
}

impl Portmap {
    pub fn new(maps: Vec<Mapping>) -> Self {
        Self { maps: Mutex::new(maps) }
    }

    /// Replace the registration table.
    pub fn set(&self, maps: Vec<Mapping>) {
        *self.maps.lock().unwrap() = maps;
    }

    fn netid_prot(netid: &[u8]) -> Option<u32> {
        match netid {
            b"udp" | b"udp6" => Some(IPPROTO_UDP),
            b"tcp" | b"tcp6" => Some(IPPROTO_TCP),
            _ => None,
        }
    }

    fn prot_netid(prot: u32) -> &'static str {
        if prot == IPPROTO_TCP { "tcp" } else { "udp" }
    }

    /// The address a client should use to reach us, as rpcbind's universal
    /// address string ("a.b.c.d.porthi.portlo"). For wildcard UDP sockets
    /// the local address is unknown, so ask the routing table which source
    /// address replies to this peer would use.
    fn uaddr(ctx: &Ctx, port: u16) -> String {
        let ip = ctx.local.map(|l| l.ip()).or_else(|| {
            let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
            s.connect(ctx.peer).ok()?;
            s.local_addr().ok().map(|a| a.ip())
        });
        let ip = match ip {
            Some(IpAddr::V4(v4)) => v4,
            _ => Ipv4Addr::UNSPECIFIED,
        };
        let o = ip.octets();
        format!("{}.{}.{}.{}.{}.{}", o[0], o[1], o[2], o[3], port >> 8, port & 0xff)
    }

    fn lookup(&self, prog: u32, vers: u32, prot: u32) -> Option<Mapping> {
        let maps = self.maps.lock().unwrap();
        maps.iter().find(|m| m.prog == prog && m.vers == vers && m.prot == prot).copied()
    }

    fn v2(&self, ctx: &Ctx, args: &mut Cur, out: &mut Xdr) -> Option<Result<(), Reject>> {
        match ctx.proc_num {
            0 => {}
            1 | 2 => {
                // SET / UNSET, only from this host.
                let m = Mapping { prog: args.u32()?, vers: args.u32()?, prot: args.u32()?, port: args.u32()? as u16 };
                if !ctx.peer.ip().is_loopback() {
                    out.bool(false);
                    return Some(Ok(()));
                }
                let mut maps = self.maps.lock().unwrap();
                if ctx.proc_num == 1 {
                    let taken = maps.iter().any(|x| x.prog == m.prog && x.vers == m.vers && x.prot == m.prot);
                    if !taken {
                        maps.push(m);
                    }
                    out.bool(!taken);
                } else {
                    maps.retain(|x| !(x.prog == m.prog && x.vers == m.vers));
                    out.bool(true);
                }
            }
            3 => {
                let (prog, vers, prot) = (args.u32()?, args.u32()?, args.u32()?);
                out.u32(self.lookup(prog, vers, prot).map_or(0, |m| m.port as u32));
            }
            4 => {
                for m in self.maps.lock().unwrap().iter() {
                    out.bool(true);
                    out.u32(m.prog);
                    out.u32(m.vers);
                    out.u32(m.prot);
                    out.u32(m.port as u32);
                }
                out.bool(false);
            }
            _ => return Some(Err(Reject::ProcUnavail)),
        }
        Some(Ok(()))
    }

    /// rpcbind v3/v4. Only the lookup and dump calls; registration happens
    /// through v2.
    fn v34(&self, ctx: &Ctx, args: &mut Cur, out: &mut Xdr) -> Option<Result<(), Reject>> {
        match ctx.proc_num {
            0 => {}
            // GETADDR, and v4's GETVERSADDR (same args and result).
            3 | 9 => {
                let prog = args.u32()?;
                let vers = args.u32()?;
                let netid = args.opaque(64)?;
                args.opaque(256)?; // addr
                args.opaque(256)?; // owner
                let prot = Self::netid_prot(netid).unwrap_or(match ctx.transport {
                    Transport::Udp => IPPROTO_UDP,
                    Transport::Tcp => IPPROTO_TCP,
                });
                match self.lookup(prog, vers, prot) {
                    Some(m) => out.opaque(Self::uaddr(ctx, m.port).as_bytes()),
                    None => out.opaque(b""),
                }
            }
            4 => {
                for m in self.maps.lock().unwrap().iter() {
                    out.bool(true);
                    out.u32(m.prog);
                    out.u32(m.vers);
                    out.opaque(Self::prot_netid(m.prot).as_bytes());
                    out.opaque(Self::uaddr(ctx, m.port).as_bytes());
                    out.opaque(b"bsnfs");
                }
                out.bool(false);
            }
            6 => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                out.u32(now.as_secs() as u32);
            }
            _ => return Some(Err(Reject::ProcUnavail)),
        }
        Some(Ok(()))
    }
}

impl Program for Portmap {
    fn prog(&self) -> u32 {
        PROG
    }

    fn name(&self) -> &'static str {
        "portmap"
    }

    fn versions(&self) -> &[u32] {
        &[2, 3, 4]
    }

    fn proc_name(&self, vers: u32, proc_num: u32) -> &'static str {
        let v2 = ["NULL", "SET", "UNSET", "GETPORT", "DUMP", "CALLIT"];
        let v34 = ["NULL", "SET", "UNSET", "GETADDR", "DUMP", "CALLIT", "GETTIME", "UADDR2TADDR", "TADDR2UADDR", "GETVERSADDR"];
        let names: &[&str] = if vers == 2 { &v2 } else { &v34 };
        names.get(proc_num as usize).copied().unwrap_or("?")
    }

    fn call(&self, ctx: &Ctx, args: &mut Cur, out: &mut Xdr) -> Result<(), Reject> {
        let r = if ctx.vers == 2 { self.v2(ctx, args, out) } else { self.v34(ctx, args, out) };
        r.unwrap_or(Err(Reject::GarbageArgs))
    }
}

/// Register `maps` with the host portmapper at `server`, replacing any
/// stale registrations for the same programs. Returns how many succeeded.
pub fn register_with(server: SocketAddr, maps: &[Mapping]) -> std::io::Result<usize> {
    let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))?;
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    // UNSET first: a previous run that died without cleaning up would
    // otherwise make SET fail.
    for m in maps {
        call_set(&sock, server, 2, m)?;
    }
    let mut ok = 0;
    for m in maps {
        if call_set(&sock, server, 1, m)? {
            ok += 1;
        }
    }
    Ok(ok)
}

/// Remove our registrations from the host portmapper.
pub fn unregister_with(server: SocketAddr, maps: &[Mapping]) {
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
        let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
        for m in maps {
            let _ = call_set(&sock, server, 2, m);
        }
    }
}

fn call_set(sock: &UdpSocket, server: SocketAddr, proc_num: u32, m: &Mapping) -> std::io::Result<bool> {
    let xid = rand_xid();
    let mut a = Xdr::new();
    a.u32(m.prog);
    a.u32(m.vers);
    a.u32(m.prot);
    a.u32(m.port as u32);
    sock.send_to(&rpc::build_call(xid, PROG, 2, proc_num, a.as_bytes()), server)?;
    let mut buf = [0u8; 512];
    loop {
        let (n, _) = sock.recv_from(&mut buf)?;
        if let Ok(mut c) = rpc::parse_reply(&buf[..n], xid) {
            return Ok(c.bool().unwrap_or(false));
        }
    }
}

fn rand_xid() -> u32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (t.subsec_nanos() ^ std::process::id().rotate_left(16)).wrapping_add(t.as_secs() as u32)
}
