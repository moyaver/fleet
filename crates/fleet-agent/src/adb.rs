// SPDX-License-Identifier: AGPL-3.0-only
//! A minimal adb *server protocol* client (research 02 §4): `host:track-devices` pushes the device
//! list on every change. The agent never starts, restarts or kills the adb server — spawning an
//! `adb` client of a different version can restart it and break Maestro's connections. If nothing
//! listens, it waits and tries again.

use std::time::Duration;

use fleet_protocol::event::AdbState;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// The adb server address: `ANDROID_ADB_SERVER_PORT` on localhost, default 5037.
pub(crate) fn server_addr() -> String {
    let port = std::env::var("ANDROID_ADB_SERVER_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(5037);
    format!("127.0.0.1:{port}")
}

/// Parses a device list payload: one `serial\tstate` per line.
pub(crate) fn parse(payload: &str) -> Vec<(String, AdbState)> {
    payload
        .lines()
        .filter_map(|l| {
            let (serial, state) = l.split_once('\t')?;
            let state = match state.trim() {
                "device" => AdbState::Device,
                "offline" => AdbState::Offline,
                "unauthorized" => AdbState::Unauthorized,
                _ => AdbState::Other,
            };
            Some((serial.trim().to_owned(), state))
        })
        .collect()
}

/// Sends the device list each time it changes, until the receiver is dropped.
pub(crate) async fn track(addr: String, tx: mpsc::Sender<Vec<(String, AdbState)>>) {
    loop {
        if let Err(e) = track_once(&addr, &tx).await {
            tracing::debug!(error = %e, "adb track-devices ended");
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn track_once(addr: &str, tx: &mpsc::Sender<Vec<(String, AdbState)>>) -> std::io::Result<()> {
    let mut s = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr)).await??;
    let request = "host:track-devices";
    s.write_all(format!("{:04x}{request}", request.len()).as_bytes())
        .await?;
    let mut status = [0u8; 4];
    s.read_exact(&mut status).await?;
    if &status != b"OKAY" {
        return Err(std::io::Error::other("adb refused track-devices"));
    }
    loop {
        let mut len = [0u8; 4];
        s.read_exact(&mut len).await?;
        let len = usize::from_str_radix(
            std::str::from_utf8(&len).map_err(std::io::Error::other)?,
            16,
        )
        .map_err(std::io::Error::other)?;
        let mut payload = vec![0u8; len];
        s.read_exact(&mut payload).await?;
        if tx
            .send(parse(&String::from_utf8_lossy(&payload)))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
}

/// The longest reply a shell command may send back. A device probe's is about 2 KiB; anything
/// longer is cut off, which keeps memory bounded (I10).
const MAX_SHELL_REPLY: u64 = 64 * 1024;
/// The longest `FAIL` message read in full; adb's messages are one line.
const MAX_FAIL_MESSAGE: usize = 4096;

/// Runs `command` on the device `serial` through the adb server: `host:transport:<serial>`, then
/// `shell:<command>` on the same connection, and returns what the command printed. The server
/// answers each request with `OKAY`, or `FAIL` and a message (for example an unknown serial),
/// which is an error here. Nothing is started or killed (see the module docs).
///
/// # Cancel safety
/// Not cancel-safe: dropping the future closes the connection. Bound it with a timeout.
pub(crate) async fn shell(addr: &str, serial: &str, command: &str) -> std::io::Result<Vec<u8>> {
    let mut s = TcpStream::connect(addr).await?;
    request(&mut s, &format!("host:transport:{serial}")).await?;
    request(&mut s, &format!("shell:{command}")).await?;
    let mut out = Vec::new();
    (&mut s).take(MAX_SHELL_REPLY).read_to_end(&mut out).await?;
    Ok(out)
}

/// Sends one request on the smart socket and reads its status.
async fn request(s: &mut TcpStream, body: &str) -> std::io::Result<()> {
    s.write_all(format!("{:04x}{body}", body.len()).as_bytes())
        .await?;
    let mut status = [0u8; 4];
    s.read_exact(&mut status).await?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let message = read_hex_prefixed(s).await?;
            Err(std::io::Error::other(message))
        }
        _ => Err(std::io::Error::other("adb replied with an unknown status")),
    }
}

/// Reads a four-hex-digit length and that many bytes of text (a `FAIL` message).
async fn read_hex_prefixed(s: &mut TcpStream) -> std::io::Result<String> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let len = usize::from_str_radix(
        std::str::from_utf8(&len).map_err(std::io::Error::other)?,
        16,
    )
    .map_err(std::io::Error::other)?;
    let mut text = vec![0u8; len.min(MAX_FAIL_MESSAGE)];
    s.read_exact(&mut text).await?;
    Ok(String::from_utf8_lossy(&text).into_owned())
}

/// A fake adb server for tests: it answers each connection as the real server does for one
/// request sequence, so the client's code runs against recorded bytes.
#[cfg(test)]
pub(crate) mod fake {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    /// Accepts connections forever. Each one must send `host:transport:<serial>` then
    /// `shell:<command>` (with `command` as given), and gets `reply` written back in full.
    /// Returns the address and how many connections were accepted.
    pub(crate) async fn device(
        serial: &'static str,
        command: &'static str,
        reply: &'static [u8],
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        devices(serial, vec![(command, reply)]).await
    }

    /// As [`device`], for several commands: each connection's `shell:` command selects its reply.
    /// A command not listed gets `FAIL`, as adb answers an unknown request.
    pub(crate) async fn devices(
        serial: &'static str,
        replies: Vec<(&'static str, &'static [u8])>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        devices_in_turn(
            serial,
            replies.into_iter().map(|(c, r)| (c, vec![r])).collect(),
        )
        .await
    }

