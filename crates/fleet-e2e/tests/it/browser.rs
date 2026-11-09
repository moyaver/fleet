// SPDX-License-Identifier: AGPL-3.0-only
//! The browser harness (PLAN §13): chromedriver plus headless Chrome driven by fantoccini
//! against the real server on a fresh Postgres 18 database. Later slices write their scenarios
//! on top of [`Browser`]; the smoke test below proves the chain works.
//!
//! The dashboard comes from `crates/fleet-web/dist` (`cargo xtask web`), which `cargo xtask ci`
//! builds before the tests. chromedriver is found by [`driver_path`].
//!
//! Nothing the harness starts outlives its test. chromedriver leads its own process group and
//! Chrome stays in it, so [`DriverProcess`] ends the whole browser tree when the [`Browser`] drops,
//! on a panic too. Each Chrome gets a profile under [`chrome_root`] named after the test process,
//! so a test binary killed outright (no drop) is reaped by the next test process that starts
//! ([`reap_stale`]). That reaper only touches processes whose command line names [`chrome_root`].

use std::{
    io::{Read as _, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Once,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

use fantoccini::{
    Client, ClientBuilder, Locator,
    wd::{Capabilities, WebDriverCompatibleCommand},
};
use hyper_util::client::legacy::connect::HttpConnector;
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill, killpg},
    unistd::{Pid, getpgrp},
};
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

/// Where every browser profile lives: `<temp dir>/fleet-e2e-chrome/<test pid>-<n>`.
fn chrome_root() -> PathBuf {
    std::env::temp_dir().join("fleet-e2e-chrome")
}

/// Profiles are numbered within a test process, so `<test pid>-<n>` is unique.
static NEXT_PROFILE: AtomicU32 = AtomicU32::new(1);

/// How long a process group gets to exit after `SIGTERM` before it is `SIGKILL`ed.
const GRACE: Duration = Duration::from_secs(1);

/// chromedriver and the Chrome it started. Dropping it ends the WebDriver session (a blocking
/// request, since Drop cannot await), then the whole process group, then removes the profile.
struct DriverProcess {
    child: Child,
    /// chromedriver's pid, which is also the id of the process group Chrome joins.
    group: i32,
    port: u16,
    session: Option<String>,
    profile: PathBuf,
}

impl Drop for DriverProcess {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            delete_session(self.port, session);
        }
        // The session close above is best effort; this is what guarantees nothing is left.
        end_group(self.group, Some(&mut self.child));
        // Chrome has exited or been killed, so the profile is this test's alone.
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

/// A headless Chrome session. Dropping it, or [`Browser::quit`], ends chromedriver and Chrome.
pub(crate) struct Browser {
    pub(crate) client: Client,
    driver: DriverProcess,
}

