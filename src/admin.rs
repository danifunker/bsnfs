//! The admin web UI: a single page plus a small JSON API, served by a
//! minimal HTTP/1.1 server (one request per connection).
//!
//! State-changing requests must carry an `X-Requested-With: bsnfs` header.
//! Browsers won't add that header to a cross-site form post, so another web
//! page can't drive the API through the user's browser.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::app::{PortmapState, Server};

const PAGE: &str = include_str!("admin.html");
const MAX_BODY: usize = 64 * 1024;

pub fn spawn(listener: TcpListener, server: Arc<Server>) -> io::Result<()> {
    std::thread::Builder::new().name("admin".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let server = server.clone();
            let _ = std::thread::Builder::new().name("admin-conn".into()).spawn(move || {
                if let Err(e) = handle(stream, &server) {
                    debug!("admin: {e}");
                }
            });
        }
    })?;
    Ok(())
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    extra: Vec<(&'static str, String)>,
}

impl Response {
    fn json(status: u16, body: String) -> Self {
        Self { status, content_type: "application/json", body: body.into_bytes(), extra: Vec::new() }
    }

    fn error(status: u16, msg: &str) -> Self {
        Self::json(status, format!("{{\"error\":{}}}", js(msg)))
    }
}

fn handle(stream: TcpStream, server: &Server) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(req) = read_request(&mut reader)? else { return Ok(()) };
    let resp = route(&req, server);
    let mut out = stream;
    let reason = match resp.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        resp.status,
        resp.content_type,
        resp.body.len()
    );
    for (k, v) in &resp.extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    out.write_all(&resp.body)
}

fn read_request(r: &mut impl BufRead) -> io::Result<Option<Request>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "bad request line"));
    };
    let method = method.to_string();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), parse_form(query.as_bytes()));
    let mut headers = HashMap::new();
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if headers.len() > 100 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "too many headers"));
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "body too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(Request {
        method,
        path,
        query,
        headers,
        body,
    }))
}

fn route(req: &Request, server: &Server) -> Response {
    if !host_allowed(req.headers.get("host").map(String::as_str), &server.cfg.admin_hosts) {
        return Response::error(
            403,
            "unexpected Host name: open this page by IP address, localhost or this machine's name (or allow the name with --admin-host)",
        );
    }
    if let Some(pw) = &server.cfg.admin_password
        && !authorized(req, pw) {
            let mut r = Response::error(401, "password required");
            r.extra.push(("WWW-Authenticate", "Basic realm=\"bsnfs\"".into()));
            return r;
        }
    let mutating = req.method == "POST";
    if mutating && req.headers.get("x-requested-with").map(String::as_str) != Some("bsnfs") {
        return Response::error(403, "missing X-Requested-With: bsnfs");
    }
    let form = parse_form(&req.body);
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => Response { status: 200, content_type: "text/html; charset=utf-8", body: PAGE.as_bytes().to_vec(), extra: Vec::new() },
        ("GET", "/api/status") => Response::json(200, status_json(req, server)),
        ("GET", "/api/browse") => browse(req.query.get("path").map(String::as_str), &server.cfg.share_roots),
        ("POST", "/api/exports/add") => add_export(server, &form),
        ("POST", "/api/exports/remove") => change_export(server, &form, |s, id| {
            let name = s.vfs.exports().into_iter().find(|(i, _)| *i == id).map(|(_, e)| e.name.clone());
            let ok = s.vfs.remove_export(id);
            if ok {
                info!("admin: stopped sharing {}", name.unwrap_or_default());
            }
            ok
        }),
        ("POST", "/api/exports/readonly") => {
            let ro = form.get("read_only").map(|v| v == "1" || v == "true").unwrap_or(false);
            change_export(server, &form, |s, id| s.vfs.set_read_only(id, ro))
        }
        _ => Response::error(404, "not found"),
    }
}

/// Whether the `Host` header names this machine. Without this check, a web
/// page on any site could point its own domain at 127.0.0.1 (DNS
/// rebinding) and drive this API from the visitor's browser.
fn host_allowed(host: Option<&str>, extra: &[String]) -> bool {
    let Some(host) = host else { return true }; // HTTP/1.0: no name to rebind
    let host = strip_port(host).trim_end_matches('.').to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() || host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    if extra.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return true;
    }
    let me = machine_name().to_ascii_lowercase();
    !me.is_empty() && (host == me || host.split('.').next() == me.split('.').next())
}

fn machine_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: gethostname writes at most buf.len() bytes.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8_lossy(&buf[..end]).into_owned();
        }
        String::new()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// Whether `path` lies inside one of the allowed share roots (all of the
/// filesystem when none are configured).
fn within_roots(path: &std::path::Path, roots: &[PathBuf]) -> bool {
    roots.is_empty() || roots.iter().any(|r| path.starts_with(r))
}

