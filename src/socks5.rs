//! SOCKS5, both directions, plus the mixed local proxy:
//! * **Client** (`socks5_connect`): used when a front SOCKS5 proxy is
//!   configured — the TCP connection to the VPNGate node (and to
//!   vpngate.net / DoH) goes through it.
//! * **Server** (`serve_mixed`): after the tunnel is up, a local
//!   proxy listener (default 127.0.0.1:10818) that sniffs the first
//!   byte of each connection — 0x05 goes to the SOCKS5 handler, an
//!   ASCII letter goes to the HTTP proxy handler, anything else is
//!   closed silently. Both forward through the tunnel as their own
//!   userspace TCP sessions, so one port serves SOCKS5 and HTTP
//!   (NekoBox-style "Mixed (SOCKS+HTTP)") and LAN sharing needs no
//!   second port.
//!
//! The HTTP proxy supports CONNECT (tunnel mode: 200, then raw bytes
//! both ways — HTTPS goes this way) and plain HTTP requests in
//! absolute form (`GET http://host:port/path ...`) or origin form
//! with a Host header (rewritten to origin-form and forwarded; the
//! response is streamed back and the connection closed — no
//! keep-alive reuse). No HTTP authentication: `Proxy-Authorization`
//! is ignored. The tunnel is IPv4-only, so IPv6 targets are refused,
//! same as the SOCKS5 path.
//!
//! Every accepted connection is logged once (`ConnLog`): source
//! address (127.0.0.1 vs a LAN address), sniffed protocol, and the
//! CONNECT / SOCKS5 target host — never a path, a body, or
//! credentials — plus a failure line when the connection dies. The
//! GUI feeds these lines into its log panel + `vpngate-gui.log`, so
//! a LAN device that cannot reach the proxy leaves no line at all
//! (blocked before us, e.g. by the Windows firewall), while a
//! connection that arrives but fails names the step it died on.

use crate::sstp::SstpTunnel;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Connect to `host:port` through a SOCKS5 proxy at `proxy_addr`.
/// Supports no-auth and username/password auth.
pub async fn socks5_connect(
    proxy_addr: &str,
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
) -> std::io::Result<TcpStream> {
    let mut s = TcpStream::connect(proxy_addr).await?;
    if let (Some(u), Some(p)) = (username, password) {
        s.write_all(&[0x05, 0x02, 0x00, 0x02]).await?;
        let mut resp = [0u8; 2];
        s.read_exact(&mut resp).await?;
        if resp != [0x05, 0x02] {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "SOCKS5 proxy rejected username/password auth",
            ));
        }
        let mut auth = vec![0x01, u.len() as u8];
        auth.extend_from_slice(u.as_bytes());
        auth.push(p.len() as u8);
        auth.extend_from_slice(p.as_bytes());
        s.write_all(&auth).await?;
        let mut aresp = [0u8; 2];
        s.read_exact(&mut aresp).await?;
        if aresp != [0x01, 0x00] {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "SOCKS5 proxy authentication failed",
            ));
        }
    } else {
        s.write_all(&[0x05, 0x01, 0x00]).await?;
        let mut resp = [0u8; 2];
        s.read_exact(&mut resp).await?;
        if resp != [0x05, 0x00] {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "SOCKS5 proxy rejected no-auth",
            ));
        }
    }
    // CONNECT, address type 3 (domain).
    let mut req = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).await?;
    let mut hdr = [0u8; 4];
    s.read_exact(&mut hdr).await?;
    if hdr[0] != 0x05 || hdr[1] != 0x00 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            format!("SOCKS5 proxy CONNECT failed, reply code {:#04x}", hdr[1]),
        ));
    }
    let skip = match hdr[3] {
        0x01 => 4,
        0x03 => {
            let mut l = [0u8; 1];
            s.read_exact(&mut l).await?;
            l[0] as usize
        }
        0x04 => 16,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad SOCKS5 reply address type",
            ));
        }
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await?;
    Ok(s)
}

