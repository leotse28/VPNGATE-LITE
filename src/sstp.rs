//! MS-SSTP + PPP tunnel to a VPNGate node, and a userspace TCP stack
//! over it (ported from the JS reference `worker_stealth_sub.js`:
//! `openTun`/`establish`, `makeTcp`, `tunConn`, `probeGw`).
//!
//! Protocol summary (see also `scripts/probe_vpngate_sstp.py` in the
//! reference skill):
//! 1. TCP (+ optional front SOCKS5) to the node, wrapped in TLS.
//!    Nodes use self-signed certificates, so TLS verification is off.
//! 2. `SSTP_DUPLEX_POST /sra_{BA195980-CD49-458b-9E23-C84EE0ADCD75}/`
//!    with `Content-Length: 18446744073709551615`; server replies 200
//!    and the TLS stream becomes a full-duplex SSTP tunnel.
//! 3. SSTP control `MSG_CALL_CONNECT_REQUEST` (0x0001, attribute 1 =
//!    PPP), then PPP: LCP Configure-Request/Ack, PAP auth with the
//!    public `vpn`/`vpn`, IPCP Configure-Request/Nak/Ack → the server
//!    assigns a virtual IPv4. That IP is the tunnel's source address.
//! 4. Afterwards every TCP connection is a hand-built IPv4+TCP packet
//!    (SYN → SYN-ACK → ACK, 1300-byte PSH+ACK chunks, ACKs, FIN)
//!    wrapped as SSTP data packets (FF 03 + PPP protocol 0x0021).
//!    Only TCP is tunnelled; UDP is not (same limitation as the JS code).

use crate::doh::resolve_ipv4;
use crate::net::{accept_any_tls_config, Connector};
use crate::util::{checksum, random_bytes, random_uuid, u16be, u32be};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio_rustls::TlsConnector;

const SSTP_PATH: &str = "/sra_{BA195980-CD49-458b-9E23-C84EE0ADCD75}/";
const PPP_LCP: u16 = 0xC021;
const PPP_PAP: u16 = 0xC023;
const PPP_IPCP: u16 = 0x8021;
const PPP_IPV4: u16 = 0x0021;
const TCP_MSS: usize = 1300;
const MAX_PACKETS_ESTABLISH: usize = 200;

// ---------------------------------------------------------------------------
// SSTP / PPP frame builders (port of JS tunData / tunCtrl / linkFrame /
// authFrame).
// ---------------------------------------------------------------------------

fn sstp_data(ppp: &[u8]) -> Vec<u8> {
    let l = ppp.len() + 6;
    let mut f = Vec::with_capacity(l);
    f.extend_from_slice(&[0x10, 0x00, (((l >> 8) & 0x0F) | 0x80) as u8, (l & 0xFF) as u8, 0xFF, 0x03]);
    f.extend_from_slice(ppp);
    f
}

fn sstp_control(msg_type: u16, attrs: &[(u8, &[u8])]) -> Vec<u8> {
    let mut body_len = 0usize;
    for (_, d) in attrs {
        body_len += 4 + d.len();
    }
    let l = 8 + body_len;
    let mut f = Vec::with_capacity(l);
    let lv = (l as u16) | 0x8000;
    f.extend_from_slice(&[0x10, 0x01, (lv >> 8) as u8, (lv & 0xFF) as u8]);
    f.extend_from_slice(&msg_type.to_be_bytes());
    f.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    for (id, d) in attrs {
        f.push(0);
        f.push(*id);
        f.extend_from_slice(&((4 + d.len()) as u16).to_be_bytes());
        f.extend_from_slice(d);
    }
    f
}

fn link_frame(proto: u16, code: u8, id: u8, opts: &[(u8, &[u8])]) -> Vec<u8> {
    let mut ol = 0usize;
    for (_, d) in opts {
        ol += 2 + d.len();
    }
    let l = 4 + ol;
    let mut f = Vec::with_capacity(2 + l);
    f.extend_from_slice(&proto.to_be_bytes());
    f.push(code);
    f.push(id);
    f.extend_from_slice(&(l as u16).to_be_bytes());
    for (t, d) in opts {
        f.push(*t);
        f.push((2 + d.len()) as u8);
        f.extend_from_slice(d);
    }
    f
}

