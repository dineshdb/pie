//! End-to-end behaviour of the proxy: does an allowed `CONNECT` really carry
//! bytes, and is a denied one really refused?
//!
//! Everything runs against a local echo server, so the tests need no network
//! and no external host.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use pie_proxy::Policy;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Accepts one connection and echoes back whatever it receives.
async fn spawn_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let addr = listener.local_addr().expect("echo addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

/// Accepts connections and immediately announces `tag`, so a test can tell
/// which server it actually reached.
async fn spawn_tagged_server(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind tagged");
    let addr = listener.local_addr().expect("tagged addr");
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = stream.write_all(tag.as_bytes()).await;
            });
        }
    });
    addr
}

/// Starts the proxy on an ephemeral port and returns where it listens.
async fn spawn_proxy(policy: Policy) -> SocketAddr {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let _ = pie_proxy::proxy::serve_with_ready(listen, policy, move |bound| {
            let _ = tx.send(bound);
        })
        .await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), rx)
        .await
        .expect("proxy did not start in time")
        .expect("proxy dropped before reporting its address")
}

/// Sends a CONNECT and returns the status line plus the open stream.
async fn connect_through(proxy: SocketAddr, authority: &str) -> (String, TcpStream) {
    let mut stream = TcpStream::connect(proxy).await.expect("connect to proxy");
    let request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send CONNECT");

    // Read just the status line; the tunnel payload must not be consumed.
    let mut status = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read_exact(&mut byte).await.is_ok() {
        status.push(byte[0]);
        if status.ends_with(b"\r\n") {
            break;
        }
    }
    (String::from_utf8_lossy(&status).trim().to_string(), stream)
}

/// Drains the remaining response headers after the status line.
async fn skip_headers(stream: &mut TcpStream) {
    let mut window = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read_exact(&mut byte).await.is_ok() {
        window.push(byte[0]);
        if window.ends_with(b"\r\n\r\n") || window.ends_with(b"\n\n") {
            return;
        }
    }
}

#[tokio::test]
async fn allowed_connect_tunnels_bytes_end_to_end() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(["127.0.0.1"], [""; 0])
        .expect("policy")
        .with_ports([echo.port()]);
    let proxy = spawn_proxy(policy).await;

    let authority = format!("127.0.0.1:{}", echo.port());
    let (status, mut stream) = connect_through(proxy, &authority).await;
    assert!(status.contains("200"), "expected 200, got {status:?}");
    skip_headers(&mut stream).await;

    stream.write_all(b"ping").await.expect("write into tunnel");
    let mut buf = [0u8; 4];
    stream
        .read_exact(&mut buf)
        .await
        .expect("read back through tunnel");
    assert_eq!(&buf, b"ping", "tunnel must relay bytes untouched");
}

#[tokio::test]
async fn connect_to_unlisted_host_is_refused() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(["127.0.0.1"], [""; 0])
        .expect("policy")
        .with_ports([echo.port()]);
    let proxy = spawn_proxy(policy).await;

    // A different loopback address, so nothing but the policy can refuse it.
    let (status, _) = connect_through(proxy, &format!("127.0.0.2:{}", echo.port())).await;
    assert!(status.contains("403"), "expected 403, got {status:?}");
}

/// Uses a *second live* server, so a 403 cannot be confused with a connection
/// failure to a closed port.
#[tokio::test]
async fn connect_to_unlisted_port_is_refused() {
    let allowed = spawn_echo_server().await;
    let forbidden = spawn_echo_server().await;
    let policy = Policy::new(["127.0.0.1"], [""; 0])
        .expect("policy")
        .with_ports([allowed.port()]);
    let proxy = spawn_proxy(policy).await;

    let (status, _) = connect_through(proxy, &format!("127.0.0.1:{}", forbidden.port())).await;
    assert!(status.contains("403"), "expected 403, got {status:?}");
}

