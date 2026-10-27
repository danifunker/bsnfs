//! End-to-end tests: a real server on ephemeral ports, driven over UDP and
//! TCP by a minimal RPC client.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use bsnfs::app::{self, PortmapState, Server};
use bsnfs::config::{Config, ExportSpec, PortmapMode};
use bsnfs::rpc::{build_call, parse_reply};
use bsnfs::server::{read_record, write_record};
use bsnfs::xdr::{Cur, Xdr};

const PORTMAP: u32 = 100000;
const NFS: u32 = 100003;
const MOUNT: u32 = 100005;
const NLM: u32 = 100021;

fn temp_dir() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::env::temp_dir().join(format!("bsnfs_it_{}_{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn start(root: &std::path::Path) -> Arc<Server> {
    let cfg = Config {
        exports: vec![ExportSpec { name: "/share".into(), path: root.to_path_buf(), read_only: false }],
        bind: "127.0.0.1".parse().unwrap(),
        nfs_port: 0,
        portmap: PortmapMode::Own,
        portmap_port: 0,
        admin: Some("127.0.0.1:0".parse().unwrap()),
        verbosity: 0,
        ..Config::default()
    };
    bsnfs::log::set_level(0);
    app::start(cfg).unwrap()
}

fn next_xid() -> u32 {
    static X: AtomicU32 = AtomicU32::new(1000);
    X.fetch_add(1, Ordering::Relaxed)
}

/// RPC client over UDP or TCP. `call` returns the result body, or the
/// accept_stat on failure.
enum Client {
    Udp(UdpSocket, SocketAddr),
    Tcp(TcpStream),
}

impl Client {
    fn udp(port: u16) -> Self {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        bsnfs::server::grow_udp_buffers(&s);
        Client::Udp(s, SocketAddr::from(([127, 0, 0, 1], port)))
    }

    fn tcp(port: u16) -> Self {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        Client::Tcp(s)
    }

    fn raw(&mut self, msg: &[u8]) -> Vec<u8> {
        match self {
            Client::Udp(s, server) => {
                s.send_to(msg, *server).unwrap();
                let mut buf = vec![0u8; 70000];
                let (n, _) = s.recv_from(&mut buf).unwrap();
                buf.truncate(n);
                buf
            }
            Client::Tcp(s) => {
                write_record(s, msg).unwrap();
                read_record(s).unwrap().unwrap()
            }
        }
    }

    fn call_xid(&mut self, xid: u32, prog: u32, vers: u32, proc_num: u32, args: &[u8]) -> Result<Vec<u8>, u32> {
        let reply = self.raw(&build_call(xid, prog, vers, proc_num, args));
        let mut c = parse_reply(&reply, xid)?;
        let n = c.remaining();
        Ok(c.fixed(n).unwrap().to_vec())
    }

    fn call(&mut self, prog: u32, vers: u32, proc_num: u32, args: &[u8]) -> Vec<u8> {
        self.call_xid(next_xid(), prog, vers, proc_num, args).expect("rpc call accepted")
    }
}

fn args(f: impl FnOnce(&mut Xdr)) -> Vec<u8> {
    let mut x = Xdr::new();
    f(&mut x);
    x.into_bytes()
}

fn skip_fattr3(c: &mut Cur) {
    c.fixed(84).unwrap();
}

fn skip_post_op(c: &mut Cur) {
    if c.bool().unwrap() {
        skip_fattr3(c);
    }
}

fn skip_wcc(c: &mut Cur) {
    if c.bool().unwrap() {
        c.fixed(24).unwrap();
    }
    skip_post_op(c);
}

/// Parse the fattr3 size field without consuming the struct.
fn fattr3_size(c: &mut Cur) -> u64 {
    let b = c.fixed(84).unwrap();
    u64::from_be_bytes(b[20..28].try_into().unwrap())
}

fn mount3(c: &mut Client, path: &[u8]) -> Vec<u8> {
    let r = c.call(MOUNT, 3, 1, &args(|x| x.opaque(path)));
    let mut cur = Cur::new(&r);
    assert_eq!(cur.u32(), Some(0), "MNT status");
    let fh = cur.opaque(64).unwrap().to_vec();
    let n = cur.u32().unwrap();
    let flavors: Vec<u32> = (0..n).map(|_| cur.u32().unwrap()).collect();
    assert!(flavors.contains(&1), "AUTH_UNIX offered");
    fh
}

/// LOOKUP/CREATE-style result: status, then (for OK) the handle.
fn v3_new_handle(r: &[u8], from_lookup: bool) -> Result<Vec<u8>, u32> {
    let mut c = Cur::new(r);
    let st = c.u32().unwrap();
    if st != 0 {
        return Err(st);
    }
    if !from_lookup {
        assert!(c.bool().unwrap(), "handle follows");
    }
    Ok(c.opaque(64).unwrap().to_vec())
}

fn diropargs(x: &mut Xdr, fh: &[u8], name: &[u8]) {
    x.opaque(fh);
    x.opaque(name);
}

fn empty_sattr3(x: &mut Xdr) {
    for _ in 0..4 {
        x.bool(false);
    }
    x.u32(0);
    x.u32(0);
}

#[test]
fn portmap_points_at_our_services() {
    let root = temp_dir();
    let s = start(&root);
    let PortmapState::Own(pm) = s.ports.portmap else { panic!("own portmapper") };
    let mut c = Client::udp(pm);

    let r = c.call(PORTMAP, 2, 3, &args(|x| {
        x.u32(NFS);
        x.u32(3);
        x.u32(6);
        x.u32(0);
    }));
    assert_eq!(Cur::new(&r).u32(), Some(s.ports.nfs as u32));

    let r = c.call(PORTMAP, 2, 3, &args(|x| {
        x.u32(MOUNT);
        x.u32(1);
        x.u32(17);
        x.u32(0);
    }));
    assert_eq!(Cur::new(&r).u32(), Some(s.ports.mount as u32));

    // rpcbind v4 GETADDR returns a universal address ending in our port.
    let r = c.call(PORTMAP, 4, 3, &args(|x| {
        x.u32(NFS);
        x.u32(3);
        x.opaque(b"tcp");
        x.opaque(b"");
        x.opaque(b"");
    }));
    let uaddr = String::from_utf8(Cur::new(&r).opaque(100).unwrap().to_vec()).unwrap();
    let p = s.ports.nfs;
    assert!(uaddr.ends_with(&format!(".{}.{}", p >> 8, p & 0xff)), "{uaddr}");
    assert!(uaddr.starts_with("127.0.0.1."), "{uaddr}");

    // An unknown version gets PROG_MISMATCH with our range.
    let reply = c.raw(&build_call(77, NFS, 4, 0, &[]));
    assert_eq!(parse_reply(&reply, 77).err(), Some(2));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn nfs3_file_lifecycle_over_tcp_and_udp() {
    let root = temp_dir();
    let s = start(&root);
    for transport in ["tcp", "udp"] {
        let mut c = if transport == "tcp" { Client::tcp(s.ports.nfs) } else { Client::udp(s.ports.nfs) };
        let dir_name = format!("dir-{transport}");
        let rootfh = mount3(&mut c, b"/share");

        // LOOKUP of a missing name.
        let r = c.call(NFS, 3, 3, &args(|x| diropargs(x, &rootfh, b"missing")));
        assert_eq!(Cur::new(&r).u32(), Some(2), "NOENT");

        // MKDIR, then CREATE inside it.
        let r = c.call(NFS, 3, 9, &args(|x| {
            diropargs(x, &rootfh, dir_name.as_bytes());
            empty_sattr3(x);
        }));
        let dirfh = v3_new_handle(&r, false).unwrap();
        let r = c.call(NFS, 3, 8, &args(|x| {
            diropargs(x, &dirfh, b"hello.txt");
            x.u32(1); // GUARDED
            empty_sattr3(x);
        }));
        let filefh = v3_new_handle(&r, false).unwrap();

        // WRITE (unstable) + COMMIT, verifiers match.
        let data = b"hello over nfs\n".repeat(1000);
        let r = c.call(NFS, 3, 7, &args(|x| {
            x.opaque(&filefh);
            x.u64(0);
            x.u32(data.len() as u32);
            x.u32(0);
            x.opaque(&data);
        }));
        let mut cur = Cur::new(&r);
        assert_eq!(cur.u32(), Some(0), "WRITE ok");
        skip_wcc(&mut cur);
        assert_eq!(cur.u32(), Some(data.len() as u32));
        assert_eq!(cur.u32(), Some(0), "UNSTABLE");
        let wverf = cur.fixed(8).unwrap().to_vec();
        let r = c.call(NFS, 3, 21, &args(|x| {
            x.opaque(&filefh);
            x.u64(0);
            x.u32(0);
        }));
        let mut cur = Cur::new(&r);
        assert_eq!(cur.u32(), Some(0));
        skip_wcc(&mut cur);
        assert_eq!(cur.fixed(8).unwrap(), &wverf[..]);
        assert_eq!(std::fs::read(root.join(&dir_name).join("hello.txt")).unwrap(), data);

        // READ it back in two pieces.
        let mut got = Vec::new();
        let mut off = 0u64;
        loop {
            let r = c.call(NFS, 3, 6, &args(|x| {
                x.opaque(&filefh);
                x.u64(off);
                x.u32(8192);
            }));
            let mut cur = Cur::new(&r);
            assert_eq!(cur.u32(), Some(0));
            skip_post_op(&mut cur);
            let n = cur.u32().unwrap();
            let eof = cur.bool().unwrap();
            got.extend_from_slice(cur.opaque(1 << 20).unwrap());
            off += n as u64;
            if eof {
                break;
            }
        }
        assert_eq!(got, data);

        // GETATTR reports the size, and the caller as owner (AUTH_NULL
        // callers are nobody, 65534).
        let r = c.call(NFS, 3, 1, &args(|x| x.opaque(&filefh)));
        assert_eq!(u32::from_be_bytes(r[16..20].try_into().unwrap()), 65534, "uid is the caller's");
        let mut cur = Cur::new(&r);
        assert_eq!(cur.u32(), Some(0));
        assert_eq!(fattr3_size(&mut cur), data.len() as u64);

        // SYMLINK + READLINK (emulated as a `symlink-link` file on Windows).
        let r = c.call(NFS, 3, 10, &args(|x| {
            diropargs(x, &dirfh, b"link");
            empty_sattr3(x);
            x.opaque(b"hello.txt");
        }));
        let linkfh = v3_new_handle(&r, false).unwrap();
        let r = c.call(NFS, 3, 5, &args(|x| x.opaque(&linkfh)));
        let mut cur = Cur::new(&r);
        assert_eq!(cur.u32(), Some(0));
        skip_post_op(&mut cur);
        assert_eq!(cur.opaque(100).unwrap(), b"hello.txt");

        // RENAME, then the old name is gone and the handle still works.
        let r = c.call(NFS, 3, 14, &args(|x| {
            diropargs(x, &dirfh, b"hello.txt");
            diropargs(x, &rootfh, format!("renamed-{transport}.txt").as_bytes());
        }));
        assert_eq!(Cur::new(&r).u32(), Some(0), "RENAME");
        let r = c.call(NFS, 3, 1, &args(|x| x.opaque(&filefh)));
        assert_eq!(Cur::new(&r).u32(), Some(0), "handle valid after rename");

        // RMDIR of a non-empty directory fails; REMOVE then RMDIR succeeds.
        let r = c.call(NFS, 3, 13, &args(|x| diropargs(x, &rootfh, dir_name.as_bytes())));
        assert_eq!(Cur::new(&r).u32(), Some(66), "NOTEMPTY");
        let r = c.call(NFS, 3, 12, &args(|x| diropargs(x, &dirfh, b"link")));
        assert_eq!(Cur::new(&r).u32(), Some(0));
        let r = c.call(NFS, 3, 13, &args(|x| diropargs(x, &rootfh, dir_name.as_bytes())));
        assert_eq!(Cur::new(&r).u32(), Some(0));

        // ACCESS, FSSTAT, FSINFO, PATHCONF all succeed on the root.
        let r = c.call(NFS, 3, 4, &args(|x| {
            x.opaque(&rootfh);
            x.u32(0x3f);
        }));
        let mut cur = Cur::new(&r);
        assert_eq!(cur.u32(), Some(0));
        skip_post_op(&mut cur);
        assert_eq!(cur.u32(), Some(0x3f), "ACCESS grants everything asked for");
        for p in [18, 19, 20] {
            let r = c.call(NFS, 3, p, &args(|x| x.opaque(&rootfh)));
            assert_eq!(Cur::new(&r).u32(), Some(0), "proc {p}");
        }
    }
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn readdir_pages_respect_the_client_limit() {
    let root = temp_dir();
    for i in 0..200 {
        std::fs::write(root.join(format!("file-with-a-longish-name-{i:03}")), b"x").unwrap();
    }
    let s = start(&root);
    let mut c = Client::udp(s.ports.nfs);

    // NFSv3 READDIRPLUS with a small maxcount.
    let rootfh = mount3(&mut c, b"/share");
    let mut names = Vec::new();
    let (mut cookie, mut verf) = (0u64, [0u8; 8]);
    let mut pages = 0;
    loop {
        let r = c.call(NFS, 3, 17, &args(|x| {
            x.opaque(&rootfh);
            x.u64(cookie);
            x.fixed(&verf);
            x.u32(1024);
            x.u32(2048);
        }));
        assert!(r.len() + 24 <= 2048, "reply {} exceeds maxcount", r.len() + 24);
        let mut cur = Cur::new(&r);
        assert_eq!(cur.u32(), Some(0));
        skip_post_op(&mut cur);
        verf.copy_from_slice(cur.fixed(8).unwrap());
        while cur.bool().unwrap() {
            cur.u64().unwrap();
            names.push(String::from_utf8(cur.opaque(255).unwrap().to_vec()).unwrap());
            cookie = cur.u64().unwrap();
            skip_post_op(&mut cur);
            if cur.bool().unwrap() {
                cur.opaque(64).unwrap();
            }
        }
        pages += 1;
        if cur.bool().unwrap() {
            break;
        }
    }
    assert!(pages > 5, "expected paging, got {pages} pages");
    assert_eq!(names.len(), 202, "200 files plus . and ..");
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 202, "no duplicates");

    // NFSv2 READDIR with MOUNT v1 and a tiny count.
    let r = c.call(MOUNT, 1, 1, &args(|x| x.opaque(b"/share")));
    let mut cur = Cur::new(&r);
    assert_eq!(cur.u32(), Some(0));
    let fh2 = cur.fixed(32).unwrap().to_vec();
    let mut names = Vec::new();
    let mut cookie = [0u8; 4];
    loop {
        let r = c.call(NFS, 2, 16, &args(|x| {
            x.fixed(&fh2);
            x.fixed(&cookie);
            x.u32(512);
        }));
        assert!(r.len() <= 512, "v2 reply {} exceeds count", r.len());
        let mut cur = Cur::new(&r);
        assert_eq!(cur.u32(), Some(0));
        while cur.bool().unwrap() {
            let fileid = cur.u32().unwrap();
            assert!(fileid > 1, "fileid 0/1 must never appear");
            names.push(cur.opaque(255).unwrap().to_vec());
            cookie.copy_from_slice(cur.fixed(4).unwrap());
        }
        if cur.bool().unwrap() {
            break;
        }
    }
    assert_eq!(names.len(), 202);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn nfs2_lookup_read_write() {
    let root = temp_dir();
    std::fs::write(root.join("old.txt"), b"version two").unwrap();
    let s = start(&root);
    let mut c = Client::udp(s.ports.nfs);
    let r = c.call(MOUNT, 1, 1, &args(|x| x.opaque(b"/")));
    let mut cur = Cur::new(&r);
    assert_eq!(cur.u32(), Some(0));
    let rootfh = cur.fixed(32).unwrap().to_vec();

    let r = c.call(NFS, 2, 4, &args(|x| {
        x.fixed(&rootfh);
        x.opaque(b"old.txt");
    }));
    let mut cur = Cur::new(&r);
    assert_eq!(cur.u32(), Some(0));
    let fh = cur.fixed(32).unwrap().to_vec();
    let attr = cur.fixed(68).unwrap();
    assert_eq!(u32::from_be_bytes(attr[0..4].try_into().unwrap()), 1, "NFREG");
    assert_eq!(u32::from_be_bytes(attr[4..8].try_into().unwrap()) & 0o170000, 0o100000, "mode has type bits");
    assert_eq!(u32::from_be_bytes(attr[20..24].try_into().unwrap()), 11, "size");

    let r = c.call(NFS, 2, 6, &args(|x| {
        x.fixed(&fh);
        x.u32(8);
        x.u32(100);
        x.u32(0);
    }));
    let mut cur = Cur::new(&r);
    assert_eq!(cur.u32(), Some(0));
    cur.fixed(68).unwrap();
    assert_eq!(cur.opaque(100).unwrap(), b"two");

    let r = c.call(NFS, 2, 8, &args(|x| {
        x.fixed(&fh);
        x.u32(0);
        x.u32(8);
        x.u32(0);
        x.opaque(b"TWO!");
    }));
    assert_eq!(Cur::new(&r).u32(), Some(0));
    assert_eq!(std::fs::read(root.join("old.txt")).unwrap(), b"version TWO!");

    // A garbage handle is a clean STALE, not a crash or garbage-args.
    let r = c.call(NFS, 2, 1, &args(|x| x.fixed(&[0xAB; 32])));
    assert_eq!(Cur::new(&r).u32(), Some(70));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn retransmitted_create_gets_the_original_reply() {
    let root = temp_dir();
    let s = start(&root);
    let mut c = Client::udp(s.ports.nfs);
    let rootfh = mount3(&mut c, b"/share");
    let create = args(|x| {
        diropargs(x, &rootfh, b"once");
        x.u32(1); // GUARDED: a second real execution would fail with EXIST
        empty_sattr3(x);
    });
    let xid = next_xid();
    let first = c.call_xid(xid, NFS, 3, 8, &create).unwrap();
    let again = c.call_xid(xid, NFS, 3, 8, &create).unwrap();
    assert_eq!(Cur::new(&first).u32(), Some(0));
    assert_eq!(first, again);
    let fresh = c.call(NFS, 3, 8, &create);
    assert_eq!(Cur::new(&fresh).u32(), Some(17), "a new xid really executes: EXIST");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn nlm_grants_locks_and_echoes_the_cookie() {
    let root = temp_dir();
    let s = start(&root);
    let mut c = Client::udp(s.ports.nlm.unwrap());
    let r = c.call(NLM, 4, 2, &args(|x| {
        x.opaque(b"cookie42");
        x.bool(false); // block
        x.bool(true); // exclusive
        // (rest of nlm4_lock is ignored by our stand-in)
    }));
    let mut cur = Cur::new(&r);
    assert_eq!(cur.opaque(64).unwrap(), b"cookie42");
    assert_eq!(cur.u32(), Some(0), "granted");
    std::fs::remove_dir_all(&root).ok();
}

fn http(addr: SocketAddr, req: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    (status, body)
}

#[test]
fn admin_api_lists_and_changes_exports() {
    let root = temp_dir();
    let extra = temp_dir();
    let s = start(&root);
    let admin = s.ports.admin.unwrap();

    let (st, page) = http(admin, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
    assert_eq!(st, 200);
    // DNS rebinding: a foreign site's name pointed at us gets nothing.
    let (st, _) = http(admin, "GET /api/status HTTP/1.1\r\nHost: evil.example.com\r\n\r\n");
    assert_eq!(st, 403);
    assert!(page.contains("<title>bsnfs</title>"));

    let (st, body) = http(admin, "GET /api/status HTTP/1.1\r\nHost: nas.local:8080\r\n\r\n");
    assert_eq!(st, 200);
    assert!(body.contains("\"name\":\"/share\""), "{body}");
    assert!(body.contains("nas.local:/share"), "mount hint uses the Host header: {body}");

    let form = format!("name=extra&path={}", extra.display().to_string().replace('/', "%2F"));
    let post = |headers: &str| {
        format!("POST /api/exports/add HTTP/1.1\r\nHost: localhost\r\n{headers}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}", form.len())
    };
    let (st, _) = http(admin, &post(""));
    assert_eq!(st, 403, "mutations need the X-Requested-With header");
    let (st, body) = http(admin, &post("X-Requested-With: bsnfs\r\n"));
    assert_eq!(st, 200, "{body}");
    assert_eq!(s.vfs.exports().len(), 2);

    // The new export is mountable right away.
    let mut c = Client::udp(s.ports.nfs);
    mount3(&mut c, b"/extra");
    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&extra).ok();
}