/// Serve the local mixed proxy (SOCKS5 + HTTP) on `listener`,
/// forwarding connections through `tunnel`. Runs until the listener
/// errors or the task is aborted. Each connection gets its own
/// userspace TCP session.
/// Where per-connection lines from the mixed proxy go. The GUI
/// passes a sender that feeds its log panel + file log
/// (`GuiEvent::ConnLog`); `None` logs to stderr (the CLI). One line
/// per accepted connection — source address, sniffed protocol, and
/// the CONNECT / SOCKS5 target host — plus a second line only when
/// the connection fails, carrying the reason. Never a path, a body,
/// or credentials: a CONNECT line names the host, nothing else.
pub type ConnLog = tokio::sync::mpsc::UnboundedSender<String>;

fn emit_conn_log(sink: &Option<ConnLog>, line: String) {
    match sink {
        Some(tx) => {
            let _ = tx.send(line);
        }
        None => eprintln!("{}", line),
    }
}

fn peer_of(socket: &TcpStream) -> String {
    socket
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".to_string())
}

pub async fn serve_mixed(
    listener: TcpListener,
    tunnel: Arc<SstpTunnel>,
    timeout: Duration,
    conn_log: Option<ConnLog>,
) {
    println!(
        "混合代理（SOCKS5+HTTP）listening on {}",
        listener.local_addr().unwrap()
    );
    println!("Configure your apps / browser to use this proxy (SOCKS5 or HTTP). Ctrl-C to stop.");
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("mixed proxy accept error: {}", e);
                continue;
            }
        };
        let t = tunnel.clone();
        let log = conn_log.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_mixed_conn(socket, t, timeout, &log).await {
                emit_conn_log(&log, format!("混合代理连接 {} 失败：{}", peer, e));
            }
        });
    }
}

/// What the first byte of a local-proxy connection means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstByte {
    /// 0x05: a SOCKS5 greeting.
    Socks5,
    /// An ASCII letter: an HTTP request line (CONNECT, GET, ...).
    Http,
    /// Anything else: not a protocol we serve; close silently.
    Unknown,
}

/// Classify the first byte of a connection to the local proxy.
fn classify_first_byte(b: u8) -> FirstByte {
    if b == 0x05 {
        FirstByte::Socks5
    } else if b.is_ascii_alphabetic() {
        FirstByte::Http
    } else {
        FirstByte::Unknown
    }
}

/// One accepted connection: sniff the first byte (`peek`, so the
/// chosen handler still sees the whole stream) and dispatch.
async fn handle_mixed_conn(
    local: TcpStream,
    tunnel: Arc<SstpTunnel>,
    timeout: Duration,
    conn_log: &Option<ConnLog>,
) -> Result<(), String> {
    let peer = peer_of(&local);
    let mut first = [0u8; 1];
    let n = local.peek(&mut first).await.map_err(|e| e.to_string())?;
    if n == 0 {
        emit_conn_log(
            conn_log,
            format!("混合代理连接 {}：未发送数据，已关闭", peer),
        );
        return Ok(()); // client closed immediately
    }
    match classify_first_byte(first[0]) {
        FirstByte::Socks5 => handle_socks5_conn(local, tunnel, timeout, conn_log).await,
        FirstByte::Http => handle_http_conn(local, tunnel, timeout, conn_log).await,
        FirstByte::Unknown => {
            emit_conn_log(
                conn_log,
                format!(
                    "混合代理连接 {}：未知协议（首字节 0x{:02x}），已关闭",
                    peer, first[0]
                ),
            );
            Ok(())
        }
    }
}

