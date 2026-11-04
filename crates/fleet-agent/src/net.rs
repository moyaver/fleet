// SPDX-License-Identifier: AGPL-3.0-only
//! Network setup for the transport (research 02 §6.2, research 06 §9.2; issues #24, #25).
//!
//! - **TLS:** Mozilla's roots, the OS store, and `FLEET_CA_FILE` (a PEM bundle, for corporate
//!   TLS inspection). One `rustls` config serves the WebSocket and the token exchange.
//! - **Proxy:** `wss://` goes through `HTTPS_PROXY` (or `https_proxy`, `ALL_PROXY`), `ws://`
//!   through `HTTP_PROXY`, unless `NO_PROXY` matches the host. tokio-tungstenite has no proxy
//!   support, so the WebSocket opens an HTTP `CONNECT` tunnel first. Proxies are `http://`,
//!   optionally with `user:password@` (sent as `Proxy-Authorization: Basic`).
//! - **Credentials:** `FLEET_TOKEN` when set; otherwise, on GitHub Actions with `id-token: write`,
//!   the job's OIDC token (audience `FLEET_OIDC_AUDIENCE`, default `fleet`) is exchanged at
//!   `POST /api/v1/agent/token` for a short-lived agent token.
//!
//! Everything fails open (I2): a bad CA file is logged and skipped, a failed exchange retries
//! with the connection backoff, and none of it can affect Maestro.

use std::{sync::Arc, time::Duration};

use base64::Engine as _;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Where a URL points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) secure: bool,
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Target {
    /// Parses `ws(s)://` and `http(s)://` URLs (host, optional port, any path).
    pub(crate) fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        let secure = match scheme {
            "wss" | "https" => true,
            "ws" | "http" => false,
            _ => return None,
        };
        let authority = rest.split(['/', '?', '#']).next()?;
        let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6.split_once(']')?;
            (
                h.to_owned(),
                after.strip_prefix(':').map(str::parse).transpose().ok()?,
            )
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p.parse().ok()?)),
                None => (authority.to_owned(), None),
            }
        };
        if host.is_empty() {
            return None;
        }
        Some(Self {
            secure,
            port: port.unwrap_or(if secure { 443 } else { 80 }),
            host,
        })
    }
}

/// An HTTP proxy.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Proxy {
    pub(crate) host: String,
    pub(crate) port: u16,
    /// `user:password`, as given.
    pub(crate) auth: Option<String>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Proxy credentials never print (I6).
        write!(
            f,
            "Proxy({}:{}, auth: {})",
            self.host,
            self.port,
            self.auth.is_some()
        )
    }
}