fn pap_frame(id: u8, user: &str, pass: &str) -> Vec<u8> {
    let ub = user.as_bytes();
    let pb = pass.as_bytes();
    let l = 6 + ub.len() + pb.len();
    let mut f = Vec::with_capacity(2 + l);
    f.extend_from_slice(&PPP_PAP.to_be_bytes());
    f.push(1);
    f.push(id);
    f.extend_from_slice(&(l as u16).to_be_bytes());
    f.push(ub.len() as u8);
    f.extend_from_slice(ub);
    f.push(pb.len() as u8);
    f.extend_from_slice(pb);
    f
}

fn find_opt<'a>(d: &'a [u8], t: u8) -> Option<&'a [u8]> {
    let mut i = 0usize;
    while i + 2 <= d.len() {
        let l = d[i + 1] as usize;
        if l < 2 || i + l > d.len() {
            return None;
        }
        if d[i] == t {
            return Some(&d[i + 2..i + l]);
        }
        i += l;
    }
    None
}

/// Parse a PPP payload (after the SSTP data header).
/// Returns (protocol, code, id, payload, raw_frame).
fn parse_link(d: &[u8]) -> Option<(u16, u8, u8, &[u8], &[u8])> {
    let o = if d.len() >= 2 && d[0] == 0xFF && d[1] == 0x03 { 2 } else { 0 };
    if d.len() - o < 4 {
        return None;
    }
    let pr = u16be(d, o);
    if pr == PPP_IPV4 {
        return Some((pr, 0, 0, &d[o + 2..], &d[o..]));
    }
    if d.len() - o < 6 {
        return None;
    }
    Some((pr, d[o + 2], d[o + 3], &d[o + 6..], &d[o..]))
}

// ---------------------------------------------------------------------------
// SSTP packet reader (shared by establish and the demuxer task).
// ---------------------------------------------------------------------------

struct PacketReader<R> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> PacketReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    async fn read_exact_buf(&mut self, n: usize) -> std::io::Result<Vec<u8>> {
        while self.buf.len() < n {
            let mut tmp = [0u8; 4096];
            let r = self.inner.read(&mut tmp).await?;
            if r == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "SSTP stream closed"));
            }
            self.buf.extend_from_slice(&tmp[..r]);
        }
        Ok(self.buf.drain(..n).collect())
    }

    /// Read one SSTP packet. Returns (is_control, body).
    async fn read_packet(&mut self) -> std::io::Result<(bool, Vec<u8>)> {
        let hd = self.read_exact_buf(4).await?;
        let total = (u16be(&hd, 2) & 0x0FFF) as usize;
        if total < 4 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad SSTP length"));
        }
        let body = self.read_exact_buf(total - 4).await?;
        Ok(((hd[1] & 1) == 1, body))
    }

    async fn read_line(&mut self) -> std::io::Result<String> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let s = String::from_utf8_lossy(&line).trim_end_matches(|c| c == '\r' || c == '\n').to_string();
                return Ok(s);
            }
            let mut tmp = [0u8; 4096];
            let r = self.inner.read(&mut tmp).await?;
            if r == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "SSTP stream closed while reading line"));
            }
            self.buf.extend_from_slice(&tmp[..r]);
        }
    }
}

// ---------------------------------------------------------------------------
// PPP establish (port of JS `establish`).
// ---------------------------------------------------------------------------