impl Browser {
    pub(crate) async fn launch() -> Self {
        tokio::task::spawn_blocking(|| REAP.call_once(reap_stale))
            .await
            .unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let profile = chrome_root().join(format!(
            "{}-{}",
            std::process::id(),
            NEXT_PROFILE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&profile).unwrap();
        let child = Command::new(driver_path())
            .arg(format!("--port={port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            // chromedriver leads its own group, and Chrome joins it.
            .process_group(0)
            .spawn()
            .unwrap();
        let group = i32::try_from(child.id().expect("chromedriver should have a pid")).unwrap();
        let mut driver = DriverProcess {
            child,
            group,
            port,
            session: None,
            profile: profile.clone(),
        };
        // CI containers have no user namespaces for Chrome's sandbox.
        let args = vec![
            "--headless=new".to_owned(),
            "--no-sandbox".to_owned(),
            "--window-size=1280,900".to_owned(),
            format!("--user-data-dir={}", profile.display()),
        ];
        let mut options = json!({});
        if let Some(bin) = std::env::var_os("CHROME") {
            options["binary"] = bin.to_string_lossy().into_owned().into();
        }
        options["args"] = args.into();
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
                    driver.session = client.session_id().await.ok().flatten();
                    for source in [CSP_RECORDER, SOCKET_RECORDER] {
                        client
                            .issue_cmd(Cdp {
                                method: "Page.addScriptToEvaluateOnNewDocument",
                                params: json!({ "source": source }),
                            })
                            .await
                            .unwrap();
                    }
                    return Self { client, driver };
                }
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // `driver` drops during the unwind and ends the browser it started.
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

    /// Ends the session; chromedriver and Chrome end when the browser drops.
    pub(crate) async fn quit(self) {
        self.client.close().await.unwrap();
    }
}

/// A process as `ps` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Proc {
    pid: i32,
    pgid: i32,
    command: String,
}

/// Every process on the machine. Empty when `ps` fails, which leaves the reaper nothing to end.
fn list_processes() -> Vec<Proc> {
    std::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,pgid=,command="])
        .output()
        .map(|out| parse_ps(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or_default()
}

/// Splits `ps -o pid=,pgid=,command=` lines. Lines that do not parse are skipped.
fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let (id, rest) = line.trim_start().split_once(char::is_whitespace)?;
            let (group, command) = rest.trim_start().split_once(char::is_whitespace)?;
            Some(Proc {
                pid: id.parse().ok()?,
                pgid: group.parse().ok()?,
                command: command.trim_start().to_owned(),
            })
        })
        .collect()
}

/// The test pid a profile directory name (`<pid>-<n>`) belongs to.
fn dir_owner(name: &str) -> Option<u32> {
    let (pid, n) = name.split_once('-')?;
    n.parse::<u32>().ok()?;
    pid.parse().ok().filter(|&pid| pid > 0)
}

/// The test pid that owns the profile a Chrome command line names under `root`, if it names one.
fn profile_owner(root: &Path, command: &str) -> Option<u32> {
    let prefix = format!("--user-data-dir={}/", root.display());
    let rest = command.split_once(prefix.as_str())?.1;
    dir_owner(rest.split(['/', ' ']).next()?)
}

/// The process groups to end: those of Chrome processes whose profile under `root` belongs to a
/// test process that is no longer alive. Never `own_group`, and never a group below 2.
fn stale_groups(
    root: &Path,
    procs: &[Proc],
    own_group: i32,
    is_alive: impl Fn(u32) -> bool,
) -> Vec<i32> {
    let mut groups: Vec<i32> = procs
        .iter()
        .filter(|p| profile_owner(root, &p.command).is_some_and(|owner| !is_alive(owner)))
        .map(|p| p.pgid)
        .filter(|&group| group > 1 && group != own_group)
        .collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

/// The profile directories under `root` whose test process is no longer alive.
fn stale_dirs(root: &Path, names: &[String], is_alive: impl Fn(u32) -> bool) -> Vec<PathBuf> {
    names
        .iter()
        .filter(|name| dir_owner(name).is_some_and(|owner| !is_alive(owner)))
        .map(|name| root.join(name))
        .collect()
}

/// Whether a process with `pid` exists. EPERM means it exists under another user, so it is alive.
fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    !matches!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH))
}

/// Ends the Chrome groups and removes the profile directories a killed test run left behind.
/// Runs once per test process, before its first browser starts.
fn reap_stale() {
    let root = chrome_root();
    let own_group = getpgrp().as_raw();
    for group in stale_groups(&root, &list_processes(), own_group, pid_alive) {
        end_group(group, None);
    }
    let names: Vec<String> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    for dir in stale_dirs(&root, &names, pid_alive) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Runs [`reap_stale`] once per test process.
static REAP: Once = Once::new();

/// Ends the WebDriver session with a blocking `DELETE`. Failures are ignored: the group end follows.
fn delete_session(port: u16, session: &str) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let request = format!(
        "DELETE /session/{session} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_ok() {
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
    }
}

/// Ends every process in `group`: SIGTERM, then SIGKILL if [`GRACE`] was not enough. `driver` is
/// the group's leader when this process started it; reaping it stops a zombie from keeping the
/// group alive.
fn end_group(group: i32, mut driver: Option<&mut Child>) {
    let group = Pid::from_raw(group);
    let _ = killpg(group, Signal::SIGTERM);
    if !wait_for_group_exit(group, driver.as_deref_mut(), GRACE) {
        let _ = killpg(group, Signal::SIGKILL);
        wait_for_group_exit(group, driver, GRACE);
    }
}