async fn handle_socks5_conn(
    mut local: TcpStream,
    tunnel: Arc<SstpTunnel>,
    timeout: Duration,
    conn_log: &Option<ConnLog>,
) -> Result<(), String> {
    // Greeting.
    let mut hdr = [0u8; 2];
    local
        .read_exact(&mut hdr)
        .await
        .map_err(|e| e.to_string())?;
    if hdr[0] != 0x05 {
        return Err("not a SOCKS5 client".to_string());
    }
    let n = hdr[1] as usize;
    let mut methods = vec![0u8; n];
    local
        .read_exact(&mut methods)
        .await
        .map_err(|e| e.to_string())?;
    if !methods.contains(&0x00) {
        local
            .write_all(&[0x05, 0xFF])
            .await
            .map_err(|e| e.to_string())?;
        return Err("SOCKS5 client offered no no-auth method".to_string());
    }
    local
        .write_all(&[0x05, 0x00])
        .await
        .map_err(|e| e.to_string())?;
    // Request.
    let mut rhdr = [0u8; 4];
    local
        .read_exact(&mut rhdr)
        .await
        .map_err(|e| e.to_string())?;
    if rhdr[0] != 0x05 || rhdr[1] != 0x01 {
        let _ = local
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return Err(format!("unsupported SOCKS5 command {}", rhdr[1]));
    }
    let (host, port) = match rhdr[3] {
        0x01 => {
            let mut b = [0u8; 6];
            local.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            (
                format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]),
                u16::from_be_bytes([b[4], b[5]]),
            )
        }
        0x03 => {
            let mut l = [0u8; 1];
            local.read_exact(&mut l).await.map_err(|e| e.to_string())?;
            let mut b = vec![0u8; l[0] as usize + 2];
            local.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            let name = String::from_utf8_lossy(&b[..l[0] as usize]).to_string();
            let p = u16::from_be_bytes([b[l[0] as usize], b[l[0] as usize + 1]]);
            (name, p)
        }
        0x04 => {
            let _ = local
                .write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            return Err("IPv6 targets are not supported by the tunnel (IPv4 only)".to_string());
        }
        t => {
            let _ = local
                .write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            return Err(format!("unsupported SOCKS5 address type {}", t));
        }
    };
    emit_conn_log(
        conn_log,
        format!(
            "混合代理连接 {}：SOCKS5 → {}:{}",
            peer_of(&local),
            host,
            port
        ),
    );
    if tunnel.is_dead() {
        let _ = local
            .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return Err("tunnel is dead".to_string());
    }
    let mut remote = match tunnel.open_stream(&host, port, timeout).await {
        Ok(s) => s,
        Err(e) => {
            let _ = local
                .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            return Err(format!("tunnel TCP to {}:{} failed: {}", host, port, e));
        }
    };
    local
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await
        .map_err(|e| e.to_string())?;
    let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP proxy (one half of the mixed local server)
// ---------------------------------------------------------------------------

/// Max HTTP header block we will read (64 KiB); larger → close.
/// Max request body we will forward (64 MiB); larger → 413/close.
const HTTP_MAX_HEADERS: usize = 64 * 1024;
const HTTP_MAX_BODY: usize = 64 * 1024 * 1024;

/// Split an HTTP request line into (method, request-uri). `None`
/// when the line is not `METHOD SP URI SP HTTP/x.y`.
fn parse_http_request_line(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let uri = parts.next()?;
    let version = parts.next()?;
    if !version.starts_with("HTTP/") || parts.next().is_some() {
        return None;
    }
    Some((method.to_string(), uri.to_string()))
}

/// Parse `host`, `host:port`, or `user@host:port` (userinfo stripped).
/// IPv6 literals are refused — the tunnel is IPv4-only, same as the
/// SOCKS5 path. Returns None on garbage.
fn parse_authority(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() || authority.starts_with('[') {
        return None;
    }
    // A bare host or host:port has at most one ':'.
    if authority.matches(':').count() > 1 {
        return None;
    }
    match authority.split_once(':') {
        Some((host, p)) => {
            if host.is_empty() {
                return None;
            }
            Some((host.to_string(), p.parse::<u16>().ok()?))
        }
        None => Some((authority.to_string(), default_port)),
    }
}

/// Resolve an HTTP proxy target to (host, port):
/// * CONNECT → the authority itself (default port 443).
/// * Absolute-form `http://host[:port]/...` → that host (default 80).
/// * Origin-form (`/path...`) → the Host header (default 80).
/// Anything else (other schemes, missing Host) → None.
fn http_target(method: &str, uri: &str, host_header: Option<&str>) -> Option<(String, u16)> {
    if method.eq_ignore_ascii_case("CONNECT") {
        return parse_authority(uri, 443);
    }
    if let Some(rest) = uri.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or("");
        return parse_authority(authority, 80);
    }
    if uri.starts_with('/') {
        let authority = host_header?;
        return parse_authority(authority, 80);
    }
    None
}

/// Origin-form path for a forwarded request: absolute-form
/// `http://host[:port]/path...` → `/path...` ("/" when empty);
/// origin-form URIs pass through unchanged.
fn origin_form_path(uri: &str) -> String {
    if let Some(rest) = uri.strip_prefix("http://") {
        match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None => "/".to_string(),
        }
    } else {
        uri.to_string()
    }
}