async fn ppp_establish<R, W>(reader: &mut PacketReader<R>, writer: &mut W, host: &str) -> Result<Ipv4Addr, String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut id: u8 = 0;
    let mut next_id = || {
        id = id.wrapping_add(1);
        id
    };
    let req = format!(
        "SSTP_DUPLEX_POST {} HTTP/1.1\r\nHost: {}\r\nContent-Length: 18446744073709551615\r\nSSTPCORRELATIONID: {{{}}}\r\n\r\n",
        SSTP_PATH,
        host,
        random_uuid()
    );
    let mru: [u8; 2] = [5, 220]; // MRU 1500
    let mut first = Vec::new();
    first.extend_from_slice(req.as_bytes());
    first.extend_from_slice(&sstp_control(0x0001, &[(1, &[0, 1])]));
    first.extend_from_slice(&sstp_data(&link_frame(PPP_LCP, 1, next_id(), &[(1, &mru)])));
    writer
        .write_all(&first)
        .await
        .map_err(|e| format!("SSTP write failed: {}", e))?;

    let status = reader.read_line().await.map_err(|e| format!("SSTP status read failed: {}", e))?;
    if !status.contains("200") {
        return Err(format!("SSTP server did not return 200 (got: {})", status));
    }
    loop {
        let line = reader.read_line().await.map_err(|e| format!("SSTP header read failed: {}", e))?;
        if line.is_empty() {
            break;
        }
    }

    let mut authed = false;
    for _ in 0..MAX_PACKETS_ESTABLISH {
        let (ctrl, body) = reader.read_packet().await.map_err(|e| format!("SSTP packet read failed: {}", e))?;
        if ctrl {
            if body.len() >= 2 && u16be(&body, 0) == 0x0008 {
                // Echo request -> echo reply.
                writer
                    .write_all(&sstp_control(0x0009, &[]))
                    .await
                    .map_err(|e| format!("SSTP echo reply failed: {}", e))?;
            }
            continue;
        }
        let Some((proto, code, _pid, payload, raw)) = parse_link(&body) else {
            continue;
        };
        if proto == PPP_IPV4 {
            continue;
        }
        if proto == PPP_LCP {
            if code == 1 {
                let mut ack = raw.to_vec();
                ack[2] = 2;
                let mut out = sstp_data(&ack);
                if !authed {
                    out.extend_from_slice(&sstp_data(&pap_frame(next_id(), "vpn", "vpn")));
                    authed = true;
                }
                writer.write_all(&out).await.map_err(|e| format!("LCP ack failed: {}", e))?;
            } else if code == 2 {
                if !authed {
                    writer
                        .write_all(&sstp_data(&pap_frame(next_id(), "vpn", "vpn")))
                        .await
                        .map_err(|e| format!("PAP send failed: {}", e))?;
                    authed = true;
                }
            } else if code == 9 {
                let mut rep = raw.to_vec();
                rep[2] = 10;
                writer.write_all(&sstp_data(&rep)).await.map_err(|e| format!("LCP reply failed: {}", e))?;
            }
        } else if proto == PPP_PAP && code == 2 {
            // Authenticate-Ack -> ask for an IP (IPCP Configure-Request).
            let req = link_frame(PPP_IPCP, 1, next_id(), &[(3, &[0, 0, 0, 0])]);
            writer.write_all(&sstp_data(&req)).await.map_err(|e| format!("IPCP request failed: {}", e))?;
        } else if proto == PPP_IPCP {
            if code == 1 {
                let mut ack = raw.to_vec();
                ack[2] = 2;
                writer.write_all(&sstp_data(&ack)).await.map_err(|e| format!("IPCP ack failed: {}", e))?;
            } else if code == 3 {
                if let Some(o) = find_opt(payload, 3) {
                    let req = link_frame(PPP_IPCP, 1, next_id(), &[(3, o)]);
                    writer.write_all(&sstp_data(&req)).await.map_err(|e| format!("IPCP retry failed: {}", e))?;
                }
            } else if code == 2 {
                if let Some(o) = find_opt(payload, 3) {
                    if o.len() == 4 {
                        let vip = Ipv4Addr::new(o[0], o[1], o[2], o[3]);
                        let zeros = [0u8; 112];
                        writer
                            .write_all(&sstp_control(0x0004, &[(2, &zeros)]))
                            .await
                            .map_err(|e| format!("CALL_CONNECTED failed: {}", e))?;
                        return Ok(vip);
                    }
                }
            }
        }
    }
    Err("PPP establish: no virtual IP assigned".to_string())
}

// ---------------------------------------------------------------------------
// Userspace TCP (port of JS makeTcp).
// ---------------------------------------------------------------------------