fn authorized(req: &Request, password: &str) -> bool {
    let Some(v) = req.headers.get("authorization") else { return false };
    let Some(b64) = v.strip_prefix("Basic ") else { return false };
    let Some(decoded) = base64_decode(b64.trim()) else { return false };
    let decoded = String::from_utf8_lossy(&decoded);
    let given = decoded.split_once(':').map(|(_, p)| p).unwrap_or("");
    // Compare without an early exit, so timing doesn't reveal a prefix match.
    given.len() == password.len() && given.bytes().zip(password.bytes()).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn add_export(server: &Server, form: &HashMap<String, String>) -> Response {
    let Some(path) = form.get("path").filter(|p| !p.is_empty()) else {
        return Response::error(400, "path is required");
    };
    let name = form.get("name").cloned().unwrap_or_default();
    let spec = if name.trim().is_empty() { path.clone() } else { format!("{}={path}", name.trim()) };
    let spec = match crate::config::export_spec(&spec) {
        Ok(s) => s,
        Err(e) => return Response::error(400, &e),
    };
    match std::fs::canonicalize(&spec.path) {
        Ok(p) if within_roots(&p, &server.cfg.share_roots) => {}
        Ok(p) => return Response::error(403, &format!("{} is outside the folders this server may share", p.display())),
        Err(e) => return Response::error(400, &format!("{}: {e}", spec.path.display())),
    }
    let read_only = form.get("read_only").map(|v| v == "1" || v == "true").unwrap_or(false);
    let _guard = server.exports_lock.lock().unwrap();
    match server.vfs.add_export(&spec.name, &spec.path, read_only) {
        Ok(id) => {
            info!("admin: sharing {} as {}{}", spec.path.display(), spec.name, if read_only { " (read-only)" } else { "" });
            saved(server, id)
        }
        Err(e) => Response::error(400, &e.to_string()),
    }
}

fn change_export(server: &Server, form: &HashMap<String, String>, f: impl FnOnce(&Server, usize) -> bool) -> Response {
    let Some(id) = form.get("id").and_then(|v| v.parse().ok()) else {
        return Response::error(400, "id is required");
    };
    let _guard = server.exports_lock.lock().unwrap();
    if !f(server, id) {
        return Response::error(404, "no such export");
    }
    saved(server, id)
}

fn saved(server: &Server, id: usize) -> Response {
    if let Err(e) = server.save_exports() {
        warn!("admin: could not save exports file: {e}");
        return Response::error(500, &format!("applied, but saving the exports file failed: {e}"));
    }
    Response::json(200, format!("{{\"ok\":true,\"id\":{id}}}"))
}

/// List the subdirectories of `path`, to help pick a folder to share.
/// Confined to the share roots, when there are any.
fn browse(path: Option<&str>, roots: &[PathBuf]) -> Response {
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    let path = match path.filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => roots.first().cloned().or_else(home).unwrap_or_else(|| "/".into()),
    };
    let path = match std::fs::canonicalize(&path) {
        Ok(p) if within_roots(&p, roots) => p,
        Ok(p) => return Response::error(403, &format!("{} is outside the folders this server may share", p.display())),
        Err(e) => return Response::error(400, &format!("{}: {e}", path.display())),
    };
    let mut dirs: Vec<String> = match std::fs::read_dir(&path) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect(),
        Err(e) => return Response::error(400, &format!("{}: {e}", path.display())),
    };
    dirs.sort_by_key(|d| d.to_lowercase());
    let parent = path
        .parent()
        .filter(|p| within_roots(p, roots))
        .map(|p| js(&p.to_string_lossy()))
        .unwrap_or_else(|| "null".into());
    let list: Vec<String> = dirs.iter().map(|d| js(d)).collect();
    Response::json(200, format!("{{\"path\":{},\"parent\":{parent},\"dirs\":[{}]}}", js(&path.to_string_lossy()), list.join(",")))
}