/// The proxy to use for `target`, from an environment lookup.
pub(crate) fn proxy_for(target: &Target, env: impl Fn(&str) -> Option<String>) -> Option<Proxy> {
    let get = |names: &[&str]| {
        names
            .iter()
            .find_map(|n| env(n).filter(|v| !v.trim().is_empty()))
    };
    let no_proxy = get(&["NO_PROXY", "no_proxy"]).unwrap_or_default();
    if bypass(&no_proxy, &target.host) {
        return None;
    }
    let url = if target.secure {
        get(&["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"])
    } else {
        get(&["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"])
    }?;
    let url = if url.contains("://") {
        url
    } else {
        format!("http://{url}")
    };
    if !url.starts_with("http://") {
        tracing::warn!("only http:// proxies are supported; connecting directly");
        return None;
    }
    let rest = url.trim_start_matches("http://");
    let authority = rest.split('/').next().unwrap_or_default();
    let auth = authority.rsplit_once('@').map(|(a, _)| percent_decode(a));
    let t = Target::parse(&format!(
        "http://{}",
        authority.rsplit_once('@').map_or(authority, |(_, h)| h)
    ))?;
    Some(Proxy {
        host: t.host,
        port: if authority.contains(':') {
            t.port
        } else {
            3128
        },
        auth,
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        if let (b'%', Some(v)) = (b, hex) {
            out.push(v);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `NO_PROXY`: comma-separated hosts or domain suffixes (`.corp`, `corp` matches `a.corp`), `*`.
fn bypass(no_proxy: &str, host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    no_proxy
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .any(|e| {
            let e = e.to_ascii_lowercase();
            let e = e
                .split(':')
                .next()
                .unwrap_or_default()
                .trim_start_matches("*.");
            e == "*"
                || host == e.trim_start_matches('.')
                || host.ends_with(&format!(".{}", e.trim_start_matches('.')))
        })
}

/// Opens a TCP connection to `target`, through `proxy` when given (`CONNECT`).
pub(crate) async fn tcp(target: &Target, proxy: Option<&Proxy>) -> Result<TcpStream, Error> {
    let Some(p) = proxy else {
        return Ok(TcpStream::connect((target.host.as_str(), target.port)).await?);
    };
    let mut s = TcpStream::connect((p.host.as_str(), p.port)).await?;
    let host = if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let auth = p.auth.as_ref().map_or(String::new(), |a| {
        format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64::engine::general_purpose::STANDARD.encode(a)
        )
    });
    s.write_all(format!("CONNECT {host} HTTP/1.1\r\nHost: {host}\r\n{auth}\r\n").as_bytes())
        .await?;
    // Read the proxy's answer up to the blank line, byte by byte: nothing after it is ours.
    let mut head = Vec::with_capacity(256);
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err("the proxy's CONNECT answer is too long".into());
        }
        head.push(s.read_u8().await?);
    }
    let status = String::from_utf8_lossy(&head);
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    if code != "200" {
        let line = status.lines().next().unwrap_or_default().to_owned();
        return Err(format!("the proxy refused CONNECT {host}: {line}").into());
    }
    Ok(s)
}

/// The TLS client config: Mozilla's roots, the OS store and `FLEET_CA_FILE`.
pub(crate) fn tls(ca_file: Option<&str>) -> Arc<rustls::ClientConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let native = rustls_native_certs::load_native_certs();
    let (added, _) = roots.add_parsable_certificates(native.certs);
    tracing::debug!(added, "OS trust store loaded");
    if let Some(path) = ca_file.filter(|p| !p.trim().is_empty()) {
        match CertificateDer::pem_file_iter(path) {
            Ok(certs) => {
                let certs: Vec<CertificateDer<'static>> = certs.filter_map(Result::ok).collect();
                let (ok, bad) = roots.add_parsable_certificates(certs);
                tracing::info!(path, ok, bad, "FLEET_CA_FILE loaded");
            }
            Err(e) => tracing::warn!(path, error = %e, "FLEET_CA_FILE unreadable; ignoring it"),
        }
    }
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

/// GitHub's OIDC token endpoint, as the runner sets it up (`permissions: id-token: write`).
#[derive(Clone)]
pub(crate) struct OidcRequest {
    pub(crate) url: String,
    pub(crate) bearer: String,
    pub(crate) audience: String,
}

impl std::fmt::Debug for OidcRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OidcRequest(<redacted>)")
    }
}

impl OidcRequest {
    /// From the environment; `None` outside GitHub Actions or without `id-token: write`.
    pub(crate) fn from_env(env: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let url = env("ACTIONS_ID_TOKEN_REQUEST_URL").filter(|v| !v.is_empty())?;
        let bearer = env("ACTIONS_ID_TOKEN_REQUEST_TOKEN").filter(|v| !v.is_empty())?;
        let audience = env("FLEET_OIDC_AUDIENCE")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| fleet_protocol::brand::OIDC_AUDIENCE.to_owned());
        Some(Self {
            url,
            bearer,
            audience,
        })
    }
}

/// The server's `http(s)://` base URL, from the agent WebSocket URL.
pub(crate) fn http_base(ws_url: &str) -> String {
    let http = if let Some(rest) = ws_url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = ws_url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        ws_url.to_owned()
    };
    match http.find("/api/") {
        Some(i) => http.get(..i).unwrap_or_default().to_owned(),
        None => http.trim_end_matches('/').to_owned(),
    }
}

/// The token-exchange URL next to the agent WebSocket URL.
pub(crate) fn token_url(ws_url: &str) -> String {
    format!("{}/api/v1/agent/token", http_base(ws_url))
}