/// Build one SSTP data packet carrying a hand-built IPv4+TCP segment.
fn tcp_frame(
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
) -> Vec<u8> {
    let total = 48 + payload.len();
    let mut f = vec![0u8; total];
    f[0] = 0x10;
    f[2] = (((total >> 8) & 0x0F) | 0x80) as u8;
    f[3] = (total & 0xFF) as u8;
    f[4] = 0xFF;
    f[5] = 0x03;
    f[6] = 0x00;
    f[7] = 0x21;
    // IPv4 header at offset 8.
    let il = 20 + 20 + payload.len();
    f[8] = 0x45;
    f[10] = (il >> 8) as u8;
    f[11] = (il & 0xFF) as u8;
    let ip_id = random_bytes(2);
    f[12] = ip_id[0];
    f[13] = ip_id[1];
    f[14] = 0x40; // DF
    f[16] = 64; // TTL
    f[17] = 6; // TCP
    f[20..24].copy_from_slice(&src_ip.octets());
    f[24..28].copy_from_slice(&dst_ip.octets());
    let c = checksum(&f[8..28]);
    f[18] = (c >> 8) as u8;
    f[19] = (c & 0xFF) as u8;
    // TCP header at offset 28.
    f[28..30].copy_from_slice(&src_port.to_be_bytes());
    f[30..32].copy_from_slice(&dst_port.to_be_bytes());
    f[32..36].copy_from_slice(&seq.to_be_bytes());
    f[36..40].copy_from_slice(&ack.to_be_bytes());
    f[40] = 0x50; // data offset 5
    f[41] = flags;
    f[42..44].copy_from_slice(&65535u16.to_be_bytes()); // window
    f[48..].copy_from_slice(payload);
    // TCP checksum over pseudo-header + TCP segment.
    let mut ps = Vec::with_capacity(12 + 20 + payload.len());
    ps.extend_from_slice(&src_ip.octets());
    ps.extend_from_slice(&dst_ip.octets());
    ps.push(0);
    ps.push(6);
    ps.extend_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
    ps.extend_from_slice(&f[28..]);
    let tc = checksum(&ps);
    f[44] = (tc >> 8) as u8;
    f[45] = (tc & 0xFF) as u8;
    f
}

#[derive(Default)]
struct SessionState {
    seq: u32,
    ack: u32, // next byte we expect from the peer (rcv_next)
    established: bool,
    closed: bool,
    fin_sent: bool,
}

struct Session {
    src_port: u16,
    dst_ip: Ipv4Addr,
    dst_port: u16,
    state: Mutex<SessionState>,
    handshake: Notify,
    payload_tx: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
}

impl Session {
    fn send_ack(&self, tx: &mpsc::UnboundedSender<Vec<u8>>, vip: Ipv4Addr) {
        if let Ok(st) = self.state.try_lock() {
            let frame = tcp_frame(vip, self.dst_ip, self.src_port, self.dst_port, st.seq, st.ack, 0x10, &[]);
            let _ = tx.send(frame);
        }
    }

    fn close(&self) {
        if let Ok(mut st) = self.state.try_lock() {
            st.closed = true;
        }
        if let Ok(mut g) = self.payload_tx.try_lock() {
            *g = None; // dropping the sender closes the receiver -> EOF
        }
        self.handshake.notify_waiters();
    }
}

/// A single TCP connection through the tunnel, usable as an async stream.
pub struct TunnelStream {
    tunnel: Arc<SstpTunnel>,
    session: Arc<Session>,
    payload_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    pending: std::collections::VecDeque<u8>,
}

impl TunnelStream {
    fn send_frame(&self, flags: u8, payload: &[u8]) -> std::io::Result<()> {
        let st = self.session.state.try_lock().map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::WouldBlock, "session state busy")
        })?;
        let frame = tcp_frame(
            self.tunnel.vip,
            self.session.dst_ip,
            self.session.src_port,
            self.session.dst_port,
            st.seq,
            st.ack,
            flags,
            payload,
        );
        drop(st);
        self.tunnel
            .tx
            .send(frame)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "tunnel closed"))?;
        Ok(())
    }
}