fn status_json(req: &Request, server: &Server) -> String {
    let now = SystemTime::now();
    let ago = |t: SystemTime| now.duration_since(t).unwrap_or_default().as_secs_f64();
    let stats = server.dispatcher.stats();
    let mounts = server.mount.mounts();
    let host = req.headers.get("host").map(|h| strip_port(h)).unwrap_or("SERVER").to_string();

    let exports: Vec<String> = server
        .vfs
        .exports()
        .into_iter()
        .map(|(id, e)| {
            format!(
                "{{\"id\":{id},\"name\":{},\"path\":{},\"read_only\":{},\"mounts\":{},\"mount_command\":{}}}",
                js(&e.name),
                js(&e.root.to_string_lossy()),
                e.read_only(),
                mounts.iter().filter(|m| m.export == id).count(),
                js(&server.mount_hint(&host, &e.name)),
            )
        })
        .collect();
    let mount_list: Vec<String> = mounts
        .iter()
        .map(|m| format!("{{\"client\":{},\"host\":{},\"path\":{},\"since\":{:.0}}}", js(&m.client.to_string()), js(&m.host), js(&m.path), ago(m.since)))
        .collect();
    let mut clients: Vec<_> = stats.clients.iter().collect();
    clients.sort_by_key(|c| std::cmp::Reverse(c.1.last_seen));
    let clients: Vec<String> = clients
        .iter()
        .map(|(ip, c)| {
            format!(
                "{{\"addr\":{},\"machine\":{},\"calls\":{},\"last_seen\":{:.1},\"first_seen\":{:.0}}}",
                js(&ip.to_string()),
                js(&c.machine),
                c.calls,
                ago(c.last_seen),
                ago(c.first_seen)
            )
        })
        .collect();
    let per_call: Vec<String> = stats.per_call.iter().map(|(k, v)| format!("{}:{v}", js(k))).collect();
    let recent: Vec<String> = stats
        .recent
        .iter()
        .rev()
        .take(50)
        .map(|r| {
            format!(
                "{{\"ago\":{:.1},\"peer\":{},\"transport\":{},\"call\":{},\"uid\":{},\"micros\":{}}}",
                ago(r.at),
                js(&r.peer.to_string()),
                js(r.transport.netid()),
                js(&r.call),
                r.uid,
                r.micros
            )
        })
        .collect();
    let p = &server.ports;
    let portmap = match p.portmap {
        PortmapState::Own(port) => format!("{{\"mode\":\"own\",\"port\":{port}}}"),
        PortmapState::System => "{\"mode\":\"system\",\"port\":111}".into(),
        PortmapState::None => "{\"mode\":\"off\",\"port\":null}".into(),
    };
    let versions: Vec<String> = server.cfg.versions.iter().map(u32::to_string).collect();
    format!(
        concat!(
            "{{\"version\":{},\"uptime\":{:.0},\"host\":{},\"bind\":{},",
            "\"ports\":{{\"nfs\":{},\"mount\":{},\"nlm\":{},\"portmap\":{}}},",
            "\"udp\":{},\"tcp\":{},\"versions\":[{}],\"exports_file\":{},",
            "\"exports\":[{}],\"mounts\":[{}],\"clients\":[{}],",
            "\"calls\":{},\"per_call\":{{{}}},\"recent\":[{}]}}"
        ),
        js(crate::VERSION),
        ago(stats.started),
        js(&host),
        js(&server.cfg.bind.to_string()),
        p.nfs,
        p.mount,
        p.nlm.map_or("null".into(), |n| n.to_string()),
        portmap,
        server.cfg.udp,
        server.cfg.tcp,
        versions.join(","),
        server.cfg.exports_file.as_ref().map_or("null".into(), |f| js(&f.to_string_lossy())),
        exports.join(","),
        mount_list.join(","),
        clients.join(","),
        stats.calls,
        per_call.join(","),
        recent.join(","),
    )
}

fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.split(']').next().map(|h| &h[1..]).unwrap_or(host);
    }
    host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
}

/// A JSON string literal.
fn js(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Keep `</script>` and friends inert if this ever lands in HTML.
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Parse `application/x-www-form-urlencoded` data.
fn parse_form(b: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(b)
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (url_decode(k), url_decode(v))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_helpers() {
        assert_eq!(base64_decode("YWRtaW46c2VjcmV0").unwrap(), b"admin:secret");
        assert_eq!(base64_decode("YQ==").unwrap(), b"a");
        assert_eq!(url_decode("a%20b+c%2F"), "a b c/");
        assert_eq!(url_decode("100%"), "100%");
        let f = parse_form(b"name=%2Fpics&path=%2Ftmp%2Fx&read_only=1");
        assert_eq!(f["name"], "/pics");
        assert_eq!(f["path"], "/tmp/x");
        assert_eq!(js("a\"b</script>"), "\"a\\\"b\\u003c/script>\"");
        assert_eq!(strip_port("host:8080"), "host");
        assert_eq!(strip_port("[::1]:8080"), "::1");
    }

    #[test]
    fn host_header_check_stops_dns_rebinding() {
        assert!(host_allowed(Some("127.0.0.1:8080"), &[]));
        assert!(host_allowed(Some("[::1]:8080"), &[]));
        assert!(host_allowed(Some("localhost:8080"), &[]));
        assert!(host_allowed(Some("192.168.1.20:8080"), &[]));
        assert!(host_allowed(Some("nas.local"), &[]));
        assert!(host_allowed(None, &[]));
        assert!(!host_allowed(Some("evil.example.com:8080"), &[]));
        assert!(host_allowed(Some("nas.home.arpa"), &["nas.home.arpa".to_string()]));
        let me = machine_name();
        if !me.is_empty() {
            assert!(host_allowed(Some(&format!("{me}:8080")), &[]));
        }
    }

    #[test]
    fn share_roots_confine_the_admin_page() {
        let roots = [PathBuf::from("/srv/nfs")];
        assert!(within_roots(std::path::Path::new("/srv/nfs/irix"), &roots));
        assert!(!within_roots(std::path::Path::new("/srv/nfsx"), &roots));
        assert!(!within_roots(std::path::Path::new("/etc"), &roots));
        assert!(within_roots(std::path::Path::new("/etc"), &[]));
    }
}
