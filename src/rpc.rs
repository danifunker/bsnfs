//! ONC RPC v2 (RFC 5531): call parsing, credentials, reply framing, program
//! dispatch, and the duplicate-request cache. Transport-agnostic: the
//! listeners in `server.rs` hand complete call messages to
//! [`Dispatcher::handle`] and send back whatever it returns.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use crate::xdr::{Cur, Xdr};

const MSG_CALL: u32 = 0;
const MSG_REPLY: u32 = 1;
const RPC_VERSION: u32 = 2;
const MSG_ACCEPTED: u32 = 0;
const MSG_DENIED: u32 = 1;
const RPC_MISMATCH: u32 = 0;

pub const AUTH_NULL: u32 = 0;
pub const AUTH_UNIX: u32 = 1;

/// The uid/gid an AUTH_NULL caller is treated as.
pub const NOBODY: u32 = 65534;

/// Why a call was not executed (RPC `accept_stat`, minus SUCCESS and
/// PROG_MISMATCH, which the dispatcher produces itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    ProcUnavail,
    GarbageArgs,
    SystemErr,
}

impl Reject {
    fn accept_stat(self) -> u32 {
        match self {
            Reject::ProcUnavail => 3,
            Reject::GarbageArgs => 4,
            Reject::SystemErr => 5,
        }
    }
}

/// Caller identity from the RPC credential. Nothing is authenticated; this
/// only decides who files are presented as belonging to.
#[derive(Debug, Clone)]
pub struct Cred {
    pub flavor: u32,
    pub machine: String,
    pub uid: u32,
    pub gid: u32,
    pub gids: Vec<u32>,
}

impl Default for Cred {
    fn default() -> Self {
        Self { flavor: AUTH_NULL, machine: String::new(), uid: NOBODY, gid: NOBODY, gids: Vec::new() }
    }
}

impl Cred {
    fn parse(flavor: u32, body: &[u8]) -> Self {
        if flavor != AUTH_UNIX {
            return Self { flavor, ..Self::default() };
        }
        let parse = || -> Option<Cred> {
            let mut c = Cur::new(body);
            let _stamp = c.u32()?;
            let machine = String::from_utf8_lossy(c.opaque(255)?).into_owned();
            let uid = c.u32()?;
            let gid = c.u32()?;
            let n = c.u32()? as usize;
            let gids = (0..n.min(16)).map(|_| c.u32()).collect::<Option<Vec<_>>>()?;
            Some(Cred { flavor, machine, uid, gid, gids })
        };
        parse().unwrap_or_else(|| Self { flavor, ..Self::default() })
    }

    pub fn in_group(&self, gid: u32) -> bool {
        self.gid == gid || self.gids.contains(&gid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Udp,
    Tcp,
}

impl Transport {
    pub fn netid(self) -> &'static str {
        match self {
            Transport::Udp => "udp",
            Transport::Tcp => "tcp",
        }
    }
}

/// Everything a procedure handler knows about the call besides its args.
pub struct Ctx {
    pub xid: u32,
    pub vers: u32,
    pub proc_num: u32,
    pub cred: Cred,
    pub peer: SocketAddr,
    /// The server address the call arrived on, when known (it is not for
    /// UDP sockets bound to the wildcard address).
    pub local: Option<SocketAddr>,
    pub transport: Transport,
}

/// One RPC program (portmap, MOUNT, NFS, NLM, ...).
pub trait Program: Send + Sync {
    fn prog(&self) -> u32;

    /// Short name for logs, e.g. "nfs".
    fn name(&self) -> &'static str;

    /// Supported versions, ascending.
    fn versions(&self) -> &[u32];

    /// Procedure name for logs.
    fn proc_name(&self, _vers: u32, _proc_num: u32) -> &'static str {
        "?"
    }

    /// Whether re-executing the procedure is harmless. Non-idempotent calls
    /// go through the duplicate-request cache so a retransmitted CREATE or
    /// REMOVE gets the original reply instead of a spurious error.
    fn idempotent(&self, _vers: u32, _proc_num: u32) -> bool {
        true
    }

    /// Execute the call, appending the result body to `out`.
    fn call(&self, ctx: &Ctx, args: &mut Cur, out: &mut Xdr) -> Result<(), Reject>;
}

