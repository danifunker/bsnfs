//! Socket listeners: hand RPC messages from UDP datagrams and TCP record
//! streams to the shared [`Dispatcher`] and send back the replies.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::thread;

use crate::rpc::{Dispatcher, Transport};

/// Largest TCP record we accept (a 1 MiB WRITE plus headers).
const MAX_RECORD: usize = 2 << 20;
const UDP_WORKERS: usize = 4;

/// Serve RPC over UDP with a few worker threads sharing the socket.
pub fn spawn_udp(sock: UdpSocket, dispatcher: Arc<Dispatcher>) -> io::Result<()> {
    grow_udp_buffers(&sock);
    let local = sock.local_addr()?;
    let local = (!local.ip().is_unspecified()).then_some(local);
    for i in 0..UDP_WORKERS {
        let sock = sock.try_clone()?;
        let d = dispatcher.clone();
        thread::Builder::new().name(format!("udp-{}-{i}", sock.local_addr()?.port())).spawn(move || {
            let mut buf = vec![0u8; 65536];
            loop {
                let (n, peer) = match sock.recv_from(&mut buf) {
                    Ok(r) => r,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        warn!("udp receive failed: {e}");
                        continue;
                    }
                };
                if let Some(reply) = d.handle(&buf[..n], peer, local, Transport::Udp)
                    && let Err(e) = sock.send_to(&reply, peer) {
                        debug!("udp reply to {peer} failed: {e}");
                    }
            }
        })?;
    }
    Ok(())
}

/// Raise a UDP socket's buffer sizes. macOS caps a datagram at the send
/// buffer size (9 KiB by default), which would make 32 KiB READ replies fail
/// with EMSGSIZE; bigger receive buffers also ride out bursts of WRITEs.
#[cfg(unix)]
pub fn grow_udp_buffers(sock: &UdpSocket) {
    use std::os::fd::AsRawFd;
    let size: libc::c_int = 512 * 1024;
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        // SAFETY: setsockopt reads `size` for the duration of the call.
        let rc = unsafe {
            libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                &size as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            warn!("could not enlarge UDP socket buffer: {}", io::Error::last_os_error());
        }
    }
}

/// Windows' defaults already allow 64 KiB datagrams.
#[cfg(not(unix))]
pub fn grow_udp_buffers(_sock: &UdpSocket) {}

/// Serve RPC over TCP: one thread per connection.
pub fn spawn_tcp(listener: TcpListener, dispatcher: Arc<Dispatcher>) -> io::Result<()> {
    let port = listener.local_addr()?.port();
    thread::Builder::new().name(format!("tcp-{port}")).spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    warn!("tcp accept failed: {e}");
                    continue;
                }
            };
            let d = dispatcher.clone();
            let _ = thread::Builder::new().name(format!("tcp-{port}-conn")).spawn(move || {
                let peer = stream.peer_addr().ok();
                if let Err(e) = serve_connection(stream, &d) {
                    debug!("tcp connection from {peer:?} ended: {e}");
                }
            });
        }
    })?;
    Ok(())
}

fn serve_connection(mut stream: TcpStream, d: &Dispatcher) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let peer = stream.peer_addr()?;
    let local = stream.local_addr().ok();
    let mut reader = io::BufReader::new(stream.try_clone()?);
    debug!("tcp connection from {peer}");
    while let Some(msg) = read_record(&mut reader)? {
        if let Some(reply) = d.handle(&msg, peer, local, Transport::Tcp) {
            write_record(&mut stream, &reply)?;
        }
    }
    Ok(())
}

/// Read one record-marked RPC message (RFC 5531 §11). `None` at a clean EOF.
pub fn read_record(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut msg = Vec::new();
    loop {
        let mut hdr = [0u8; 4];
        if let Err(e) = r.read_exact(&mut hdr) {
            return if e.kind() == io::ErrorKind::UnexpectedEof && msg.is_empty() { Ok(None) } else { Err(e) };
        }
        let h = u32::from_be_bytes(hdr);
        let len = (h & 0x7fff_ffff) as usize;
        if msg.len() + len > MAX_RECORD {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "rpc record too large"));
        }
        let start = msg.len();
        msg.resize(start + len, 0);
        r.read_exact(&mut msg[start..])?;
        if h & 0x8000_0000 != 0 {
            return Ok(Some(msg));
        }
    }
}

pub fn write_record(w: &mut impl Write, msg: &[u8]) -> io::Result<()> {
    let mut out = Vec::with_capacity(msg.len() + 4);
    out.extend_from_slice(&(0x8000_0000 | msg.len() as u32).to_be_bytes());
    out.extend_from_slice(msg);
    w.write_all(&out)
}

/// Bind UDP and TCP on the same port. Port 0 picks a free port for both.
pub fn bind_pair(addr: SocketAddr) -> io::Result<(UdpSocket, TcpListener)> {
    let mut last_err = None;
    for _ in 0..16 {
        let tcp = TcpListener::bind(addr)?;
        let port = tcp.local_addr()?.port();
        match UdpSocket::bind(SocketAddr::new(addr.ip(), port)) {
            Ok(udp) => return Ok((udp, tcp)),
            Err(e) if addr.port() == 0 => last_err = Some(e), // try another port
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap())
}