    /// As [`devices`], where a command answers its replies in turn, and the last one again once
    /// they are used up: a device whose reading changes between two reads.
    pub(crate) async fn devices_in_turn(
        serial: &'static str,
        replies: Vec<(&'static str, Vec<&'static [u8]>)>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&count);
        let mut turn = vec![0usize; replies.len()];
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = read_request(&mut s).await;
                assert_eq!(body, format!("host:transport:{serial}"));
                s.write_all(b"OKAY").await.unwrap();
                let body = read_request(&mut s).await;
                match replies
                    .iter()
                    .position(|(c, _)| body == format!("shell:{c}"))
                {
                    Some(i) => {
                        let (_, answers) = &replies[i];
                        let n = turn[i].min(answers.len() - 1);
                        turn[i] += 1;
                        s.write_all(answers[n]).await.unwrap();
                    }
                    None => s.write_all(b"FAIL000funknown command").await.unwrap(),
                }
            }
        });
        (addr, count)
    }

    /// A server that accepts and never answers: the client must give up on its own.
    pub(crate) async fn silent() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });
        addr
    }

    async fn read_request(s: &mut tokio::net::TcpStream) -> String {
        let mut len = [0u8; 4];
        s.read_exact(&mut len).await.unwrap();
        let n = usize::from_str_radix(std::str::from_utf8(&len).unwrap(), 16).unwrap();
        let mut body = vec![0u8; n];
        s.read_exact(&mut body).await.unwrap();
        String::from_utf8(body).unwrap()
    }

    /// A fake server replying to one request sequence, step by step (see [`super::tests`]).
    pub(crate) async fn scripted(steps: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            for (expect, reply) in steps {
                assert_eq!(read_request(&mut s).await, expect);
                s.write_all(&reply).await.unwrap();
            }
        });
        addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_lists() {
        assert_eq!(
            parse(
                "emulator-5554\tdevice\n127.0.0.1:5555\toffline\nR58M\tunauthorized\nx\trecovery\n"
            ),
            vec![
                ("emulator-5554".into(), AdbState::Device),
                ("127.0.0.1:5555".into(), AdbState::Offline),
                ("R58M".into(), AdbState::Unauthorized),
                ("x".into(), AdbState::Other),
            ]
        );
        assert_eq!(parse(""), Vec::new());
    }

    /// The probe's real replies, captured from an emulator (fixtures/adb/android-37.1, see its
    /// README): the transport's `OKAY`, then the shell's `OKAY` and what it printed.
    #[tokio::test]
    async fn shell_returns_what_the_device_printed() {
        let captured: &[u8] = include_bytes!("../../../fixtures/adb/android-37.1/probe.raw");
        let addr = fake::scripted(vec![
            (
                "host:transport:emulator-5584".into(),
                captured[..4].to_vec(),
            ),
            (
                format!("shell:{}", crate::device::PROBE),
                captured[4..].to_vec(),
            ),
        ])
        .await;
        let out = shell(&addr, "emulator-5584", crate::device::PROBE)
            .await
            .unwrap();
        assert!(
            out.starts_with(b"boot=1\ncpu  "),
            "{:?}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(out, captured[8..]);
    }

    /// adb's own refusal, captured for an unknown serial, is an error carrying its message.
    #[tokio::test]
    async fn an_unknown_serial_is_an_error_with_adbs_message() {
        let refused: &[u8] =
            include_bytes!("../../../fixtures/adb/android-37.1/missing-device.raw");
        let addr = fake::scripted(vec![(
            "host:transport:emulator-5999".into(),
            refused.to_vec(),
        )])
        .await;
        let err = shell(&addr, "emulator-5999", crate::device::PROBE)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "device 'emulator-5999' not found");
    }

    /// Nothing listening (adb not running): an error, no panic, no start of a server.
    #[tokio::test]
    async fn no_adb_server_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        drop(listener);
        assert!(
            shell(&addr, "emulator-5584", crate::device::PROBE)
                .await
                .is_err()
        );
    }

    /// adb's own `host:track-devices` answer, captured from the same emulator: `OKAY`, the
    /// payload's length, then one `serial<TAB>state` line per device.
    #[test]
    fn the_captured_device_list_parses() {
        let captured: &[u8] =
            include_bytes!("../../../fixtures/adb/android-37.1/track-devices.raw");
        assert_eq!(&captured[..8], b"OKAY002a");
        assert_eq!(
            parse(&String::from_utf8_lossy(&captured[8..])),
            vec![
                ("emulator-5554".into(), AdbState::Device),
                ("emulator-5584".into(), AdbState::Device),
            ]
        );
    }

    #[tokio::test]
    async fn tracks_a_fake_adb_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 22];
            s.read_exact(&mut req).await.unwrap();
            assert_eq!(&req, b"0012host:track-devices");
            let one = "emulator-5554\tdevice\n";
            s.write_all(format!("OKAY{:04x}{one}0000", one.len()).as_bytes())
                .await
                .unwrap();
        });
        let (tx, mut rx) = mpsc::channel(4);
        tokio::spawn(track(addr, tx));
        assert_eq!(
            rx.recv().await.unwrap(),
            vec![("emulator-5554".to_owned(), AdbState::Device)]
        );
        assert_eq!(rx.recv().await.unwrap(), Vec::new(), "the device went away");
    }
}