/// Gets the job's OIDC token and exchanges it for an agent token. Uses the same TLS roots; the
/// HTTP client honours `HTTPS_PROXY`/`NO_PROXY` itself.
pub(crate) async fn exchange(
    req: &OidcRequest,
    server_ws_url: &str,
    tls: Arc<rustls::ClientConfig>,
) -> Result<String, Error> {
    #[derive(serde::Deserialize)]
    struct IdToken {
        value: String,
    }
    #[derive(serde::Deserialize)]
    struct AgentToken {
        token: String,
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .tls_backend_preconfigured((*tls).clone())
        .build()?;
    let sep = if req.url.contains('?') { '&' } else { '?' };
    let id: IdToken = client
        .get(format!("{}{sep}audience={}", req.url, req.audience))
        .bearer_auth(&req.bearer)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let res = client
        .post(token_url(server_ws_url))
        .json(&serde_json::json!({ "id_token": id.value }))
        .send()
        .await?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(format!("the server refused the OIDC token ({status}): {body}").into());
    }
    Ok(res.json::<AgentToken>().await?.token)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn urls_parse_into_targets() {
        let t = Target::parse("wss://fleet.corp:8443/api/v1/agent/ws").unwrap();
        assert_eq!(
            (t.secure, t.host.as_str(), t.port),
            (true, "fleet.corp", 8443)
        );
        assert_eq!(Target::parse("ws://h/x").unwrap().port, 80);
        assert_eq!(Target::parse("https://[::1]/").unwrap().host, "::1");
        assert!(Target::parse("ftp://x").is_none());
        assert_eq!(
            token_url("wss://f.corp/api/v1/agent/ws"),
            "https://f.corp/api/v1/agent/token"
        );
        assert_eq!(
            token_url("ws://127.0.0.1:9/api/v1/agent/ws"),
            "http://127.0.0.1:9/api/v1/agent/token"
        );
    }

    #[test]
    fn proxies_follow_the_scheme_and_no_proxy() {
        let secure = Target::parse("wss://fleet.corp/x").unwrap();
        let plain = Target::parse("ws://fleet.corp/x").unwrap();
        let e = env(&[
            ("HTTPS_PROXY", "http://u%40x:p%3Ass@proxy.corp:8080"),
            ("HTTP_PROXY", "proxy2:3128"),
        ]);
        let p = proxy_for(&secure, &e).unwrap();
        assert_eq!(
            (p.host.as_str(), p.port, p.auth.as_deref()),
            ("proxy.corp", 8080, Some("u@x:p:ss"))
        );
        assert!(
            !format!("{p:?}").contains("p:ss"),
            "credentials never print"
        );
        assert_eq!(proxy_for(&plain, &e).unwrap().host, "proxy2");
        for np in ["fleet.corp", ".corp", "corp", "*", "other, fleet.corp:443"] {
            let e = env(&[("HTTPS_PROXY", "http://p:1"), ("NO_PROXY", np)]);
            assert!(proxy_for(&secure, &e).is_none(), "{np}");
        }
        let e = env(&[("HTTPS_PROXY", "http://p:1"), ("NO_PROXY", "notcorp")]);
        assert!(
            proxy_for(&secure, &e).is_some(),
            "suffixes match on a dot boundary"
        );
        assert!(proxy_for(&secure, env(&[])).is_none());
    }

    #[test]
    fn oidc_needs_both_runner_variables() {
        assert!(
            OidcRequest::from_env(env(&[("ACTIONS_ID_TOKEN_REQUEST_URL", "https://x")])).is_none()
        );
        let r = OidcRequest::from_env(env(&[
            ("ACTIONS_ID_TOKEN_REQUEST_URL", "https://x?api-version=2.0"),
            ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "t"),
        ]))
        .unwrap();
        assert_eq!(r.audience, "fleet");
        assert_eq!(format!("{r:?}"), "OidcRequest(<redacted>)");
    }

    #[tokio::test]
    async fn connect_tunnels_through_a_proxy() {
        // A target that echoes, and a proxy that checks CONNECT then splices.
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taddr = target.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = target.accept().await.unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf).await.unwrap();
        });
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let paddr = proxy.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(String::new()));
        let seen2 = Arc::clone(&seen);
        tokio::spawn(async move {
            let (mut c, _) = proxy.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(c.read_u8().await.unwrap());
            }
            *seen2.lock().unwrap() = String::from_utf8(head).unwrap();
            let mut up = TcpStream::connect(taddr).await.unwrap();
            c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            tokio::io::copy_bidirectional(&mut c, &mut up).await.ok();
        });
        let t = Target {
            secure: false,
            host: "127.0.0.1".into(),
            port: taddr.port(),
        };
        let p = Proxy {
            host: "127.0.0.1".into(),
            port: paddr.port(),
            auth: Some("u:p".into()),
        };
        let mut s = tcp(&t, Some(&p)).await.unwrap();
        s.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
        let head = seen.lock().unwrap().clone();
        assert!(
            head.starts_with(&format!("CONNECT 127.0.0.1:{} HTTP/1.1", taddr.port())),
            "{head}"
        );
        assert!(head.contains("Proxy-Authorization: Basic dTpw"), "{head}");
    }

    #[tokio::test]
    async fn a_refusing_proxy_is_an_error() {
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let paddr = proxy.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut c, _) = proxy.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(c.read_u8().await.unwrap());
            }
            c.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
                .await
                .unwrap();
        });
        let t = Target {
            secure: true,
            host: "fleet.corp".into(),
            port: 443,
        };
        let p = Proxy {
            host: "127.0.0.1".into(),
            port: paddr.port(),
            auth: None,
        };
        let err = tcp(&t, Some(&p)).await.unwrap_err().to_string();
        assert!(err.contains("407"), "{err}");
    }

    #[test]
    fn a_bad_ca_file_is_ignored() {
        let cfg = tls(Some("/nonexistent/ca.pem"));
        assert!(Arc::strong_count(&cfg) >= 1);
    }
}
