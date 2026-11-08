// SPDX-License-Identifier: AGPL-3.0-only
//! The browser harness (PLAN §13): chromedriver plus headless Chrome driven by fantoccini
//! against the real server on a fresh Postgres 18 database. Later slices write their scenarios
//! on top of [`Browser`]; the smoke test below proves the chain works.
//!
//! The dashboard comes from `crates/fleet-web/dist` (`cargo xtask web`), which `cargo xtask ci`
//! builds before the tests. chromedriver is found by [`driver_path`].

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use fantoccini::{
    Client, ClientBuilder, Locator,
    wd::{Capabilities, WebDriverCompatibleCommand},
};
use hyper_util::client::legacy::connect::HttpConnector;
use serde_json::{Value, json};
use tokio::process::{Child, Command};

use crate::support::Stack;

/// Injected before any page script, so a policy violation during page load is recorded too.
const CSP_RECORDER: &str = "window.__cspViolations = [];\
    document.addEventListener('securitypolicyviolation', e => \
    window.__cspViolations.push(e.violatedDirective + ' ' + e.blockedURI));";

/// Records every WebSocket the page constructs, so a test can count the sockets it holds open.
const SOCKET_RECORDER: &str = "window.__sockets = []; const NativeWebSocket = window.WebSocket;\
    window.WebSocket = class extends NativeWebSocket {\
    constructor(...args) { super(...args); window.__sockets.push(this); } };";

/// chromedriver: `$CHROMEDRIVER`, then `target/tools/chromedriver` (`cargo xtask setup`), then
/// `$CHROMEWEBDRIVER` (GitHub's runners), then `PATH`.
fn driver_path() -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut candidates = Vec::new();
    candidates.extend(std::env::var_os("CHROMEDRIVER").map(PathBuf::from));
    candidates.push(repo.join("target/tools/chromedriver"));
    candidates
        .extend(std::env::var_os("CHROMEWEBDRIVER").map(|d| Path::new(&d).join("chromedriver")));
    candidates.extend(
        std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|d| d.join("chromedriver")),
    );
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .expect("chromedriver should be installed: run `cargo xtask setup` or set CHROMEDRIVER")
}

/// chromedriver's vendor command that runs a Chrome `DevTools` Protocol method.
#[derive(Debug)]
struct Cdp {
    method: &'static str,
    params: Value,
}

impl WebDriverCompatibleCommand for Cdp {
    fn endpoint(
        &self,
        base_url: &url::Url,
        session_id: Option<&str>,
    ) -> Result<url::Url, url::ParseError> {
        base_url.join(&format!(
            "session/{}/goog/cdp/execute",
            session_id.unwrap_or_default()
        ))
    }

    fn method_and_body(&self, _: &url::Url) -> (http::Method, Option<String>) {
        let body = json!({ "cmd": self.method, "params": self.params });
        (http::Method::POST, Some(body.to_string()))
    }
}

/// A headless Chrome session; chromedriver stops with it.
pub(crate) struct Browser {
    pub(crate) client: Client,
    _chromedriver: Child,
}

impl Browser {
    pub(crate) async fn launch() -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(driver_path())
            .arg(format!("--port={port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // CI containers have no user namespaces for Chrome's sandbox.
        let mut args = vec!["--headless=new", "--no-sandbox", "--window-size=1280,900"];
        let mut options = json!({});
        if let Some(bin) = std::env::var_os("CHROME") {
            options["binary"] = bin.to_string_lossy().into_owned().into();
        }
        options["args"] = std::mem::take(&mut args).into();
        let mut caps = Capabilities::new();
        caps.insert("browserName".into(), "chrome".into());
        caps.insert("goog:chromeOptions".into(), options);
        let url = format!("http://127.0.0.1:{port}");
        let mut last = None;
        // chromedriver needs a moment before it accepts sessions.
        for _ in 0..50 {
            match ClientBuilder::new(HttpConnector::new())
                .capabilities(caps.clone())
                .connect(&url)
                .await
            {
                Ok(client) => {
                    for source in [CSP_RECORDER, SOCKET_RECORDER] {
                        client
                            .issue_cmd(Cdp {
                                method: "Page.addScriptToEvaluateOnNewDocument",
                                params: json!({ "source": source }),
                            })
                            .await
                            .unwrap();
                    }
                    return Self {
                        client,
                        _chromedriver: child,
                    };
                }
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("chromedriver should accept a session: {last:?}");
    }

    /// The `WebSockets` the page has constructed, and how many of them are open (connecting or open).
    pub(crate) async fn sockets(&self) -> (usize, usize) {
        let value = self
            .client
            .execute(
                "const s = window.__sockets; return [s.length, s.filter(w => w.readyState <= 1).length];",
                Vec::new(),
            )
            .await
            .unwrap();
        let counts: Vec<usize> = serde_json::from_value(value).unwrap();
        (counts[0], counts[1])
    }

    /// Content-security-policy violations the page has reported so far.
    pub(crate) async fn csp_violations(&self) -> Vec<String> {
        let value = self
            .client
            .execute("return window.__cspViolations;", Vec::new())
            .await
            .unwrap();
        serde_json::from_value(value).unwrap()
    }

    /// Sets a host-only, `Secure`, `HttpOnly` cookie for `url` through the `DevTools` protocol.
    /// (WebDriver's own cookie command refuses cookies without a registrable domain, and the
    /// `__Host-` prefix forbids one.)
    pub(crate) async fn set_secure_cookie(&self, url: &str, name: &str, value: &str) {
        self.client
            .issue_cmd(Cdp {
                method: "Network.setCookie",
                params: json!({
                    "name": name, "value": value, "url": url,
                    "path": "/", "secure": true, "httpOnly": true, "sameSite": "Lax"
                }),
            })
            .await
            .unwrap();
    }

    /// Lays the page out at `width` CSS px (and 900 px high), as a window or a phone of that width
    /// would. Desktop emulation: the width is the layout viewport and nothing else changes.
    pub(crate) async fn set_viewport_width(&self, width: u32) {
        self.client
            .issue_cmd(Cdp {
                method: "Emulation.setDeviceMetricsOverride",
                params: json!({
                    "width": width, "height": 900, "deviceScaleFactor": 1, "mobile": false
                }),
            })
            .await
            .unwrap();
    }

    /// Ends the session (chromedriver follows on drop).
    pub(crate) async fn quit(self) {
        self.client.close().await.unwrap();
    }
}

#[tokio::test]
async fn the_dashboard_loads_in_a_real_browser_without_csp_violations() {
    let stack = Stack::start(|_| {}).await;
    let browser = Browser::launch().await;
    browser
        .client
        .goto(&format!("http://{}/", stack.addr()))
        .await
        .unwrap();
    // The heading only exists once the wasm bundle ran under the page's CSP.
    let h1 = browser
        .client
        .wait()
        .at_most(Duration::from_secs(20))
        .for_element(Locator::Css("h1"))
        .await
        .unwrap();
    assert_eq!(h1.text().await.unwrap(), "Fleet");
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}