impl AsyncRead for TunnelStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if let Some(b) = self.pending.pop_front() {
                buf.put_slice(&[b]);
                // Drain up to the caller's remaining space in this poll.
                while buf.remaining() > 0 {
                    match self.pending.pop_front() {
                        Some(b) => buf.put_slice(&[b]),
                        None => break,
                    }
                }
                return Poll::Ready(Ok(()));
            }
            match self.payload_rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    self.pending.extend(chunk);
                    continue;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())), // EOF
                Poll::Pending => {
                    if self.session.state.try_lock().map(|s| s.closed).unwrap_or(false) {
                        return Poll::Ready(Ok(()));
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

impl AsyncWrite for TunnelStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Send in MSS-sized chunks, advancing our sequence number.
        let mut sent = 0usize;
        for chunk in buf.chunks(TCP_MSS) {
            let seq_advance = {
                let mut st = match self.session.state.try_lock() {
                    Ok(g) => g,
                    Err(_) => {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "session state busy, retry",
                        )))
                    }
                };
                let frame = tcp_frame(
                    self.tunnel.vip,
                    self.session.dst_ip,
                    self.session.src_port,
                    self.session.dst_port,
                    st.seq,
                    st.ack,
                    0x18,
                    chunk,
                );
                st.seq = st.seq.wrapping_add(chunk.len() as u32);
                drop(st);
                if self.tunnel.tx.send(frame).is_err() {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "tunnel closed",
                    )));
                }
                chunk.len()
            };
            sent += seq_advance;
        }
        Poll::Ready(Ok(sent))
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let already = {
            let mut st = match self.session.state.try_lock() {
                Ok(g) => g,
                Err(_) => return Poll::Ready(Ok(())),
            };
            let a = st.fin_sent;
            st.fin_sent = true;
            a
        };
        if !already {
            let _ = self.send_frame(0x11, &[]);
        }
        Poll::Ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// The tunnel: owns the TLS stream halves, the demuxer/writer tasks and the
// session table.
// ---------------------------------------------------------------------------

pub struct SstpTunnel {
    #[allow(dead_code)]
    pub host: String,
    #[allow(dead_code)]
    pub port: u16,
    pub vip: Ipv4Addr,
    pub connector: Connector,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    sessions: Arc<Mutex<HashMap<u16, Arc<Session>>>>,
    dead: AtomicBool,
}