/// The property that matters most: whatever the policy checked has to be what
/// the proxy actually dials. Two live servers, only one allowed; every spelling
/// of the forbidden one must fail to reach it.
#[tokio::test]
async fn the_checked_target_is_the_connected_target() {
    let allowed = spawn_echo_server().await;
    let forbidden = spawn_tagged_server("FORBIDDEN").await;

    // `*` allows any host, but the forbidden address is denied outright, so
    // only a check-versus-connect mismatch could ever reach it.
    let policy = Policy::new(["*"], ["127.0.0.1"])
        .expect("policy")
        .with_ports([allowed.port(), forbidden.port()]);
    let proxy = spawn_proxy(policy).await;

    for authority in [
        format!("127.0.0.1:{}", forbidden.port()),
        // userinfo: renders into the checked string, dropped when dialling
        format!("x@127.0.0.1:{}", forbidden.port()),
        format!("@127.0.0.1:{}", forbidden.port()),
        // percent-encoded host
        format!("%31%32%37.0.0.1:{}", forbidden.port()),
        format!("127%2E0%2E0%2E1:{}", forbidden.port()),
        // IPv4-mapped IPv6 forms of the same address
        format!("[::ffff:127.0.0.1]:{}", forbidden.port()),
        format!("[::ffff:7f00:1]:{}", forbidden.port()),
        // alternate IPv4 encodings
        format!("127.1:{}", forbidden.port()),
        format!("2130706433:{}", forbidden.port()),
        format!("0x7f.0.0.1:{}", forbidden.port()),
    ] {
        let (status, mut stream) = connect_through(proxy, &authority).await;
        assert!(
            !status.contains("200"),
            "{authority} was tunnelled (status {status:?}) — policy bypass"
        );
        // Belt and braces: nothing from the forbidden server may come back.
        let mut buf = [0u8; 64];
        let read =
            tokio::time::timeout(std::time::Duration::from_millis(300), stream.read(&mut buf))
                .await;
        if let Ok(Ok(n)) = read
            && n > 0
        {
            let body = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(
                !body.contains("FORBIDDEN"),
                "{authority} reached the forbidden server: {body:?}"
            );
        }
    }
}

/// A portless CONNECT must be refused: rama's connector defaults such a target
/// to port 80, while the policy would have judged 443.
#[tokio::test]
async fn portless_connect_is_refused() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(["127.0.0.1"], [""; 0])
        .expect("policy")
        .with_ports([echo.port()]);
    let proxy = spawn_proxy(policy).await;

    let (status, _) = connect_through(proxy, "127.0.0.1").await;
    assert!(
        status.contains("400") || status.contains("403"),
        "expected refusal, got {status:?}"
    );
}

/// A denied request must not open an upstream socket at all.
#[tokio::test]
async fn a_denied_connect_opens_no_upstream_socket() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = upstream.local_addr().expect("addr").port();
    let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        while let Ok((_stream, _)) = upstream.accept().await {
            let _ = accepted_tx.send(()).await;
        }
    });

    let policy = Policy::new(["*"], ["127.0.0.1"])
        .expect("policy")
        .with_ports([port]);
    let proxy = spawn_proxy(policy).await;

    let (status, _) = connect_through(proxy, &format!("127.0.0.1:{port}")).await;
    assert!(status.contains("403"), "expected 403, got {status:?}");

    let accepted = tokio::time::timeout(std::time::Duration::from_millis(400), accepted_rx.recv())
        .await
        .ok()
        .flatten();
    assert!(
        accepted.is_none(),
        "a denied CONNECT must not reach the upstream server"
    );
}

#[tokio::test]
async fn explicit_deny_beats_allow_over_the_wire() {
    let echo = spawn_echo_server().await;
    let policy = Policy::new(["*"], ["127.0.0.1"])
        .expect("policy")
        .with_ports([echo.port()]);
    let proxy = spawn_proxy(policy).await;

    let (status, _) = connect_through(proxy, &format!("127.0.0.1:{}", echo.port())).await;
    assert!(status.contains("403"), "expected 403, got {status:?}");
}