/// Polls until `group` has no members, or `within` has passed. Returns whether it is empty.
fn wait_for_group_exit(group: Pid, mut driver: Option<&mut Child>, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if let Some(child) = driver.as_deref_mut() {
            let _ = child.try_wait();
        }
        if killpg(group, None).is_err() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
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

/// The processes in `group`, as `ps` lists them now.
fn group_members(group: i32) -> Vec<Proc> {
    list_processes()
        .into_iter()
        .filter(|p| p.pgid == group)
        .collect()
}

#[tokio::test]
async fn a_dropped_session_leaves_no_chrome_in_its_group() {
    let browser = Browser::launch().await;
    let group = browser.driver.group;
    let profile = browser.driver.profile.clone();
    browser.client.goto("about:blank").await.unwrap();
    // Chrome joined chromedriver's group and runs on this test's profile.
    let profile_arg = format!("--user-data-dir={}", profile.display());
    assert!(
        group_members(group)
            .iter()
            .any(|p| p.command.contains(&profile_arg)),
        "Chrome should run in chromedriver's process group"
    );
    // Dropped without quit(), as a panicking test drops it: the session is still open.
    drop(browser);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut left = group_members(group);
    while !left.is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        left = group_members(group);
    }
    assert_eq!(left, Vec::new(), "no process should outlive the browser");
    assert!(!profile.exists(), "the profile should be removed");
}

#[cfg(test)]
mod reaper {
    use super::*;

    fn proc(id: i32, group: i32, command: &str) -> Proc {
        Proc {
            pid: id,
            pgid: group,
            command: command.to_owned(),
        }
    }

    #[test]
    fn ps_lines_split_into_pid_group_and_command() {
        assert_eq!(
            parse_ps("  123   45 chrome --user-data-dir=/x y\n    7    7 launchd\nbad line\n"),
            vec![
                proc(123, 45, "chrome --user-data-dir=/x y"),
                proc(7, 7, "launchd"),
            ]
        );
    }

    #[test]
    fn a_dead_owners_chrome_group_is_reaped_and_a_live_owners_is_kept() {
        let root = Path::new("/tmp/t/fleet-e2e-chrome");
        let procs = [
            proc(
                10,
                500,
                &format!("chrome --user-data-dir={}/77-1 --type=gpu", root.display()),
            ),
            proc(
                11,
                600,
                &format!("chrome --user-data-dir={}/88-1", root.display()),
            ),
            proc(12, 600, "chromedriver --port=1"),
        ];
        let alive = |pid| pid == 77;
        assert_eq!(stale_groups(root, &procs, 1, alive), vec![600]);
        // Never the group the test process itself is in.
        assert_eq!(stale_groups(root, &procs, 600, alive), Vec::<i32>::new());
    }

    #[test]
    fn no_chrome_outside_the_root_is_ever_selected() {
        let root = Path::new("/tmp/t/fleet-e2e-chrome");
        let procs = [
            proc(
                20,
                700,
                "Google Chrome --user-data-dir=/Users/me/Library/Chrome",
            ),
            proc(
                21,
                701,
                "chrome --user-data-dir=/tmp/t/fleet-e2e-chrome-old/88-1",
            ),
            proc(
                22,
                702,
                "chrome --user-data-dir=/tmp/t/other/fleet-e2e-chrome/88-1",
            ),
            proc(23, 703, "chrome --headless=new"),
            proc(
                24,
                704,
                "chrome --user-data-dir=/tmp/t/fleet-e2e-chrome/88-1x",
            ),
            proc(
                25,
                705,
                "chrome --user-data-dir=/tmp/t/fleet-e2e-chrome/0-1",
            ),
        ];
        assert_eq!(stale_groups(root, &procs, 1, |_| false), Vec::<i32>::new());
    }

    #[test]
    fn only_dead_owners_profile_dirs_are_removed() {
        let root = Path::new("/tmp/t/fleet-e2e-chrome");
        let names: Vec<String> = ["77-1", "88-1", "88-2", "not-a-profile", "0-1"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(
            stale_dirs(root, &names, |pid| pid == 77),
            vec![root.join("88-1"), root.join("88-2")]
        );
    }
}
