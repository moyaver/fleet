// SPDX-License-Identifier: AGPL-3.0-only
//! Liveness and readiness (research 03 §11, research 06 §9.4).
//!
//! `/livez`: the process answers. `/readyz`: start-up finished, not draining, and Postgres
//! answers within a second. `fleet healthcheck` probes `/readyz` over loopback, because the
//! distroless image has no curl.

use std::{net::SocketAddr, time::Duration};

use anyhow::Context;
use axum::{Router, extract::State, http::StatusCode, routing::get};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::state::AppState;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/livez", get(|| async { "ok" }))
        .route("/readyz", get(ready))
}

async fn ready(State(state): State<AppState>) -> (StatusCode, &'static str) {
    if !state.is_ready() {
        return (StatusCode::SERVICE_UNAVAILABLE, "not ready");
    }
    match tokio::time::timeout(Duration::from_secs(1), state.store.ping()).await {
        Ok(Ok(())) => (StatusCode::OK, "ready"),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "database unavailable"),
    }
}

/// `fleet healthcheck`: GET /readyz on loopback; an error unless it answers 200 within 3 s.
pub(crate) async fn probe(listen: SocketAddr) -> anyhow::Result<()> {
    let target = if listen.ip().is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], listen.port()))
    } else {
        listen
    };
    let check = async {
        let mut sock = tokio::net::TcpStream::connect(target)
            .await
            .with_context(|| format!("connecting to {target}"))?;
        sock.write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut head = [0u8; 12];
        sock.read_exact(&mut head).await?;
        anyhow::ensure!(
            head.starts_with(b"HTTP/1.1 200"),
            "not ready: {}",
            String::from_utf8_lossy(&head)
        );
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(3), check)
        .await
        .context("the server did not answer within 3 s")?
}