/// Parsed HTTP request head: the request line plus (name, value)
/// pairs (names lowercased for matching, values trimmed).
struct HttpRequestHead {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
}

impl HttpRequestHead {
    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse a header block (request line + `Name: value` lines) into a
/// head. `None` on a malformed request line or a header line without
/// a colon.
fn parse_http_head(block: &[u8]) -> Option<HttpRequestHead> {
    let text = String::from_utf8_lossy(block);
    let mut lines = text.lines();
    let request_line = lines.next()?;
    let (method, uri) = parse_http_request_line(request_line)?;
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((
            name.trim().to_ascii_lowercase(),
            value.trim().to_string(),
        ));
    }
    Some(HttpRequestHead {
        method,
        uri,
        headers,
    })
}

/// Rebuild the header block for the origin server: request line in
/// origin-form, all headers except Proxy-Authorization,
/// Proxy-Connection and Connection (replaced with
/// `Connection: close`), and any Keep-Alive dropped.
fn build_forward_head(req: &HttpRequestHead, origin_path: &str) -> Vec<u8> {
    let mut out = format!("{} {} HTTP/1.1\r\n", req.method, origin_path);
    for (name, value) in &req.headers {
        match name.as_str() {
            "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive" => {}
            _ => out.push_str(&format!("{}: {}\r\n", name, value)),
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

/// First index of `needle` in `hay`, if present.
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read one HTTP header block (up to and including the first blank
/// line). Returns (header_bytes, leftover_bytes): leftover is
/// whatever was already read past the header terminator (start of
/// the body / TLS handshake) and must be forwarded, never dropped.
/// Errors on EOF before the terminator or an oversized block.
async fn read_http_headers<R: AsyncRead + Unpin>(
    stream: &mut R,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = find_subslice(&buf, b"\r\n\r\n") {
            let headers = buf[..end + 4].to_vec();
            let leftover = buf[end + 4..].to_vec();
            return Ok((headers, leftover));
        }
        if buf.len() > HTTP_MAX_HEADERS {
            return Err("HTTP header block too large".to_string());
        }
        let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before HTTP headers finished".to_string());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// A byte source that first drains a leftover prefix, then reads
/// from the socket. Lets the chunked-body reader reuse bytes that
/// were already read past the header block.
struct PrefixedReader<'a, R: AsyncRead + Unpin> {
    prefix: Vec<u8>,
    pos: usize,
    stream: &'a mut R,
}

impl<'a, R: AsyncRead + Unpin> PrefixedReader<'a, R> {
    async fn read_line(&mut self) -> Result<Vec<u8>, String> {
        let mut line = Vec::new();
        loop {
            while self.pos < self.prefix.len() {
                let b = self.prefix[self.pos];
                self.pos += 1;
                line.push(b);
                if b == b'\n' {
                    return Ok(line);
                }
            }
            let mut one = [0u8; 1];
            let n = self
                .stream
                .read(&mut one)
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("connection closed mid-chunk".to_string());
            }
            line.push(one[0]);
            if one[0] == b'\n' {
                return Ok(line);
            }
            if line.len() > 8192 {
                return Err("chunk-size line too long".to_string());
            }
        }
    }

    async fn read_exact_into(&mut self, buf: &mut [u8]) -> Result<(), String> {
        let mut filled = 0;
        while filled < buf.len() {
            if self.pos < self.prefix.len() {
                let take = (self.prefix.len() - self.pos).min(buf.len() - filled);
                buf[filled..filled + take]
                    .copy_from_slice(&self.prefix[self.pos..self.pos + take]);
                self.pos += take;
                filled += take;
                continue;
            }
            let n = self
                .stream
                .read(&mut buf[filled..])
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("connection closed mid-chunk".to_string());
            }
            filled += n;
        }
        Ok(())
    }
}

/// Forward a chunked request body (framing preserved verbatim),
/// reading from `local` (with `leftover` bytes first) to `remote`.
async fn forward_chunked_body<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    local: &mut R,
    remote: &mut W,
    leftover: Vec<u8>,
) -> Result<(), String> {
    let mut r = PrefixedReader {
        prefix: leftover,
        pos: 0,
        stream: local,
    };
    let mut total: usize = 0;
    loop {
        let line = r.read_line().await?;
        // Chunk-size line may carry extensions after ';'.
        let size_str = String::from_utf8_lossy(&line)
            .trim()
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        let size = usize::from_str_radix(&size_str, 16)
            .map_err(|_| "bad chunk size".to_string())?;
        remote
            .write_all(&line)
            .await
            .map_err(|e| e.to_string())?;
        total = total
            .checked_add(size)
            .ok_or("chunked body too large")?;
        if total > HTTP_MAX_BODY {
            return Err("HTTP request body too large".to_string());
        }
        if size == 0 {
            // Trailers: forward lines until the blank line.
            loop {
                let tline = r.read_line().await?;
                let done = tline == b"\r\n" || tline == b"\n";
                remote
                    .write_all(&tline)
                    .await
                    .map_err(|e| e.to_string())?;
                if done {
                    return Ok(());
                }
            }
        }
        let mut buf = vec![0u8; size];
        r.read_exact_into(&mut buf).await?;
        remote
            .write_all(&buf)
            .await
            .map_err(|e| e.to_string())?;
        // Trailing CRLF after the chunk data.
        let mut crlf = [0u8; 2];
        r.read_exact_into(&mut crlf).await?;
        remote
            .write_all(&crlf)
            .await
            .map_err(|e| e.to_string())?;
    }
}

/// Forward a request body from `local` to `remote`, honouring
/// Content-Length or chunked framing. `leftover` is bytes already
/// read past the headers. No body declared → nothing to forward.
async fn forward_http_body<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    local: &mut R,
    remote: &mut W,
    req: &HttpRequestHead,
    leftover: Vec<u8>,
) -> Result<(), String> {
    if let Some(len) = req
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > HTTP_MAX_BODY {
            return Err("HTTP request body too large".to_string());
        }
        let mut remaining = len;
        // First the bytes we already hold.
        let take = remaining.min(leftover.len());
        if take > 0 {
            remote
                .write_all(&leftover[..take])
                .await
                .map_err(|e| e.to_string())?;
            remaining -= take;
        }
        let mut chunk = [0u8; 8192];
        while remaining > 0 {
            let want = remaining.min(chunk.len());
            let n = local
                .read(&mut chunk[..want])
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("connection closed mid-body".to_string());
            }
            remote
                .write_all(&chunk[..n])
                .await
                .map_err(|e| e.to_string())?;
            remaining -= n;
        }
        return Ok(());
    }
    if req
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        return forward_chunked_body(local, remote, leftover).await;
    }
    // No body declared; bytes already read past the headers (if any)
    // have no framing to honour, so they are dropped.
    Ok(())
}

