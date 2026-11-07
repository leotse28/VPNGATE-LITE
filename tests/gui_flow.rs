//! Headless end-to-end test of the flow the GUI drives: connect a tunnel
//! to the mock SSTP server, run generate_204 through it, and serve a
//! local SOCKS5 port through it. No window, no GPU.
//!
//! The mock server is tests/mock_sstp.py (stdlib Python + a self-signed
//! cert at /tmp/mock_sstp_cert.pem, generated with openssl if missing).
//! If python3 or openssl is unavailable the test prints a skip note and
//! passes, instead of failing on a missing tool.

use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vpngate_client::net::Connector;
use vpngate_client::sstp::SstpTunnel;

struct MockServer(Option<Child>);

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn tool_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ensure_cert() {
    let cert = "/tmp/mock_sstp_cert.pem";
    let key = "/tmp/mock_sstp_key.pem";
    if std::path::Path::new(cert).exists() && std::path::Path::new(key).exists() {
        return;
    }
    let _ = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-keyout",
            key,
            "-out",
            cert,
            "-days",
            "1",
            "-nodes",
            "-subj",
            "/CN=localhost",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn start_mock() -> Option<MockServer> {
    // Reuse a mock that is already listening (another test in this
    // binary may have started it; pkill would break it mid-test).
    if std::net::TcpStream::connect_timeout(
        &"127.0.0.1:8443".parse().unwrap(),
        Duration::from_millis(300),
    )
    .is_ok()
    {
        return Some(MockServer(None));
    }
    // Clear any stale mock from an earlier run (pkill's own cmdline does
    // not contain "mock_sstp", so it cannot match itself here).
    let _ = Command::new("pkill").args(["-f", "mock_sstp.py"]).output();
    std::thread::sleep(Duration::from_millis(300));
    ensure_cert();
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = format!("{}/tests/mock_sstp.py", manifest);
    let child = Command::new("python3")
        .arg(script)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    Some(MockServer(Some(child)))
}

async fn wait_for_port() -> bool {
    for _ in 0..50 {
        if tokio::net::TcpStream::connect("127.0.0.1:8443")
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// The mock SSTP server is shared by the tests in this binary, but
/// its lifetime is tied to one test's `MockServer` handle: if the
/// owner finishes first, its Drop kills the mock under the other
/// test, and a parallel `start_mock` can pkill a mock that is just
/// starting (or already in use). Serialize the mock-using tests
/// instead — each holds this lock for its whole body, so the mock is
/// never started twice at once and never killed mid-test.
static MOCK_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

#[tokio::test]
async fn gui_connect_flow_against_mock() {
    if !tool_exists("python3") || !tool_exists("openssl") {
        eprintln!("SKIP: python3/openssl not available; mock SSTP server cannot run");
        return;
    }
    let _mock_guard = MOCK_LOCK.lock().await;
    let _mock = match start_mock() {
        Some(m) => m,
        None => {
            eprintln!("SKIP: could not start mock SSTP server");
            return;
        }
    };
    assert!(
        wait_for_port().await,
        "mock SSTP server did not listen on 8443"
    );

    let connector = Connector::new(None);
    let timeout = Duration::from_secs(10);
    let tunnel = SstpTunnel::connect(&connector, "127.0.0.1", 8443, timeout)
        .await
        .expect("tunnel connect to mock");
    assert_eq!(tunnel.vip.to_string(), "10.0.0.2", "mock assigns 10.0.0.2");

    // generate_204 through the tunnel (the GUI's "测试隧道" step).
    let mut s = tunnel
        .open_stream("www.gstatic.com", 80, timeout)
        .await
        .expect("open stream");
    s.write_all(b"GET http://www.gstatic.com/generate_204 HTTP/1.1\r\nHost: www.gstatic.com\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(timeout, s.read(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        head.contains("204"),
        "generate_204 should return 204, got: {}",
        head
    );

    // Local mixed proxy server through the tunnel (the GUI's connect
    // path): SOCKS5 still works through the mixed port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socks_addr = listener.local_addr().unwrap();
    let serve = tokio::spawn(vpngate_client::socks5::serve_mixed(
        listener,
        tunnel.clone(),
        timeout,
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = tokio::net::TcpStream::connect(socks_addr).await.unwrap();
    client.write_all(&[0x05, 0x01, 0x00]).await.unwrap(); // no-auth
    let mut resp = [0u8; 2];
    client.read_exact(&mut resp).await.unwrap();
    assert_eq!(resp, [0x05, 0x00]);
    // CONNECT example.com:80 (domain form).
    let mut req = vec![0x05, 0x01, 0x00, 0x03, 11];
    req.extend_from_slice(b"example.com");
    req.extend_from_slice(&80u16.to_be_bytes());
    client.write_all(&req).await.unwrap();
    let mut conn_resp = [0u8; 10];
    client.read_exact(&mut conn_resp).await.unwrap();
    assert_eq!(conn_resp[1], 0x00, "SOCKS5 CONNECT should succeed");
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut buf2 = vec![0u8; 4096];
    let n2 = tokio::time::timeout(timeout, client.read(&mut buf2))
        .await
        .unwrap()
        .unwrap();
    let body = String::from_utf8_lossy(&buf2[..n2]).to_string();
    assert!(
        body.contains("204"),
        "SOCKS5-through-tunnel should return the mock 204, got: {}",
        body
    );

    serve.abort();
    drop(tunnel); // last tunnel ref: writer task shuts TLS down
}

/// Shared mock setup for the mixed-proxy tests: lock the mock for
/// the whole test, start it if needed, and open one tunnel to it.
/// Returns the lock guard first so the caller keeps it alive — the
/// mock is shared, and dropping the guard early would let another
/// test kill the mock mid-test.
async fn setup_mock_tunnel() -> Option<(
    tokio::sync::MutexGuard<'static, ()>,
    MockServer,
    std::sync::Arc<SstpTunnel>,
)> {
    if !tool_exists("python3") || !tool_exists("openssl") {
        eprintln!("SKIP: python3/openssl not available; mock SSTP server cannot run");
        return None;
    }
    let guard = MOCK_LOCK.lock().await;
    let mock = match start_mock() {
        Some(m) => m,
        None => {
            eprintln!("SKIP: could not start mock SSTP server");
            return None;
        }
    };
    if !wait_for_port().await {
        eprintln!("SKIP: mock SSTP server did not listen on 8443");
        return None;
    }
    let connector = Connector::new(None);
    let timeout = Duration::from_secs(10);
    let tunnel = SstpTunnel::connect(&connector, "127.0.0.1", 8443, timeout)
        .await
        .ok()?;
    Some((guard, mock, tunnel))
}

/// Read from `client` until `needle` appears or the timeout elapses;
/// returns everything read so far.
async fn read_until(
    client: &mut tokio::net::TcpStream,
    needle: &str,
    timeout: Duration,
) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, client.read(&mut buf)).await {
            Ok(Ok(0)) => break, // peer closed
            Ok(Ok(n)) => {
                out.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&out).contains(needle) {
                    break;
                }
            }
            _ => break, // timeout or read error
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The mixed port speaks HTTP as well as SOCKS5: an HTTP CONNECT
/// builds a tunnel and passes raw bytes through it, a plain
/// absolute-form GET returns the response through the tunnel, and a
/// POST with a Content-Length body forwards the body too. The mock
/// answers every tunnel stream with a canned 204.
#[tokio::test]
async fn mixed_proxy_http_methods_against_mock() {
    let Some((_guard, _mock, tunnel)) = setup_mock_tunnel().await else {
        return;
    };
    let timeout = Duration::from_secs(10);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve = tokio::spawn(vpngate_client::socks5::serve_mixed(
        listener,
        tunnel.clone(),
        timeout,
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;

    // CONNECT: 200, then raw bytes pass through the tunnel and the
    // mock's 204 comes back.
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
        .await
        .unwrap();
    let head = read_until(&mut c, "\r\n\r\n", timeout).await;
    assert!(
        head.contains("200"),
        "CONNECT should answer 200, got: {}",
        head
    );
    c.write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let body = read_until(&mut c, "204", timeout).await;
    assert!(
        body.contains("204"),
        "bytes through the CONNECT tunnel should return the mock 204, got: {}",
        body
    );
    drop(c);

    // Plain absolute-form GET: the response comes back through the
    // tunnel (and the connection then closes — a fresh connection is
    // needed for the next request).
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(b"GET http://example.com/ HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let body = read_until(&mut c, "204", timeout).await;
    assert!(
        body.contains("204"),
        "plain HTTP GET through the tunnel should return the mock 204, got: {}",
        body
    );
    drop(c);

    // POST with a Content-Length body: the body is forwarded through
    // the tunnel, and the mock's 204 comes back.
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(b"POST http://example.com/post HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
        .await
        .unwrap();
    let body = read_until(&mut c, "204", timeout).await;
    assert!(
        body.contains("204"),
        "POST with a body through the tunnel should return the mock 204, got: {}",
        body
    );
    drop(c);

    serve.abort();
    drop(tunnel);
}

/// LAN sharing binds the same mixed port on all interfaces: one
/// 0.0.0.0 bind serves loopback too, so a client connecting via
/// 127.0.0.1 reaches the mixed server (SOCKS5 greeting answered).
#[tokio::test]
async fn wildcard_bind_serves_loopback_against_mock() {
    let Some((_guard, _mock, tunnel)) = setup_mock_tunnel().await else {
        return;
    };
    let timeout = Duration::from_secs(10);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve = tokio::spawn(vpngate_client::socks5::serve_mixed(
        listener,
        tunnel.clone(),
        timeout,
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    client.write_all(&[0x05, 0x01, 0x00]).await.unwrap(); // no-auth
    let mut resp = [0u8; 2];
    client.read_exact(&mut resp).await.unwrap();
    assert_eq!(
        resp,
        [0x05, 0x00],
        "a wildcard bind must answer loopback SOCKS5 clients"
    );

    serve.abort();
    drop(tunnel);
}

/// A TCP listener that accepts and holds connections forever, never
/// sending a byte — a hanging candidate port (a dead node whose TLS
/// handshake never answers).
fn black_hole_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || loop {
        let Ok((s, _)) = listener.accept() else {
            break;
        };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3600));
            drop(s);
        });
    });
    port
}

/// One candidate port answers (the mock SSTP server), the others hang:
/// the parallel race must return the good one fast — a serial walk
/// would burn the whole timeout on the hanging port listed first.
/// Regression test for Leo's v3.2 log (dead-node probe ~20 s).
#[tokio::test]
async fn parallel_candidate_race_is_fast() {
    if !tool_exists("python3") || !tool_exists("openssl") {
        eprintln!("SKIP: python3/openssl not available; mock SSTP server cannot run");
        return;
    }
    let _mock_guard = MOCK_LOCK.lock().await;
    let _mock = match start_mock() {
        Some(m) => m,
        None => {
            eprintln!("SKIP: could not start mock SSTP server");
            return;
        }
    };
    assert!(
        wait_for_port().await,
        "mock SSTP server did not listen on 8443"
    );

    let connector = Connector::new(None);
    let timeout = Duration::from_secs(5);
    // Hanging ports first: serial order would hang on them before
    // ever reaching the mock.
    let candidates = vec![
        ("127.0.0.1".to_string(), black_hole_port()),
        ("127.0.0.1".to_string(), black_hole_port()),
        ("127.0.0.1".to_string(), 8443),
    ];
    let start = std::time::Instant::now();
    let (vip, ms) = SstpTunnel::probe_any(&connector, &candidates, timeout)
        .await
        .expect("race must find the live mock port");
    let elapsed = start.elapsed();
    assert_eq!(vip.to_string(), "10.0.0.2", "mock assigns 10.0.0.2");
    assert!(
        elapsed < Duration::from_secs(3),
        "parallel race must beat the 5 s timeout, took {:?} (probe said {} ms)",
        elapsed,
        ms.as_millis()
    );
}

/// Every candidate hangs: the race must still return, within about
/// one total timeout — not one timeout per candidate.
#[tokio::test]
async fn parallel_candidate_race_all_hanging_returns_within_timeout() {
    let connector = Connector::new(None);
    let timeout = Duration::from_secs(3);
    let candidates = vec![
        ("127.0.0.1".to_string(), black_hole_port()),
        ("127.0.0.1".to_string(), black_hole_port()),
        ("127.0.0.1".to_string(), black_hole_port()),
    ];
    let start = std::time::Instant::now();
    let res = SstpTunnel::probe_any(&connector, &candidates, timeout).await;
    let elapsed = start.elapsed();
    assert!(res.is_err(), "all-hanging race must fail");
    assert!(
        elapsed < Duration::from_secs(5),
        "three hanging candidates must cost about one 3 s timeout, took {:?}",
        elapsed
    );
    assert!(
        elapsed >= Duration::from_secs(2),
        "the race must actually wait out the deadline, took {:?}",
        elapsed
    );
}
