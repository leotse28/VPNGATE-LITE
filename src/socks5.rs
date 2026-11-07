//! SOCKS5, both directions:
//! * **Client** (`socks5_connect`): used when a front SOCKS5 proxy is
//!   configured — the TCP connection to the VPNGate node (and to
//!   vpngate.net / DoH) goes through it.
//! * **Server** (`serve_socks5`): after the tunnel is up, a local
//!   SOCKS5 listener (default 127.0.0.1:10808) forwards each accepted
//!   TCP connection through the tunnel as its own userspace TCP session.

use crate::sstp::SstpTunnel;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "SOCKS5 proxy rejected username/password auth"));
        }
        let mut auth = vec![0x01, u.len() as u8];
        auth.extend_from_slice(u.as_bytes());
        auth.push(p.len() as u8);
        auth.extend_from_slice(p.as_bytes());
        s.write_all(&auth).await?;
        let mut aresp = [0u8; 2];
        s.read_exact(&mut aresp).await?;
        if aresp != [0x01, 0x00] {
            return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "SOCKS5 proxy authentication failed"));
        }
    } else {
        s.write_all(&[0x05, 0x01, 0x00]).await?;
        let mut resp = [0u8; 2];
        s.read_exact(&mut resp).await?;
        if resp != [0x05, 0x00] {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "SOCKS5 proxy rejected no-auth"));
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
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad SOCKS5 reply address type"));
        }
    };
    let mut rest = vec![0u8; skip + 2];
    s.read_exact(&mut rest).await?;
    Ok(s)
}

/// Serve a local SOCKS5 proxy on `listener`, forwarding connections
/// through `tunnel`. Runs until the listener errors or the task is
/// aborted. Each connection gets its own userspace TCP session.
pub async fn serve_socks5(listener: TcpListener, tunnel: Arc<SstpTunnel>, timeout: Duration) {
    println!("SOCKS5 listening on {}", listener.local_addr().unwrap());
    println!("Configure your apps / browser to use this SOCKS5 proxy. Ctrl-C to stop.");
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("SOCKS5 accept error: {}", e);
                continue;
            }
        };
        let t = tunnel.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_socks5_conn(socket, t, timeout).await {
                eprintln!("SOCKS5 conn from {} failed: {}", peer, e);
            }
        });
    }
}

async fn handle_socks5_conn(
    mut local: TcpStream,
    tunnel: Arc<SstpTunnel>,
    timeout: Duration,
) -> Result<(), String> {
    // Greeting.
    let mut hdr = [0u8; 2];
    local.read_exact(&mut hdr).await.map_err(|e| e.to_string())?;
    if hdr[0] != 0x05 {
        return Err("not a SOCKS5 client".to_string());
    }
    let n = hdr[1] as usize;
    let mut methods = vec![0u8; n];
    local.read_exact(&mut methods).await.map_err(|e| e.to_string())?;
    if !methods.contains(&0x00) {
        local.write_all(&[0x05, 0xFF]).await.map_err(|e| e.to_string())?;
        return Err("SOCKS5 client offered no no-auth method".to_string());
    }
    local.write_all(&[0x05, 0x00]).await.map_err(|e| e.to_string())?;
    // Request.
    let mut rhdr = [0u8; 4];
    local.read_exact(&mut rhdr).await.map_err(|e| e.to_string())?;
    if rhdr[0] != 0x05 || rhdr[1] != 0x01 {
        let _ = local.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
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
            let _ = local.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
            return Err("IPv6 targets are not supported by the tunnel (IPv4 only)".to_string());
        }
        t => {
            let _ = local.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
            return Err(format!("unsupported SOCKS5 address type {}", t));
        }
    };
    if tunnel.is_dead() {
        let _ = local.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
        return Err("tunnel is dead".to_string());
    }
    let mut remote = match tunnel.open_stream(&host, port, timeout).await {
        Ok(s) => s,
        Err(e) => {
            let _ = local.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
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
