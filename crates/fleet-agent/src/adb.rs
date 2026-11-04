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
