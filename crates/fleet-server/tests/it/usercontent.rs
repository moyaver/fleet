// SPDX-License-Identifier: AGPL-3.0-only
//! The usercontent origin on a listener of its own (`usercontent.listen`), over real sockets.

use std::net::SocketAddr;

use fleet_server::config::Secret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::support::Harness;

/// One raw HTTP/1.1 exchange: the whole response, head included, lowercased.
async fn raw(addr: SocketAddr, host: &str, path: &str) -> String {
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    sock.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).to_ascii_lowercase()
}

#[tokio::test]
async fn the_usercontent_listener_serves_signed_links_and_nothing_else() {
    let mut h = Harness::start_with(|c| {
        c.api.public_url = Some("https://fleet.example.com".into());
        c.usercontent.url = Some("https://fleet-usercontent.example.net".into());
        c.usercontent.listen = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
        c.usercontent.signing_key = Some(Secret::from("0123456789abcdef0123456789abcdef-key"));
        c.storage.path = std::env::temp_dir().join(format!("fleet-uc-{}", uuid::Uuid::now_v7()));
    })
    .await;
    let server = h.server.as_ref().unwrap();
    let uc = server.usercontent_addr().expect("a usercontent listener");
    assert_ne!(uc, server.addr(), "its own port");

    // Whatever Host the proxy forwards: only `/u/…` links exist here, and a bad one is refused.
    let link = format!("/u/{0}/{0}/9999999999/AAAA/x.png", uuid::Uuid::now_v7());
    let res = raw(uc, "anything", &link).await;
    assert!(res.starts_with("http/1.1 403"), "{res}");
    assert!(res.contains("content-security-policy: sandbox;"), "{res}");
    assert!(res.contains("x-content-type-options: nosniff"), "{res}");
    for path in ["/", "/api/v1/projects", "/readyz"] {
        let res = raw(uc, "anything", path).await;
        assert!(res.starts_with("http/1.1 404"), "{path}: {res}");
    }
    // The main listener serves the app and still does not serve the link.
    let res = raw(server.addr(), "fleet.example.com", &link).await;
    assert!(res.starts_with("http/1.1 404"), "{res}");
    h.stop().await;
}