async fn handle_http_conn(
    mut local: TcpStream,
    tunnel: Arc<SstpTunnel>,
    timeout: Duration,
    conn_log: &Option<ConnLog>,
) -> Result<(), String> {
    let peer = peer_of(&local);
    let (head_bytes, leftover) = match read_http_headers(&mut local).await {
        Ok(v) => v,
        Err(_) => {
            // Malformed / empty request: close without a reply.
            emit_conn_log(
                conn_log,
                format!("混合代理连接 {}：HTTP 请求头未读完，已关闭", peer),
            );
            return Ok(());
        }
    };
    let Some(req) = parse_http_head(&head_bytes) else {
        let _ = local
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        emit_conn_log(
            conn_log,
            format!("混合代理连接 {}：HTTP 请求行畸形，已回 400", peer),
        );
        return Ok(());
    };
    let host_header = req.get("host").map(|s| s.to_string());
    let Some((host, port)) = http_target(&req.method, &req.uri, host_header.as_deref()) else {
        let _ = local
            .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        emit_conn_log(
            conn_log,
            format!("混合代理连接 {}：HTTP 请求目标无法解析，已回 400", peer),
        );
        return Ok(());
    };
    // The connection line: source, protocol, target host only —
    // never the path or the body (see `ConnLog`).
    if req.method.eq_ignore_ascii_case("CONNECT") {
        emit_conn_log(
            conn_log,
            format!("混合代理连接 {}：HTTP CONNECT → {}:{}", peer, host, port),
        );
    } else {
        emit_conn_log(
            conn_log,
            format!(
                "混合代理连接 {}：HTTP {} → {}:{}",
                peer,
                req.method.to_ascii_uppercase(),
                host,
                port
            ),
        );
    }
    let mut remote = match tunnel.open_stream(&host, port, timeout).await {
        Ok(s) => s,
        Err(e) => {
            let _ = local
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
            return Err(format!("tunnel TCP to {}:{} failed: {}", host, port, e));
        }
    };
    if req.method.eq_ignore_ascii_case("CONNECT") {
        // Tunnel mode: 200, then raw bytes both ways (TLS inside).
        local
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
        if !leftover.is_empty() {
            remote
                .write_all(&leftover)
                .await
                .map_err(|e| e.to_string())?;
        }
        let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
        return Ok(());
    }
    // Plain HTTP: rewrite to origin-form, forward, then stream the
    // response back and close (no keep-alive reuse).
    let origin_path = origin_form_path(&req.uri);
    let forward = build_forward_head(&req, &origin_path);
    remote
        .write_all(&forward)
        .await
        .map_err(|e| e.to_string())?;
    if let Err(e) = forward_http_body(&mut local, &mut remote, &req, leftover).await {
        return Err(e);
    }
    // One-way copy: the origin server closes after the response (we
    // sent Connection: close), then the client side is closed.
    let _ = tokio::io::copy(&mut remote, &mut local).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[test]
    fn classify_first_byte_routes_protocols() {
        assert_eq!(classify_first_byte(0x05), FirstByte::Socks5);
        for m in [b'C', b'G', b'P', b'H', b'D', b'O', b'T', b'c', b'g'] {
            assert_eq!(classify_first_byte(m), FirstByte::Http, "byte {:#04x}", m);
        }
        for b in [0x00, 0x01, 0x04, 0x16, 0xFF] {
            assert_eq!(classify_first_byte(b), FirstByte::Unknown, "byte {:#04x}", b);
        }
    }

    #[test]
    fn parse_http_request_line_edges() {
        assert_eq!(
            parse_http_request_line("GET http://example.com/ HTTP/1.1"),
            Some(("GET".to_string(), "http://example.com/".to_string()))
        );
        assert_eq!(
            parse_http_request_line("CONNECT example.com:443 HTTP/1.1"),
            Some(("CONNECT".to_string(), "example.com:443".to_string()))
        );
        assert_eq!(
            parse_http_request_line("GET / HTTP/1.0"),
            Some(("GET".to_string(), "/".to_string()))
        );
        assert!(parse_http_request_line("GET").is_none());
        assert!(parse_http_request_line("FOO BAR").is_none());
        assert!(parse_http_request_line("GET / HTTP/1.1 EXTRA").is_none());
        assert!(parse_http_request_line("").is_none());
    }

    #[test]
    fn parse_authority_edges() {
        assert_eq!(
            parse_authority("example.com", 80),
            Some(("example.com".to_string(), 80))
        );
        assert_eq!(
            parse_authority("example.com:443", 80),
            Some(("example.com".to_string(), 443))
        );
        assert_eq!(
            parse_authority("user@example.com:8080", 80),
            Some(("example.com".to_string(), 8080))
        );
        assert!(parse_authority("[::1]:8080", 80).is_none(), "IPv6 refused");
        assert!(parse_authority("a:b:c", 80).is_none());
        assert!(parse_authority(":80", 80).is_none());
        assert!(parse_authority("", 80).is_none());
        assert!(parse_authority("example.com:0", 80).is_some(), "port 0 parses");
    }

    #[test]
    fn http_target_edges() {
        assert_eq!(
            http_target("CONNECT", "example.com:443", None),
            Some(("example.com".to_string(), 443))
        );
        assert_eq!(
            http_target("CONNECT", "example.com", None),
            Some(("example.com".to_string(), 443))
        );
        assert_eq!(
            http_target("GET", "http://example.com:8080/a?b=c", None),
            Some(("example.com".to_string(), 8080))
        );
        assert_eq!(
            http_target("GET", "http://example.com/a", None),
            Some(("example.com".to_string(), 80))
        );
        assert_eq!(
            http_target("GET", "http://example.com", None),
            Some(("example.com".to_string(), 80))
        );
        assert_eq!(
            http_target("GET", "/a", Some("example.com:81")),
            Some(("example.com".to_string(), 81))
        );
        assert_eq!(http_target("GET", "/a", None), None);
        assert_eq!(http_target("GET", "https://example.com/a", None), None);
        assert_eq!(http_target("GET", "ftp://example.com/", None), None);
    }

    #[test]
    fn origin_form_path_edges() {
        assert_eq!(origin_form_path("http://h:80/a?b"), "/a?b");
        assert_eq!(origin_form_path("http://h"), "/");
        assert_eq!(origin_form_path("http://h/"), "/");
        assert_eq!(origin_form_path("/a"), "/a");
    }

    #[test]
    fn build_forward_head_drops_proxy_headers() {
        let req = HttpRequestHead {
            method: "GET".to_string(),
            uri: "http://example.com/a".to_string(),
            headers: vec![
                ("host".to_string(), "example.com".to_string()),
                (
                    "proxy-authorization".to_string(),
                    "Basic eA==".to_string(),
                ),
                ("proxy-connection".to_string(), "keep-alive".to_string()),
                ("connection".to_string(), "keep-alive".to_string()),
                ("accept".to_string(), "*/*".to_string()),
            ],
        };
        let out = String::from_utf8(build_forward_head(&req, "/a")).unwrap();
        assert!(out.starts_with("GET /a HTTP/1.1\r\n"), "request line: {}", out);
        assert!(out.contains("host: example.com\r\n"), "keeps Host: {}", out);
        assert!(out.contains("accept: */*\r\n"), "keeps Accept: {}", out);
        assert!(!out.contains("proxy-authorization"), "drops auth: {}", out);
        assert!(!out.contains("proxy-connection"), "drops proxy-connection: {}", out);
        assert!(out.contains("Connection: close\r\n"), "forces close: {}", out);
        assert_eq!(out.matches("Connection: close").count(), 1, "one close: {}", out);
    }

    #[tokio::test]
    async fn read_http_headers_splits_leftover() {
        let (mut a, mut b) = duplex(65536);
        a.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\nBODYPART")
            .await
            .unwrap();
        drop(a);
        let (head, leftover) = read_http_headers(&mut b).await.unwrap();
        assert_eq!(head, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(leftover, b"BODYPART");
    }

    #[tokio::test]
    async fn read_http_headers_errors_on_eof() {
        let (mut a, mut b) = duplex(65536);
        a.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n").await.unwrap();
        drop(a);
        assert!(read_http_headers(&mut b).await.is_err());
    }

    #[tokio::test]
    async fn forward_http_body_content_length() {
        let (mut client, mut server) = duplex(65536);
        let (mut sink_a, mut sink_b) = duplex(65536);
        let req = HttpRequestHead {
            method: "POST".to_string(),
            uri: "http://example.com/a".to_string(),
            headers: vec![("content-length".to_string(), "5".to_string())],
        };
        // Body arrives split: "HE" already past the headers (leftover),
        // "LLO" still in the stream.
        client.write_all(b"LLO").await.unwrap();
        drop(client);
        forward_http_body(&mut server, &mut sink_a, &req, b"HE".to_vec())
            .await
            .unwrap();
        drop(sink_a);
        let mut got = Vec::new();
        sink_b.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"HELLO");
    }

    #[tokio::test]
    async fn forward_http_body_chunked() {
        let (mut client, mut server) = duplex(65536);
        let (mut sink_a, mut sink_b) = duplex(65536);
        let req = HttpRequestHead {
            method: "POST".to_string(),
            uri: "http://example.com/a".to_string(),
            headers: vec![("transfer-encoding".to_string(), "chunked".to_string())],
        };
        // "5\r\nHELLO\r\n0\r\n\r\n" split across writes.
        client.write_all(b"5\r\nHE").await.unwrap();
        client.write_all(b"LLO\r\n0\r\n\r\n").await.unwrap();
        drop(client);
        forward_http_body(&mut server, &mut sink_a, &req, Vec::new())
            .await
            .unwrap();
        drop(sink_a);
        let mut got = Vec::new();
        sink_b.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"5\r\nHELLO\r\n0\r\n\r\n");
    }
}