impl SstpTunnel {
    /// Establish a tunnel to `host:port` (direct or via front SOCKS5).
    pub async fn connect(
        connector: &Connector,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<Arc<Self>, String> {
        let tcp = connector
            .connect_tcp(host, port)
            .await
            .map_err(|e| format!("TCP connect to {}:{} failed: {}", host, port, e))?;
        let server_name = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            tokio_rustls::rustls::pki_types::ServerName::IpAddress(
                tokio_rustls::rustls::pki_types::IpAddr::from(ip),
            )
        } else {
            tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|e| format!("bad server name {}: {}", host, e))?
        };
        let tls = TlsConnector::from(accept_any_tls_config())
            .connect(server_name, tcp)
            .await
            .map_err(|e| format!("TLS handshake to {}:{} failed: {}", host, port, e))?;
        let (read_half, mut write_half) = tokio::io::split(tls);
        let mut reader = PacketReader::new(read_half);
        let vip = tokio::time::timeout(timeout, ppp_establish(&mut reader, &mut write_half, host))
            .await
            .map_err(|_| format!("SSTP/PPP establish timed out after {:?}", timeout))?
            .map_err(|e| e)?;
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let sessions: Arc<Mutex<HashMap<u16, Arc<Session>>>> = Arc::new(Mutex::new(HashMap::new()));
        let tunnel = Arc::new(SstpTunnel {
            host: host.to_string(),
            port,
            vip,
            connector: connector.clone(),
            tx,
            sessions: sessions.clone(),
            dead: AtomicBool::new(false),
        });
        // Writer task: outbound SSTP packets -> TLS.
        {
            let mut write_half = write_half;
            let mut rx = rx;
            let t = tunnel.clone();
            tokio::spawn(async move {
                while let Some(pkt) = rx.recv().await {
                    if write_half.write_all(&pkt).await.is_err() {
                        break;
                    }
                }
                // Last tunnel reference dropped: send TLS close_notify so
                // the server closes and the reader task exits too.
                let _ = write_half.shutdown().await;
                t.mark_dead();
            });
        }
        // Reader task: inbound SSTP packets -> demux.
        {
            let t = tunnel.clone();
            tokio::spawn(async move {
                let mut reader = reader;
                loop {
                    match reader.read_packet().await {
                        Ok((ctrl, body)) => {
                            if let Err(e) = t.handle_packet(ctrl, &body).await {
                                eprintln!("tunnel demux error: {}", e);
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                t.mark_dead();
            });
        }
        Ok(tunnel)
    }

    fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// Probe only: establish the tunnel, report the virtual IP, then drop it.
    pub async fn probe(
        connector: &Connector,
        host: &str,
        port: u16,
        timeout: Duration,
    ) -> Result<(Ipv4Addr, Duration), String> {
        let start = std::time::Instant::now();
        let t = Self::connect(connector, host, port, timeout).await?;
        let vip = t.vip;
        Ok((vip, start.elapsed()))
    }

    async fn handle_packet(&self, ctrl: bool, body: &[u8]) -> Result<(), String> {
        if ctrl {
            if body.len() >= 2 && u16be(body, 0) == 0x0008 {
                let _ = self.tx.send(sstp_control(0x0009, &[]));
            }
            return Ok(());
        }
        let Some((proto, code, _id, payload, raw)) = parse_link(body) else {
            return Ok(());
        };
        if proto == PPP_IPV4 {
            self.handle_ip_packet(payload);
            return Ok(());
        }
        // PPP control after establish: keep the link alive.
        if proto == PPP_LCP {
            if code == 1 {
                let mut ack = raw.to_vec();
                ack[2] = 2;
                let _ = self.tx.send(sstp_data(&ack));
            } else if code == 9 {
                let mut rep = raw.to_vec();
                rep[2] = 10;
                let _ = self.tx.send(sstp_data(&rep));
            }
        } else if proto == PPP_PAP && code == 2 {
            let req = link_frame(PPP_IPCP, 1, random_bytes(1)[0], &[(3, &[0, 0, 0, 0])]);
            let _ = self.tx.send(sstp_data(&req));
        } else if proto == PPP_IPCP {
            if code == 1 {
                let mut ack = raw.to_vec();
                ack[2] = 2;
                let _ = self.tx.send(sstp_data(&ack));
            } else if code == 3 {
                if let Some(o) = find_opt(payload, 3) {
                    let req = link_frame(PPP_IPCP, 1, random_bytes(1)[0], &[(3, o)]);
                    let _ = self.tx.send(sstp_data(&req));
                }
            }
        }
        Ok(())
    }

    fn handle_ip_packet(&self, ip: &[u8]) {
        if ip.len() < 40 || (ip[0] >> 4) != 4 || ip[9] != 6 {
            return;
        }
        let ihl = ((ip[0] & 0x0F) as usize) * 4;
        if ip.len() < ihl + 20 {
            return;
        }
        let src_ip = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
        let src_port = u16be(ip, ihl);
        let dst_port = u16be(ip, ihl + 2);
        let seq = u32be(ip, ihl + 4);
        let flags = ip[ihl + 13];
        let doff = ((ip[ihl + 12] >> 4) as usize) * 4;
        let ip_len = (u16be(ip, 2) as usize).min(ip.len());
        let payload: &[u8] = if ip_len > ihl + doff {
            &ip[ihl + doff..ip_len]
        } else {
            &[]
        };
        // Our session is keyed by our source port (= IP dst port here).
        let session = {
            let map = match self.sessions.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            match map.get(&dst_port) {
                Some(s) => s.clone(),
                None => return,
            }
        };
        // Sanity: packet must be from the target we dialled.
        if src_ip != session.dst_ip || src_port != session.dst_port {
            return;
        }
        if (flags & 0x12) == 0x12 {
            // SYN-ACK: complete the handshake.
            let mut st = match session.state.try_lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            if !st.established {
                st.established = true;
                st.ack = seq.wrapping_add(1);
                drop(st);
                session.handshake.notify_waiters();
                session.send_ack(&self.tx, self.vip);
            }
            return;
        }
        let mut st = match session.state.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if !st.established {
            return;
        }
        if flags & 0x04 != 0 {
            // RST
            drop(st);
            session.close();
            return;
        }
        if !payload.is_empty() {
            if seq == st.ack {
                st.ack = st.ack.wrapping_add(payload.len() as u32);
                let chunk = payload.to_vec();
                drop(st);
                if let Ok(g) = session.payload_tx.try_lock() {
                    if let Some(tx) = g.as_ref() {
                        let _ = tx.send(chunk);
                    }
                }
                session.send_ack(&self.tx, self.vip);
            } else if seq < st.ack {
                // Retransmission of already-seen data: just re-ACK.
                drop(st);
                session.send_ack(&self.tx, self.vip);
            } else {
                // Gap: ACK what we have so far; the peer will retransmit.
                drop(st);
                session.send_ack(&self.tx, self.vip);
            }
        } else if flags & 0x01 != 0 {
            // FIN (no payload): ACK it and close our read side.
            st.ack = st.ack.wrapping_add(1);
            drop(st);
            session.send_ack(&self.tx, self.vip);
            session.close();
        }
    }

    /// Open one TCP connection through the tunnel to `dst_host:dst_port`.
    pub async fn open_stream(
        self: &Arc<Self>,
        dst_host: &str,
        dst_port: u16,
        timeout: Duration,
    ) -> Result<TunnelStream, String> {
        if self.is_dead() {
            return Err("tunnel is dead".to_string());
        }
        let dst_ip_str = resolve_ipv4(&self.connector, dst_host).await?;
        let dst_ip: Ipv4Addr = dst_ip_str.parse().map_err(|_| format!("bad resolved IP {}", dst_ip_str))?;
        // Allocate a unique source port.
        let src_port = loop {
            let rb = random_bytes(2);
            let p = 20000 + (u16be(&rb, 0) % 40000);
            let map = self.sessions.lock().await;
            if !map.contains_key(&p) {
                break p;
            }
        };
        let (payload_tx, payload_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let session = Arc::new(Session {
            src_port,
            dst_ip,
            dst_port,
            state: Mutex::new(SessionState {
                seq: {
                    let b = random_bytes(4);
                    u32be(&b, 0)
                },
                ack: 0,
                established: false,
                closed: false,
                fin_sent: false,
            }),
            handshake: Notify::new(),
            payload_tx: Mutex::new(Some(payload_tx)),
        });
        {
            let mut map = self.sessions.lock().await;
            map.insert(src_port, session.clone());
        }
        let stream = TunnelStream {
            tunnel: self.clone(),
            session: session.clone(),
            payload_rx,
            pending: std::collections::VecDeque::new(),
        };
        // SYN with retransmits. All retransmits reuse the same sequence
        // number; only after the handshake completes do we advance past it.
        let syn_seq = session.state.lock().await.seq;
        let per_attempt = std::cmp::max(Duration::from_secs(2), timeout / 4);
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            let syn = tcp_frame(self.vip, dst_ip, src_port, dst_port, syn_seq, 0, 0x02, &[]);
            if self.tx.send(syn).is_err() {
                self.remove_session(src_port);
                return Err("SYN send failed: tunnel closed".to_string());
            }
            // Register the waiter *before* checking the flag, so a
            // SYN-ACK that arrives in between cannot be missed.
            let notified = session.handshake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if session.state.lock().await.established {
                let mut st = session.state.lock().await;
                st.seq = syn_seq.wrapping_add(1);
                return Ok(stream);
            }
            let waited = tokio::time::timeout(per_attempt, notified).await;
            let established = session.state.lock().await.established;
            if established {
                let mut st = session.state.lock().await;
                st.seq = syn_seq.wrapping_add(1);
                return Ok(stream);
            }
            if session.state.lock().await.closed || self.is_dead() {
                self.remove_session(src_port);
                return Err("TCP handshake failed: connection closed/reset".to_string());
            }
            if waited.is_err() && attempts >= 4 {
                self.remove_session(src_port);
                return Err(format!("TCP handshake to {}:{} timed out after {} attempts", dst_host, dst_port, attempts));
            }
            if waited.is_err() {
                continue; // retransmit SYN
            }
        }
    }

    fn remove_session(&self, src_port: u16) {
        if let Ok(mut map) = self.sessions.try_lock() {
            if let Some(s) = map.remove(&src_port) {
                s.close();
            }
        }
    }
}