/// Start an accepted reply with an AUTH_NULL verifier.
fn accepted(xid: u32, accept_stat: u32) -> Xdr {
    let mut x = Xdr::new();
    x.u32(xid);
    x.u32(MSG_REPLY);
    x.u32(MSG_ACCEPTED);
    x.u32(AUTH_NULL);
    x.u32(0);
    x.u32(accept_stat);
    x
}

/// Duplicate-request cache keyed by caller and xid. Bounded, FIFO-evicted.
struct Drc {
    cap: usize,
    order: VecDeque<DrcKey>,
    map: HashMap<DrcKey, Vec<u8>>,
}

type DrcKey = (IpAddr, u32, u32, u32, u32); // (client, xid, prog, vers, proc)

impl Drc {
    fn new(cap: usize) -> Self {
        Self { cap, order: VecDeque::new(), map: HashMap::new() }
    }

    fn get(&self, k: &DrcKey) -> Option<Vec<u8>> {
        self.map.get(k).cloned()
    }

    fn put(&mut self, k: DrcKey, reply: Vec<u8>) {
        if self.map.insert(k, reply).is_some() {
            return;
        }
        self.order.push_back(k);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// One recently handled call, for the admin UI.
#[derive(Debug, Clone)]
pub struct RecentCall {
    pub at: SystemTime,
    pub peer: SocketAddr,
    pub transport: Transport,
    /// e.g. "nfs3 READ".
    pub call: String,
    pub uid: u32,
    pub micros: u64,
}

#[derive(Debug, Clone)]
pub struct ClientStat {
    pub calls: u64,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
    pub machine: String,
}

/// Call counters since startup.
#[derive(Debug, Clone)]
pub struct Stats {
    pub started: SystemTime,
    pub calls: u64,
    pub per_call: BTreeMap<String, u64>,
    pub clients: HashMap<IpAddr, ClientStat>,
    pub recent: VecDeque<RecentCall>,
}

const RECENT_CALLS: usize = 100;

impl Stats {
    fn new() -> Self {
        Self { started: SystemTime::now(), calls: 0, per_call: BTreeMap::new(), clients: HashMap::new(), recent: VecDeque::new() }
    }

    fn record(&mut self, ctx: &Ctx, call: String, micros: u64) {
        let now = SystemTime::now();
        self.calls += 1;
        *self.per_call.entry(call.clone()).or_default() += 1;
        let c = self.clients.entry(ctx.peer.ip()).or_insert_with(|| ClientStat {
            calls: 0,
            first_seen: now,
            last_seen: now,
            machine: String::new(),
        });
        c.calls += 1;
        c.last_seen = now;
        if !ctx.cred.machine.is_empty() {
            c.machine.clone_from(&ctx.cred.machine);
        }
        if self.recent.len() >= RECENT_CALLS {
            self.recent.pop_front();
        }
        self.recent.push_back(RecentCall { at: now, peer: ctx.peer, transport: ctx.transport, call, uid: ctx.cred.uid, micros });
    }
}

/// Routes calls to registered programs. One dispatcher serves every
/// listener, so any program answers on any of our ports.
pub struct Dispatcher {
    programs: Vec<Arc<dyn Program>>,
    drc: Mutex<Drc>,
    stats: Mutex<Stats>,
}

impl Dispatcher {
    pub fn new(programs: Vec<Arc<dyn Program>>) -> Self {
        Self { programs, drc: Mutex::new(Drc::new(1024)), stats: Mutex::new(Stats::new()) }
    }

    pub fn stats(&self) -> Stats {
        self.stats.lock().unwrap().clone()
    }

    /// Handle one complete RPC message. `None` means it was not a call we
    /// should answer (a reply, or too short to carry an xid).
    pub fn handle(&self, msg: &[u8], peer: SocketAddr, local: Option<SocketAddr>, transport: Transport) -> Option<Vec<u8>> {
        let mut c = Cur::new(msg);
        let xid = c.u32()?;
        if c.u32()? != MSG_CALL {
            return None;
        }
        if c.u32()? != RPC_VERSION {
            let mut x = Xdr::new();
            x.u32(xid);
            x.u32(MSG_REPLY);
            x.u32(MSG_DENIED);
            x.u32(RPC_MISMATCH);
            x.u32(RPC_VERSION);
            x.u32(RPC_VERSION);
            return Some(x.into_bytes());
        }
        let (Some(prog), Some(vers), Some(proc_num)) = (c.u32(), c.u32(), c.u32()) else {
            return Some(accepted(xid, Reject::GarbageArgs.accept_stat()).into_bytes());
        };
        let Some((cred_flavor, cred_body)) = c.u32().zip(c.opaque(400)) else {
            return Some(accepted(xid, Reject::GarbageArgs.accept_stat()).into_bytes());
        };
        if c.u32().zip(c.opaque(400)).is_none() {
            return Some(accepted(xid, Reject::GarbageArgs.accept_stat()).into_bytes());
        }

        let Some(program) = self.programs.iter().find(|p| p.prog() == prog) else {
            debug!("rpc: {peer} called unknown program {prog} v{vers} proc {proc_num}");
            return Some(accepted(xid, 1).into_bytes()); // PROG_UNAVAIL
        };
        let versions = program.versions();
        if !versions.contains(&vers) {
            debug!("rpc: {peer} called {} v{vers}, which we don't serve", program.name());
            let mut x = accepted(xid, 2); // PROG_MISMATCH
            x.u32(versions[0]);
            x.u32(*versions.last().unwrap());
            return Some(x.into_bytes());
        }

        let ctx = Ctx { xid, vers, proc_num, cred: Cred::parse(cred_flavor, cred_body), peer, local, transport };
        let pname = program.proc_name(vers, proc_num);
        let cache_key = (!program.idempotent(vers, proc_num)).then_some((peer.ip(), xid, prog, vers, proc_num));
        if let Some(k) = &cache_key
            && let Some(cached) = self.drc.lock().unwrap().get(k) {
                debug!("{}{vers} {pname} from {peer}: retransmission, replaying cached reply", program.name());
                return Some(cached);
            }

        let started = Instant::now();
        let mut body = Xdr::new();
        let reply = match program.call(&ctx, &mut c, &mut body) {
            Ok(()) => {
                let mut x = accepted(xid, 0);
                x.raw(body.as_bytes());
                x.into_bytes()
            }
            Err(r) => {
                trace!("{}{vers} {pname} from {peer}: {r:?}", program.name());
                accepted(xid, r.accept_stat()).into_bytes()
            }
        };
        let elapsed = started.elapsed();
        debug!("{}{vers} {pname} from {peer} uid={} ({:.1}ms)", program.name(), ctx.cred.uid, elapsed.as_secs_f64() * 1000.0);
        if prog != crate::portmap::PROG || proc_num != 0 {
            let name = format!("{}{vers} {pname}", program.name());
            self.stats.lock().unwrap().record(&ctx, name, elapsed.as_micros() as u64);
        }
        if let Some(k) = cache_key {
            self.drc.lock().unwrap().put(k, reply.clone());
        }
        Some(reply)
    }
}

/// Build a CALL message (AUTH_NULL credentials). Used when we act as a
/// client of the system portmapper, and by tests.
pub fn build_call(xid: u32, prog: u32, vers: u32, proc_num: u32, args: &[u8]) -> Vec<u8> {
    let mut x = Xdr::new();
    x.u32(xid);
    x.u32(MSG_CALL);
    x.u32(RPC_VERSION);
    x.u32(prog);
    x.u32(vers);
    x.u32(proc_num);
    x.u32(AUTH_NULL);
    x.u32(0);
    x.u32(AUTH_NULL);
    x.u32(0);
    x.raw(args);
    x.into_bytes()
}

/// Parse an accepted, successful reply, returning a cursor at the result.
/// `Err` carries the accept_stat (or `u32::MAX` if the call was denied).
pub fn parse_reply(msg: &[u8], xid: u32) -> Result<Cur<'_>, u32> {
    let mut c = Cur::new(msg);
    let ok = (|| {
        if c.u32()? != xid || c.u32()? != MSG_REPLY {
            return None;
        }
        c.u32()
    })();
    match ok {
        Some(MSG_ACCEPTED) => {}
        _ => return Err(u32::MAX),
    }
    let stat = (|| {
        c.u32()?; // verifier flavor
        c.opaque(400)?;
        c.u32()
    })()
    .ok_or(u32::MAX)?;
    if stat != 0 {
        return Err(stat);
    }
    Ok(c)
}
